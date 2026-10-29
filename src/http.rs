//! The optional web surface: a status page and nothing else.
//!
//! Two modes, and the mode decides whether there is a listener at all.
//!
//! There is deliberately **no control surface**. Driving the strip over HTTP
//! meant maintaining a second control path with its own precedence rules, and it
//! could not do anything MQTT cannot — so the page reports what the daemon
//! believes, tails the log, and changes nothing. The only thing that moves the
//! light is MQTT.
//!
//! There is also no authentication, which is a deliberate choice documented in
//! `SECURITY.md`: this is a page for a light, on a network you already trust. It
//! being read-only is what makes that a small promise rather than a large one.

use std::time::Instant;

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::ble::worker::Snapshot;
use crate::config::WebMode;
use crate::logs::LogBuffer;

/// Routes. Named because the health path is also used by the container's own
/// healthcheck and by `--healthcheck`, which must agree with what is served.
pub const PATH_HEALTH: &str = "/healthz";
pub const PATH_STATE: &str = "/api/state";
pub const PATH_LOGS: &str = "/api/logs";
pub const PATH_ROOT: &str = "/";

/// Where the source lives.
///
/// Linked from the status page so that a page found open in a browser can
/// explain itself: who wrote it, which version is running, and where to go if it
/// is behaving oddly.
pub const REPO_URL: &str = "https://github.com/Hyper-Dragon/NeewerTV";

/// Where the wire protocol is written down.
///
/// Linked from the page because the page is where someone ends up when the
/// strip does something unexplained, and the answer is usually in here.
pub const PROTOCOL_URL: &str =
    "https://github.com/Hyper-Dragon/NeewerTV/blob/main/docs/PROTOCOL.md";

/// The GitHub mark, inlined.
///
/// Not a hotlinked image and not an icon font: the page has to render on a box
/// with no route to the internet. Sized in `em` and filled with `currentColor`
/// so it takes the footer's colour in either theme.
const GITHUB_MARK: &str = concat!(
    "<svg class=\"gh\" viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" ",
    "aria-hidden=\"true\" focusable=\"false\"><path d=\"M8 0C3.58 0 0 3.58 0 8c0 ",
    "3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94",
    "-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 ",
    "2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02",
    ".08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 ",
    "2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 ",
    "1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z\"/></svg>",
);

/// Everything the handlers need. Cheap to clone; the channels and the log
/// buffer are handles.
#[derive(Clone)]
pub struct AppState {
    pub started: Instant,
    pub mode: WebMode,
    pub snapshot: watch::Receiver<Snapshot>,
    pub logs: LogBuffer,
    pub device_id: String,
    pub device_name: String,
    /// The address from the environment, used only until the link comes up.
    ///
    /// Two strips on one network produce two pages that otherwise look identical,
    /// so the address belongs on the page before anyone has to guess.
    pub configured_address: Option<String>,
    pub version: String,
}

/// Build the router.
///
/// Nothing here can change the strip, so there is no mode-dependent routing left
/// to get wrong.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route(PATH_HEALTH, get(healthz))
        .route(PATH_STATE, get(api_state))
        .route(PATH_LOGS, get(api_logs))
        .route(PATH_ROOT, get(index))
        .with_state(state)
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
        "device": {
            "id": state.device_id,
            "name": state.device_name,
        },
        "link": {
            "connected": snapshot.connected,
            "address": snapshot.address,
            "configured_address": state.configured_address,
            "drops": snapshot.drops,
            "rssi": snapshot.rssi,
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

/// The tail of the log, newest last.
async fn api_logs(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "lines": state.logs.snapshot() }))
}

/// Minimal HTML escaping for the values interpolated into the page.
///
/// The alert identifier arrives in an MQTT topic and the device name comes from
/// the environment, so neither is text this program wrote.
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

/// Inline styling, deliberately not a framework and not a CDN: this page has to
/// work on a box with no internet access, and it should look like something you
/// would not mind leaving open on a tablet.
///
/// Light and dark are both handled through `prefers-color-scheme`, because a page
/// about a light in a dark room should not be the brightest thing in it.
const PAGE_STYLE: &str = r"<style>
:root{color-scheme:light dark}
*{box-sizing:border-box}
body{margin:0;padding:2rem 1.25rem;font:15px/1.5 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;background:#f6f7f9;color:#14161a}
main{max-width:52rem;margin:0 auto}
header{display:flex;align-items:center;justify-content:space-between;gap:1rem;flex-wrap:wrap;margin-bottom:1.4rem}
h1{font-size:1.3rem;margin:0;letter-spacing:-.015em}
h2{font-size:.75rem;text-transform:uppercase;letter-spacing:.09em;color:#6b7280;margin:0 0 .9rem}
.card{background:#fff;border:1px solid #e3e6ea;border-radius:14px;padding:1.25rem;margin-bottom:1rem}
.hero{display:flex;align-items:center;gap:1.25rem}
.swatch{width:3.5rem;height:3.5rem;border-radius:12px;flex:none;background:#e5e7eb;border:1px solid rgba(0,0,0,.12)}
.big{font-size:1.55rem;font-weight:650;letter-spacing:-.02em;margin:0;text-transform:capitalize}
.big.off{color:#6b7280}
.sub{color:#6b7280;margin:.15rem 0 0;font-size:.9rem}
.ident{min-width:0}
.meta{margin:.2rem 0 0;font:12.5px/1.4 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;color:#6b7280}
.pill{display:inline-flex;align-items:center;gap:.45rem;font-size:.85rem;padding:.32rem .75rem;border-radius:999px;background:#eef1f5;color:#374151;white-space:nowrap}
.pill::before{content:'';width:.5rem;height:.5rem;border-radius:50%;background:#9aa3af;flex:none}
.pill[data-state=up]::before{background:#22c55e}
.pill[data-state=down]::before{background:#ef4444}
dl.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(9rem,1fr));gap:1.1rem;margin:0}
dl.grid>div{min-width:0}
dt{font-size:.72rem;text-transform:uppercase;letter-spacing:.08em;color:#6b7280;margin:0 0 .25rem}
dd{margin:0;font-variant-numeric:tabular-nums;overflow-wrap:anywhere}
pre#logs{margin:0;padding:.9rem;border-radius:10px;background:#0f1216;color:#c9d1d9;
  font:12.5px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
  max-height:22rem;overflow:auto;white-space:pre-wrap;overflow-wrap:anywhere}
.logline{display:block}
.logline.warn{color:#f0b429}
.logline.error{color:#f87171}
footer{margin-top:1.4rem;font-size:.85rem;color:#6b7280;display:flex;gap:.5rem;flex-wrap:wrap;align-items:center}
a{color:inherit}
footer a{display:inline-flex;align-items:center;gap:.4rem;text-decoration:none}
footer a:hover span{text-decoration:underline}
.gh{width:1em;height:1em;fill:currentColor;flex:none;opacity:.9}
.credit{font-weight:500;color:#4b5563}
@media (prefers-color-scheme:dark){
body{background:#101215;color:#e7e9ee}
.card{background:#171a1f;border-color:#262b33}
h2,dt,.sub,footer,.meta{color:#9aa3af}
.credit{color:#c3c9d2}
.pill{background:#20252c;color:#c3c9d2}
.swatch{border-color:rgba(255,255,255,.14)}
pre#logs{background:#0b0d10;border:1px solid #23272e}
}
</style>";

/// Page behaviour: refresh the readout and the log tail. Nothing writes.
const PAGE_SCRIPT: &str = r#"<script>
const node = (id) => document.getElementById(id);
const set = (id, value) => {
  const target = node(id);
  if (target) target.textContent = value;
};
const refreshState = async () => {
  const res = await fetch('/api/state');
  if (!res.ok) return;
  const s = await res.json();

  const pill = node('s-pill');
  if (pill) {
    const up = s.link.connected;
    pill.dataset.state = up ? 'up' : 'down';
    pill.textContent = up ? 'connected' : 'no link';
    pill.title = up ? s.link.address : (s.link.last_error || 'the strip is not reachable');
  }

  const power = node('s-power');
  if (power) {
    power.textContent = s.state.on ? 'on' : 'off';
    power.className = s.state.on ? 'big' : 'big off';
  }

  const known = s.state.brightness !== null;
  set('s-brightness', known ? s.state.brightness + ' / 255' : 'not known yet');

  // Which strip this is. The live address is empty until the link comes up, so
  // fall back to the configured one rather than reporting nothing at all.
  set('s-ident',
    (s.link.address || s.link.configured_address || 'address not known yet') +
    ' · ' + s.device.id);

  const swatch = node('s-swatch');
  if (swatch) {
    swatch.style.background = s.state.colour
      ? 'rgb(' + s.state.colour.r + ',' + s.state.colour.g + ',' + s.state.colour.b + ')'
      : '';
  }

  set('s-sub', (known ? 'brightness ' + s.state.brightness + '/255' : 'brightness unknown') +
    ' · ' + (s.state.effect || 'no effect'));
  set('s-effect', s.state.effect || '—');
  set('s-alert', s.state.alert || '—');
  set('s-drops', s.link.drops === 0 ? 'none' : s.link.drops + ' since start');
  set('s-rssi', s.link.rssi === null ? '—' : s.link.rssi + ' dBm');
  set('s-commands', s.traffic.commands_received + ' in, ' +
    s.traffic.commands_coalesced + ' coalesced, ' + s.traffic.writes_sent + ' written');
};
const refreshLogs = async () => {
  const res = await fetch('/api/logs');
  if (!res.ok) return;
  const { lines } = await res.json();
  const box = node('logs');
  if (!box) return;
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  box.replaceChildren(...lines.map((line) => {
    const span = document.createElement('span');
    span.className = 'logline' + (line.includes(' WARN ') ? ' warn'
      : line.includes(' ERROR ') ? ' error' : '');
    span.textContent = line;
    return span;
  }));
  if (atBottom) box.scrollTop = box.scrollHeight;
};
refreshState();
refreshLogs();
setInterval(refreshState, 3000);
setInterval(refreshLogs, 4000);
</script>"#;

/// A status page: what the daemon thinks, and the tail of the log.
async fn index(State(state): State<AppState>) -> Html<String> {
    let snapshot = state.snapshot.borrow().clone();

    let link_up = snapshot.connected;
    let link_label = if link_up { "connected" } else { "no link" };
    let link_hint = if link_up {
        snapshot.address.clone()
    } else {
        snapshot
            .last_error
            .clone()
            .unwrap_or_else(|| "the strip is not reachable".to_owned())
    };

    // Which strip this page is about. The name is what the operator chose, so two
    // containers can share it; the address and the device id are what tell them
    // apart, and the address is unknown until the link comes up.
    let identity = {
        let address = if snapshot.address.is_empty() {
            state.configured_address.clone()
        } else {
            Some(snapshot.address.clone())
        };
        match address {
            Some(address) => format!(
                "{} &middot; {}",
                escape_html(&address),
                escape_html(&state.device_id)
            ),
            None => format!(
                "address not known yet &middot; {}",
                escape_html(&state.device_id)
            ),
        }
    };

    let brightness = match snapshot.brightness {
        Some(value) => format!("{value} / 255"),
        None => "not known yet".to_owned(),
    };
    let effect = snapshot.effect.clone().unwrap_or_else(|| "—".to_owned());
    let alert = snapshot.alert.clone().unwrap_or_else(|| "—".to_owned());

    let subtitle = format!(
        "brightness {} &middot; {}",
        match snapshot.brightness {
            Some(value) => format!("{value}/255"),
            None => "unknown".to_owned(),
        },
        escape_html(snapshot.effect.as_deref().unwrap_or("no effect")),
    );

    let swatch = match snapshot.colour {
        Some((red, green, blue)) => format!("background:rgb({red},{green},{blue})"),
        None => String::new(),
    };

    // Rendered server-side so the tail is there before any script runs, and so a
    // browser with scripting off still sees something useful.
    let log_tail: String = state
        .logs
        .snapshot()
        .iter()
        .map(|line| format!("<span class=\"logline\">{}</span>", escape_html(line)))
        .collect();

    let footer = format!(
        "<a href=\"{state_path}\">JSON</a><span>&middot;</span>\
         <a href=\"{logs_path}\">log</a><span>&middot;</span>\
         <a href=\"{protocol_url}\">protocol</a><span>&middot;</span>\
         <a class=\"credit\" href=\"{repo_url}\">{mark}<span>Hyper-Dragon</span></a>\
         <span>&middot;</span><span>v{version}</span>",
        state_path = PATH_STATE,
        logs_path = PATH_LOGS,
        protocol_url = PROTOCOL_URL,
        repo_url = REPO_URL,
        mark = GITHUB_MARK,
        version = state.version,
    );

    Html(format!(
        "<!doctype html><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{name}</title>{style}\
         <main>\
         <header><div class=\"ident\"><h1>{name}</h1><p class=\"meta\" id=\"s-ident\">{identity}</p></div>\
         <span class=\"pill\" id=\"s-pill\" data-state=\"{link_state}\" title=\"{link_hint}\">{link_label}</span>\
         </header>\
         <section class=\"card hero\">\
         <div class=\"swatch\" id=\"s-swatch\" style=\"{swatch}\"></div>\
         <div><p class=\"big\" id=\"s-power\">{power}</p>\
         <p class=\"sub\" id=\"s-sub\">{subtitle}</p></div>\
         </section>\
         <section class=\"card\"><h2>Status</h2><dl class=\"grid\">\
         <div><dt>Brightness</dt><dd id=\"s-brightness\">{brightness}</dd></div>\
         <div><dt>Effect</dt><dd id=\"s-effect\">{effect}</dd></div>\
         <div><dt>Alert</dt><dd id=\"s-alert\">{alert}</dd></div>\
         <div><dt>Link drops</dt><dd id=\"s-drops\">{drops}</dd></div>\
         <div><dt>Signal</dt><dd id=\"s-rssi\">{rssi}</dd></div>\
         <div><dt>Commands</dt><dd id=\"s-commands\">{received} in, {coalesced} coalesced, {writes} written</dd></div>\
         </dl></section>\
         <section class=\"card\"><h2>Log</h2><pre id=\"logs\">{log_tail}</pre></section>\
         <footer>{footer}</footer>\
         </main>\
         {script}",
        name = escape_html(&state.device_name),
        style = PAGE_STYLE,
        footer = footer,
        script = PAGE_SCRIPT,
        link_state = if link_up { "up" } else { "down" },
        link_label = link_label,
        link_hint = escape_html(&link_hint),
        identity = identity,
        swatch = swatch,
        power = if snapshot.on { "on" } else { "off" },
        subtitle = subtitle,
        brightness = escape_html(&brightness),
        effect = escape_html(&effect),
        alert = escape_html(&alert),
        drops = if snapshot.drops == 0 {
            "none".to_owned()
        } else {
            format!("{} since start", snapshot.drops)
        },
        rssi = match snapshot.rssi {
            Some(dbm) => format!("{dbm} dBm"),
            None => "—".to_owned(),
        },
        received = snapshot.commands_received,
        coalesced = snapshot.commands_coalesced,
        writes = snapshot.writes_sent,
        log_tail = log_tail,
    ))
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

    #[test]
    fn the_page_never_writes_to_the_strip() {
        // There is no control surface any more, and the point of removing it was
        // to stop maintaining a second way to change the light. If a script ever
        // posts to an endpoint, or the markup grows a control, this says so.
        for forbidden in ["POST", "method:", "/api/set", "/api/alert"] {
            assert!(
                !PAGE_SCRIPT.contains(forbidden),
                "the page script mentions {forbidden:?}"
            );
        }
        assert!(!PAGE_STYLE.contains("button"));
    }

    #[test]
    fn the_footer_points_at_the_protocol_and_the_author() {
        // The page is where someone lands when the strip misbehaves, so the
        // protocol notes have to be reachable from it — and the mark has to be
        // inline, because this page must render on a box with no internet.
        assert!(
            PROTOCOL_URL.starts_with(REPO_URL),
            "the notes live in the repo"
        );
        assert!(PROTOCOL_URL.ends_with("docs/PROTOCOL.md"));
        assert!(GITHUB_MARK.starts_with("<svg"), "inlined, not hotlinked");
        assert!(!GITHUB_MARK.contains("http"), "no external reference");
        assert!(
            PAGE_STYLE.contains(".gh{width:1em;height:1em;fill:currentColor"),
            "the mark takes the footer's colour in either theme"
        );
    }

    /// A page for one particular strip.
    async fn page_for(device_id: &str, address: &str, configured: Option<&str>) -> String {
        let (_, snapshot) = watch::channel(Snapshot {
            connected: true,
            address: address.to_owned(),
            ..Snapshot::default()
        });

        let state = AppState {
            started: Instant::now(),
            mode: WebMode::Observe,
            snapshot,
            logs: LogBuffer::new(),
            device_id: device_id.to_owned(),
            device_name: "TV Backlight".to_owned(),
            configured_address: configured.map(str::to_owned),
            version: "0.0.0".to_owned(),
        };

        index(State(state)).await.0
    }

    #[tokio::test]
    async fn two_strips_render_pages_that_can_be_told_apart() {
        // Same name on both containers is entirely likely — it is whatever the
        // operator typed. The address and the device id are what distinguish
        // them, so they have to be on the page itself.
        let first = page_for("neewer_ns03", "26:03:09:30:01:09", None).await;
        let second = page_for("neewer_ns03_b", "26:03:09:30:01:0A", None).await;

        assert!(first.contains("26:03:09:30:01:09"));
        assert!(second.contains("26:03:09:30:01:0A"));
        assert_ne!(first, second, "two strips must not render the same page");
    }

    #[tokio::test]
    async fn the_page_names_the_configured_address_before_the_link_is_up() {
        // Otherwise the page is anonymous exactly when someone is looking at it
        // to work out which strip failed to connect.
        let page = page_for("neewer_ns03", "", Some("26:03:09:30:01:09")).await;

        assert!(page.contains("26:03:09:30:01:09"));
        assert!(page.contains("neewer_ns03"));
        assert!(
            PAGE_SCRIPT.contains("configured_address"),
            "the refresh has to keep the identity line rather than blanking it"
        );
    }
}
