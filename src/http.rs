//! The optional web surface.
//!
//! Three modes, and the mode decides which routes *exist* rather than which
//! routes refuse: a route that is not mounted cannot be reached by a mistake in
//! an authorisation check added later.
//!
//! * `off` — no listener at all.
//! * `observe` — health, state, and a small page. Read-only.
//! * `full` — the above, plus the controls.
//!
//! There is no authentication, which is a deliberate choice documented in
//! `SECURITY.md`: this is a local page for a local light, on a network you
//! already trust.

use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::app::command::{self, SetPayload};
use crate::app::queue::Batch;
use crate::ble::worker::{Message, Snapshot};
use crate::config::WebMode;

/// Routes. Named because the health path is also used by the container's own
/// healthcheck and by `--healthcheck`, which must agree with what is served.
pub const PATH_HEALTH: &str = "/healthz";
pub const PATH_STATE: &str = "/api/state";
pub const PATH_SET: &str = "/api/set";
pub const PATH_ROOT: &str = "/";

/// Where the source lives.
///
/// Linked from the status page so that a page found open in a browser can
/// explain itself: who wrote it, which version is running, and where to go if it
/// is behaving oddly.
pub const REPO_URL: &str = "https://github.com/Hyper-Dragon/NeewerTV";

/// Everything the handlers need. Cheap to clone; the channels are handles.
#[derive(Clone)]
pub struct AppState {
    pub started: Instant,
    pub mode: WebMode,
    pub snapshot: watch::Receiver<Snapshot>,
    pub commands: mpsc::Sender<Message>,
    pub device_name: String,
    pub version: &'static str,
}

/// Build the router for the configured mode.
pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route(PATH_HEALTH, get(healthz))
        .route(PATH_STATE, get(api_state))
        .route(PATH_ROOT, get(index));

    // Controls exist only in `full`; `observe` answers 404, which is honest.
    if state.mode == WebMode::Full {
        router = router.route(PATH_SET, post(api_set));
    }

    router.with_state(state)
}

/// Liveness of the process, which is all Docker needs to decide whether to
/// restart the container.
///
/// Deliberately not a reachability check. A strip that is switched off at the
/// wall is unreachable and no restart will change that, so failing this probe
/// would put the container into a restart loop over a light that is simply not
/// plugged in. Reachability is reported where it can be acted on: on the MQTT
/// availability topic, where Home Assistant shows the light as unavailable.
async fn healthz(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.snapshot.borrow().clone();

    Json(json!({
        "status": "ok",
        "version": state.version,
        "uptime_seconds": state.started.elapsed().as_secs(),
        "link": {
            "connected": snapshot.connected,
            "address": snapshot.address,
        },
    }))
}

/// The daemon's own view of itself.
async fn api_state(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.snapshot.borrow().clone();

    Json(json!({
        "version": state.version,
        "uptime_s": state.started.elapsed().as_secs(),
        "mode": state.mode.to_string(),
        "link": {
            "connected": snapshot.connected,
            "address": snapshot.address,
            "last_error": snapshot.last_error,
        },
        "state": {
            "on": snapshot.on,
            "brightness": snapshot.brightness,
            "colour": snapshot.colour.map(|(red, green, blue)| json!({
                "r": red, "g": green, "b": blue
            })),
            "effect": snapshot.effect,
            "alert": snapshot.alert,
        },
        "traffic": {
            "commands_received": snapshot.commands_received,
            "commands_coalesced": snapshot.commands_coalesced,
            "writes_sent": snapshot.writes_sent,
        },
    }))
}

/// Minimal HTML escaping for the values interpolated into the status page.
///
/// The alert identifier arrives in an MQTT topic and the device name comes from
/// the environment, so neither is text this program wrote. The page is
/// unauthenticated by design (see `SECURITY.md`), which makes a rendering bug here
/// reachable by anyone who can publish to the broker — not the place to discover
/// that a topic name was `<script>`.
fn escape_html(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// A plain page, so opening the port in a browser is not a JSON dump.
async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.snapshot.borrow().clone();
    let link = if snapshot.connected {
        format!("connected to {}", snapshot.address)
    } else {
        "not connected".to_owned()
    };
    let effect = snapshot.effect.unwrap_or_else(|| "—".to_owned());
    let alert = snapshot.alert.unwrap_or_else(|| "—".to_owned());
    let brightness = match snapshot.brightness {
        Some(value) => format!("{value}/255"),
        None => "not known yet".to_owned(),
    };

    // Built as one line rather than inline with the rest of the markup: the `\`
    // continuations used above strip leading whitespace, which would weld the
    // separators onto the links.
    let footer = format!(
        "<p><a href=\"{state_path}\">JSON</a> &middot; \
         <a href=\"{repo_url}\">source</a> &middot; v{version}</p>",
        state_path = PATH_STATE,
        repo_url = REPO_URL,
        version = state.version,
    );

    format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <style>body{{font:14px system-ui;margin:2rem;max-width:40rem}}\
         dt{{font-weight:600}}dd{{margin:0 0 .4rem 0}}</style>\
         <h1>{name}</h1><dl>\
         <dt>Link</dt><dd>{link}</dd>\
         <dt>Power</dt><dd>{power}</dd>\
         <dt>Brightness</dt><dd>{brightness}</dd>\
         <dt>Effect</dt><dd>{effect}</dd>\
         <dt>Alert</dt><dd>{alert}</dd>\
         <dt>Commands</dt><dd>{received} received, {coalesced} coalesced, {writes} written</dd>\
         </dl>{footer}",
        name = escape_html(&state.device_name),
        footer = footer,
        link = escape_html(&link),
        power = if snapshot.on { "on" } else { "off" },
        brightness = escape_html(&brightness),
        effect = escape_html(&effect),
        alert = escape_html(&alert),
        received = snapshot.commands_received,
        coalesced = snapshot.commands_coalesced,
        writes = snapshot.writes_sent,
    )
}

/// Accept the same payload shape Home Assistant publishes.
async fn api_set(State(state): State<AppState>, Json(payload): Json<SetPayload>) -> Response {
    match command::resolve(&payload) {
        Ok(resolved) if resolved.is_empty() => StatusCode::NO_CONTENT.into_response(),

        Ok(resolved) => {
            let batch = Batch { commands: resolved };
            match state.commands.send(Message::Batch(batch)).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "the worker is not running" })),
                )
                    .into_response(),
            }
        }

        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_neutralises_everything_that_can_end_a_context() {
        assert_eq!(
            escape_html(r#"<script>alert("x")</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"
        );
        // The ampersand has to go first, or the escapes themselves get escaped.
        assert_eq!(escape_html("a & b"), "a &amp; b");
        assert_eq!(escape_html("it's"), "it&#39;s");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        // Nothing should grow or mangle, so the page still reads normally.
        assert_eq!(escape_html("TV Backlight"), "TV Backlight");
        assert_eq!(escape_html("—"), "—");
    }
}
