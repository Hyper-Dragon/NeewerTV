//! `neewertv` — bridge a NEEWER NS03-series TV backlight onto MQTT.
//!
//! The shape of the process, which everything else has to fit into:
//!
//! * configuration is read once from the environment; a bad value is fatal;
//! * exactly **one** task owns the Bluetooth link — the device accepts a single
//!   connection, so a second writer is a defect rather than a race;
//! * MQTT and HTTP only ever send messages and read a snapshot.
//!
//! Shutdown is a `CancellationToken`: stop accepting work, let the worker finish
//! its current write, publish `offline`, and close the link. A restart that
//! never disconnects leaves the strip believing it is still held.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use neewertv::ble::bluetooth::BluetoothLink;
use neewertv::ble::worker::{Message, Snapshot, Worker};
use neewertv::config::{self, Config, LogFormat};
use neewertv::{http, mqtt};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version plus the commit the image was built from, when the build said so.
///
/// Two containers of the same version are indistinguishable over a browser tab,
/// and "is this the image with the fix?" is a question that gets asked at the
/// worst possible moment. The GitHub workflows pass the commit in as
/// `NEEWERTV_BUILD`; a plain `cargo build` leaves it out rather than pretending to
/// a revision it does not know.
fn build_version() -> String {
    match option_env!("NEEWERTV_BUILD").map(str::trim) {
        Some(build) if !build.is_empty() => {
            let short: String = build.chars().take(7).collect();
            format!("{VERSION} (sha-{short})")
        }
        _ => VERSION.to_owned(),
    }
}

/// The flag the container's `HEALTHCHECK` invokes.
const FLAG_HEALTHCHECK: &str = "--healthcheck";

/// The status line a healthy probe must see.
const HTTP_OK: &str = "HTTP/1.1 200";

/// And the body marker it must see with it.
///
/// A status line alone is not enough: if something else on the host already
/// holds the port, the probe gets that server's 200 and reports this container
/// healthy while its own page never bound. Seen exactly that during an audit,
/// with an unrelated container answering `200` to a request meant for us. The
/// marker is the `"status":"ok"` this daemon's own `/healthz` returns.
const BODY_OK: &str = "\"status\":\"ok\"";

/// How often the alert clock is advanced. Alerts are measured in whole seconds,
/// so this is comfortably finer than the resolution that matters.
const TICK: Duration = Duration::from_millis(250);

fn main() -> anyhow::Result<()> {
    // Checked before the runtime starts so the probe costs a TCP connect, not a
    // whole tokio setup.
    if std::env::args().skip(1).any(|arg| arg == FLAG_HEALTHCHECK) {
        return probe_health();
    }

    run()
}

/// Ask our own `/healthz` whether the process is alive.
fn probe_health() -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};

    // With the page switched off there is no endpoint to ask, and probing a port
    // nobody bound would mark a deliberately-configured container unhealthy for
    // ever. The process being up is all this probe can honestly report.
    let mode = std::env::var(config::env::WEB_INTERFACE)
        .ok()
        .and_then(|raw| raw.trim().parse::<config::WebMode>().ok())
        .unwrap_or(config::default::WEB_INTERFACE);
    if !mode.is_enabled() {
        return Ok(());
    }

    let bind = std::env::var(config::env::WEB_BIND)
        .ok()
        .and_then(|raw| raw.parse::<SocketAddr>().ok())
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], config::default::WEB_PORT)));

    let ip = if bind.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        bind.ip()
    };
    let target = SocketAddr::new(ip, bind.port());

    let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(3))
        .with_context(|| format!("connecting to {target}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .context("setting the read timeout")?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        http::PATH_HEALTH
    );
    stream
        .write_all(request.as_bytes())
        .context("sending the health request")?;

    let mut status_line = [0_u8; 64];
    let mut filled = 0;
    // Read until the status line is complete. A single `read` is free to return
    // one byte, and comparing a partial `HTTP/1.1 2` against the expected prefix
    // would report a healthy process as dead — a probe that cries wolf is worse
    // than no probe.
    while filled < status_line.len() {
        let read = stream
            .read(&mut status_line[filled..])
            .context("reading the reply")?;
        if read == 0 {
            break;
        }
        filled += read;
        if status_line[..filled].windows(2).any(|pair| pair == b"\r\n") {
            break;
        }
    }
    let status_line = String::from_utf8_lossy(&status_line[..filled]);

    if !status_line.starts_with(HTTP_OK) {
        anyhow::bail!("health endpoint answered {:?}", status_line.trim_end());
    }

    // The status line alone is not proof that we are talking to ourselves. If
    // something else on the host already holds the port, this probe gets that
    // server's 200 and calls this container healthy while its own page never
    // bound — seen during an audit, with an unrelated container answering. Ask
    // for the marker only this daemon's `/healthz` returns.
    let mut body = [0_u8; 512];
    let mut seen = Vec::new();
    while seen.len() < body.len() {
        let read = stream.read(&mut body).context("reading the body")?;
        if read == 0 {
            break;
        }
        seen.extend_from_slice(&body[..read]);
        if seen
            .windows(BODY_OK.len())
            .any(|window| window == BODY_OK.as_bytes())
        {
            return Ok(());
        }
    }

    anyhow::bail!(
        "the health endpoint did not identify itself: {:?}",
        String::from_utf8_lossy(&seen).trim_end()
    )
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    let config = Config::from_env().context("reading configuration")?;
    let logs = init_tracing(&config).context("initialising logging")?;

    let started = Instant::now();
    let version = build_version();
    tracing::info!(
        version = %version,
        web = %config.web,
        broker = %config.mqtt.broker(),
        ble = %config.ble.name_match,
        "neewertv starting"
    );

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());

    // The worker owns the link; everything else holds a handle.
    let (command_tx, command_rx) = mpsc::channel::<Message>(32);
    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::default());

    let link = BluetoothLink::new(
        config.ble.name_match.clone(),
        config.ble.address.clone(),
        Duration::from_secs(config.ble.scan_seconds),
    );

    let mut worker = tokio::spawn(
        Worker::new(link)
            .with_keepalive(std::time::Duration::from_secs(config.ble.keepalive_seconds))
            .run(command_rx, snapshot_tx),
    );

    // The alert clock. Alerts are counted in whole seconds, so quarter-second
    // resolution is plenty and keeps the chatty part of the loop cheap.
    let ticker = {
        let commands = command_tx.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let mut interval = tokio::time::interval(TICK);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = start.elapsed().as_millis() as u64;
                        if commands.send(Message::Tick(now)).await.is_err() {
                            break;
                        }
                    }
                    () = shutdown.cancelled() => break,
                }
            }
        })
    };

    let mut broker = tokio::spawn(mqtt::run(
        config.clone(),
        snapshot_rx.clone(),
        command_tx.clone(),
        shutdown.clone(),
    ));

    let mut web = tokio::spawn(serve_web(
        config.clone(),
        snapshot_rx,
        started,
        shutdown.clone(),
        logs.clone(),
        version.clone(),
    ));

    // A failure in any one of these is fatal: a daemon that has lost its broker
    // or its worker is not a daemon, and pretending otherwise makes the failure
    // turn up later in a more confusing place.
    //
    // The exit code has to say so as well. Exiting 0 after a fatal component
    // failure tells Docker, compose and anything watching that the container did
    // its job and finished — `restart: on-failure` does not restart it, and
    // nobody is paged. Seen exactly that with a port clash: the log said ERROR and
    // the container said success.
    let mut failed: Option<&'static str> = None;
    tokio::select! {
        result = &mut worker => {
            tracing::error!(?result, "worker stopped");
            failed = Some("the worker");
        }
        result = &mut broker => {
            tracing::error!(?result, "MQTT stopped");
            failed = Some("MQTT");
        }
        result = &mut web => {
            tracing::error!(?result, "web server stopped");
            failed = Some("the web interface");
        }
        () = shutdown.cancelled() => tracing::info!("shutdown requested"),
    }

    let _ticker = ticker;

    // Whatever ended the loop, the rest is told to stop now. Without this a
    // failure in one task leaves the others running until the process happens to
    // exit, which hides the failure instead of acting on it.
    shutdown.cancel();

    // Ask the worker to release the strip and wait for it. Exiting without this
    // leaves BlueZ holding the single connection, and the device stops
    // advertising — which is how the *next* start fails with "no device found".
    let _ = command_tx.send(Message::Shutdown).await;
    if tokio::time::timeout(Duration::from_secs(10), &mut worker)
        .await
        .is_err()
    {
        tracing::warn!("the worker did not shut down within 10s");
    }

    // Then the broker, which publishes `offline` and disconnects on purpose. Held
    // to rather than dropped: a clean disconnect does not fire the last will, so
    // the goodbye only exists if this task gets the chance to send it.
    if tokio::time::timeout(Duration::from_secs(5), &mut broker)
        .await
        .is_err()
    {
        tracing::warn!("MQTT did not close within 5s");
    }

    tracing::info!("stopped");

    match failed {
        Some(component) => Err(anyhow::anyhow!("{component} stopped unexpectedly")),
        None => Ok(()),
    }
}

async fn serve_web(
    config: Config,
    snapshot: watch::Receiver<Snapshot>,
    started: Instant,
    shutdown: CancellationToken,
    logs: neewertv::logs::LogBuffer,
    version: String,
) -> anyhow::Result<()> {
    if !config.web.is_enabled() {
        tracing::info!("web interface disabled; MQTT only");
        // Stay alive with the rest of the process rather than returning, which
        // would look like a crash to the select above.
        shutdown.cancelled().await;
        return Ok(());
    }

    // A busy port must not cost the user their light. The page is how you inspect
    // this daemon; MQTT is what it is for, and a backlight that refuses to run
    // because something else on the host already has 8080 is a worse outcome than
    // a daemon you can only see through `docker logs`. The container will report
    // unhealthy (the healthcheck probes this port), which is the signal that it
    // happened — visible without the light going dark.
    let listener = match tokio::net::TcpListener::bind(config.web_bind).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(
                addr = %config.web_bind,
                %err,
                "the web interface could not bind; continuing with MQTT only"
            );
            shutdown.cancelled().await;
            return Ok(());
        }
    };
    tracing::info!(addr = %config.web_bind, mode = %config.web, "web interface listening");

    let app = http::router(http::AppState {
        started,
        mode: config.web,
        snapshot,
        logs,
        device_id: config.device.id.clone(),
        device_name: config.device.name.clone(),
        configured_address: config.ble.address.clone(),
        version,
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("serving the web interface")
}

/// Install the logger, and hand back the buffer the status page reads.
///
/// ANSI colour is off. Escape sequences are noise in the page's `<pre>`, and a log
/// line pasted out of `docker logs` into a bug report is more useful without them
/// too — which is how most of them arrive.
fn init_tracing(config: &Config) -> anyhow::Result<neewertv::logs::LogBuffer> {
    let filter = EnvFilter::try_new(&config.log_level)
        .with_context(|| format!("LOG_LEVEL={:?} is not a valid directive", config.log_level))?;

    let buffer = neewertv::logs::LogBuffer::new();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(buffer.clone());

    match config.log_format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }

    Ok(buffer)
}

/// Ask the process to stop on `SIGINT` or `SIGTERM`.
fn spawn_signal_handler(shutdown: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            match signal(SignalKind::terminate()) {
                Ok(mut term) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "cannot listen for SIGTERM; handling SIGINT only");
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }

        shutdown.cancel();
    });
}
