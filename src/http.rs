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

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::ambient;
use crate::ambient::sampling;
use crate::ble::worker::Snapshot;
use crate::config::WebMode;
use crate::logs::LogBuffer;

/// Routes. Named because the health path is also used by the container's own
/// healthcheck and by `--healthcheck`, which must agree with what is served.
pub const PATH_HEALTH: &str = "/healthz";
pub const PATH_STATE: &str = "/api/state";
pub const PATH_LOGS: &str = "/api/logs";
pub const PATH_ROOT: &str = "/";
/// The ambient calibration surface. Served only in `WEB_INTERFACE=ambient`.
pub const PATH_CALIBRATE: &str = "/calibrate";
/// The camera as an MJPEG stream, consumed by a plain `<img>`.
pub const PATH_PREVIEW: &str = "/preview";
/// The latest twenty zone colours and their outlines.
pub const PATH_AMBIENT: &str = "/api/ambient";
/// The stored calibration, read and written.
pub const PATH_CALIBRATION: &str = "/api/calibration";
/// Measure the white or black field that is on screen right now.
pub const PATH_CALIBRATION_FIELD: &str = "/api/calibration/field";
/// The camera's own controls: gain, exposure, white balance.
pub const PATH_CAMERA: &str = "/api/camera";

/// Boundary between the parts of the preview stream.
const PREVIEW_BOUNDARY: &str = "neewertv-frame";

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
    /// Present only while ambient capture is running, which is what decides
    /// whether the calibration routes exist at all.
    pub ambient: Option<ambient::Handle>,
}

/// Build the router.
///
/// The calibration routes are registered only when they can work. A route that
/// does not exist cannot be reached by a mistake in an authorisation check, and
/// that is the same reasoning that keeps the status page read-only.
pub fn router(state: AppState) -> Router {
    let calibration = state.ambient.is_some() && state.mode.permits_calibration();

    let mut router = Router::new()
        .route(PATH_HEALTH, get(healthz))
        .route(PATH_STATE, get(api_state))
        .route(PATH_LOGS, get(api_logs))
        .route(PATH_ROOT, get(index));

    if calibration {
        router = router
            .route(PATH_CALIBRATE, get(calibrate))
            .route(PATH_PREVIEW, get(preview))
            .route(PATH_AMBIENT, get(api_ambient))
            .route(
                PATH_CALIBRATION,
                get(api_calibration).post(api_calibration_save),
            )
            .route(PATH_CALIBRATION_FIELD, post(api_calibration_field))
            .route(PATH_CAMERA, get(api_camera).post(api_camera_set));
    }

    router.with_state(state)
}

/// The tab row shown on the status page when the calibration screen exists.
const TABS_MONITOR: &str = concat!(
    "<nav class=\"tabs\">",
    "<a class=\"on\" href=\"/\">Monitor</a>",
    "<a href=\"/calibrate\">Calibrate</a>",
    "</nav>"
);

/// The camera page's own copy of the row, with the selection reversed.
const TABS_CALIBRATE: &str = concat!(
    "<nav class=\"tabs\">",
    "<a href=\"/\">Monitor</a>",
    "<a class=\"on\" href=\"/calibrate\">Calibrate</a>",
    "</nav>"
);

/// Extra styling for the calibration page only. The camera needs a real surface,
/// not a 3 rem swatch, so none of this belongs on the status page.
const CALIBRATION_STYLE: &str = r"<style>
.cam{position:relative;border-radius:12px;overflow:hidden;background:#000;line-height:0}
.cam img{width:100%;display:block}
.cam canvas{position:absolute;inset:0;width:100%;height:100%;touch-action:none;cursor:crosshair}
.row{display:flex;gap:1rem;align-items:center;flex-wrap:wrap}
.row label{display:flex;gap:.4rem;align-items:center;font-size:.9rem;color:#4b5563}
button{font:inherit;padding:.42rem .9rem;border-radius:10px;border:1px solid #d7dbe0;background:#fff;cursor:pointer}
button.primary{background:#14161a;color:#fff;border-color:#14161a}
.swatches{display:grid;grid-template-columns:repeat(10,minmax(0,1fr));gap:.4rem}
.swatch.small{width:auto;height:2.4rem;border-radius:8px;font-size:.68rem;display:flex;align-items:flex-end;justify-content:center;color:#fff;text-shadow:0 0 3px #000}
.swatch[data-clipped=1]{outline:2px solid #ef4444;outline-offset:1px}
.hint{color:#6b7280;font-size:.85rem;margin:.9rem 0 0}
.controls{display:grid;gap:.35rem}
.ctrl{display:grid;grid-template-columns:minmax(8rem,13rem) 1fr auto;gap:.6rem;align-items:center;font-size:.85rem;color:#4b5563}
.ctrl em{font-style:normal;font-size:.72rem;color:#9aa3af}
.ctrl input[type=range]{width:100%}
.ctrl select{font:inherit;padding:.2rem .4rem;border-radius:8px;border:1px solid #d7dbe0;background:#fff;color:inherit}
@media (prefers-color-scheme:dark){
.row label,.hint,.ctrl{color:#9aa3af}
button{background:#1b1f25;color:#e7e9ee;border-color:#2b313a}
button.primary{background:#e7e9ee;color:#101215;border-color:#e7e9ee}
.ctrl select{background:#1b1f25;color:#e7e9ee;border-color:#2b313a}
}
</style>";

/// The page above the tab row.
const CALIBRATION_TOP: &str = r#"<main>
<header><div class="ident"><h1>Ambient calibration</h1>
<p class="meta">camera &rarr; twenty zone colours</p></div></header>
"#;

/// Everything below the tab row: the camera, the controls, and the overlay.
const CALIBRATION_BOTTOM: &str = r#"
<style>input[type=number]{font:inherit;padding:.3rem .5rem;border-radius:8px;border:1px solid #d7dbe0}
input[type=range]{width:8rem}
@media (prefers-color-scheme:dark){input[type=number]{background:#1b1f25;color:#e7e9ee;border-color:#2b313a}}</style>
<section class="card"><h2>Camera</h2>
<div class="cam"><img id="preview" src="/preview" alt="camera preview">
<canvas id="view" width="640" height="480"></canvas></div>
<p class="sub" id="status">starting&hellip;</p>
</section>
<section class="card"><h2>Parameters</h2>
<div class="row">
<label>Depth <input id="depth" type="range" min="3" max="30" value="10"><span id="depthv">10%</span></label>
<label><input id="linear" type="checkbox" checked> Linear light</label>
<button id="save" class="primary">Save calibration</button>
<button id="png">Save PNG</button>
</div>
<div class="row">
<label>Black level <input id="blacklevel" type="number" min="0" max="64" step="1" value="10"></label>
<label>Colour gain <input id="satgain" type="number" min="1" max="8" step="0.1" value="2"></label>
<label>Light gain <input id="lightgain" type="number" min="1" max="8" step="0.1" value="1.4"></label>
</div>
<p class="hint">Click the four corners of the screen in order: top-left, top-right,
bottom-right, bottom-left. Drag a handle to adjust it; right-click removes the last
corner. The swatches below are drawn from the same numbers the twenty segments are
sent, so the gains above show up there as you change them.</p>
</section>
<section class="card"><h2>Flat field</h2>
<div class="row">
<span class="hint" id="fields">nothing measured</span>
<button id="white">Measure white</button>
<button id="black">Measure black</button>
</div>
<p class="hint">Fill the screen with the white field and measure: every zone sees the same
colour, so whatever differs between them is the camera &mdash; lens shading, angle,
distance, the light in the room. Do the same with the black field to find the floor the
camera reports with nothing on screen. Either one can be measured on its own, and both
are saved into the calibration.</p>
</section>
<section class="card"><h2>Camera</h2>
<div id="camera" class="controls"><span class="hint">reading the camera&hellip;</span></div>
<p class="hint">Pin the camera's own gain and exposure here rather than turning the
television down. Left alone a camera re-exposes whenever the picture changes, so a white
field captured at one setting and a black field at another describe two different
cameras. Turn <em>Auto Exposure</em> off before setting <em>Exposure, Absolute</em>, and
set <em>Power Line Frequency</em> to the rate your lighting runs at or the sampling can
pick up banding. Everything here is stored with the calibration and applied when the
device opens.</p>
</section>
<section class="card"><h2>Segment colours</h2><div id="swatches" class="swatches"></div></section>
</main>
<script>
const view = document.getElementById('view');
const ctx = view.getContext('2d');
const img = document.getElementById('preview');
const statusLine = document.getElementById('status');
const swatchBox = document.getElementById('swatches');
const labels = ['TL', 'TR', 'BR', 'BL'];

let corners = [];
let latest = null;
let drag = -1;
let aspect = 1.7778;
let depth = 0.10;
let linear = true;
// Kept so the tuning can be sent back whole: the page does not edit the floor, but
// dropping it would silently reset it to the default on every save.
let render = { black_level: 10, saturation_gain: 2, lightness_gain: 1.4, lightness_floor: 8 };

const numberFrom = (id, fallback) => {
  const value = parseFloat(document.getElementById(id).value);
  return Number.isFinite(value) ? value : fallback;
};

const pointAt = (event) => {
  const box = view.getBoundingClientRect();
  return {
    x: (event.clientX - box.left) / box.width,
    y: (event.clientY - box.top) / box.height,
  };
};

function label(text, x, y) {
  ctx.font = '600 ' + Math.max(11, view.height / 42) + 'px system-ui, sans-serif';
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  ctx.lineWidth = 3;
  ctx.strokeStyle = 'rgba(0,0,0,0.85)';
  ctx.strokeText(text, x, y);
  ctx.fillStyle = '#fff';
  ctx.fillText(text, x, y);
}

function draw() {
  ctx.clearRect(0, 0, view.width, view.height);

  if (latest && latest.zones) {
    latest.zones.forEach((zone, index) => {
      ctx.beginPath();
      zone.outline.forEach(([u, v], position) => {
        const x = u * view.width, y = v * view.height;
        if (position === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
      });
      ctx.closePath();
      const c = zone.colour;
      ctx.fillStyle = 'rgba(' + c.r + ',' + c.g + ',' + c.b + ',0.55)';
      ctx.fill();
      ctx.strokeStyle = 'rgba(255,255,255,0.75)';
      ctx.lineWidth = 1.5;
      ctx.stroke();
      let cx = 0, cy = 0;
      zone.outline.forEach(([u, v]) => { cx += u; cy += v; });
      label(String(index + 1),
        (cx / zone.outline.length) * view.width,
        (cy / zone.outline.length) * view.height);
    });
  }

  if (corners.length >= 2) {
    ctx.beginPath();
    corners.forEach((corner, position) => {
      const x = corner.x * view.width, y = corner.y * view.height;
      if (position === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    });
    if (corners.length === 4) ctx.closePath();
    ctx.strokeStyle = 'rgba(90,220,140,0.95)';
    ctx.lineWidth = 2.5;
    ctx.stroke();
  }

  corners.forEach((corner, index) => {
    const x = corner.x * view.width, y = corner.y * view.height;
    const radius = Math.max(9, view.height / 55);
    ctx.beginPath();
    ctx.arc(x, y, radius, 0, Math.PI * 2);
    ctx.fillStyle = 'rgba(90,220,140,0.25)';
    ctx.fill();
    ctx.strokeStyle = '#5adc8c';
    ctx.lineWidth = 2;
    ctx.stroke();
    label(labels[index] || '', x, y - radius - radius * 0.85);
  });

  if (corners.length < 4) {
    ctx.font = '600 ' + Math.max(13, view.height / 32) + 'px system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.lineWidth = 4;
    ctx.strokeStyle = 'rgba(0,0,0,0.85)';
    ctx.strokeText('click ' + labels[corners.length], view.width / 2, view.height * 0.06);
    ctx.fillStyle = '#5adc8c';
    ctx.fillText('click ' + labels[corners.length], view.width / 2, view.height * 0.06);
  }
}

function renderSwatches() {
  if (!latest || !latest.zones) return;
  if (swatchBox.children.length !== latest.zones.length) {
    swatchBox.replaceChildren();
    latest.zones.forEach((_, index) => {
      const cell = document.createElement('div');
      cell.className = 'swatch small';
      const number = document.createElement('span');
      number.textContent = String(index + 1);
      cell.append(number);
      swatchBox.append(cell);
    });
  }
  latest.zones.forEach((zone, index) => {
    const cell = swatchBox.children[index];
    const c = zone.strip
      ? hsvToRgb(zone.strip.hue, zone.strip.saturation, zone.strip.lightness)
      : [zone.colour.r, zone.colour.g, zone.colour.b];
    cell.style.background = 'rgb(' + c.join(',') + ')';
    cell.dataset.clipped = zone.clipped ? '1' : '0';
  });
}

// What the firmware does with the (hue, saturation, lightness) triple it is sent
// is close enough to HSV for a preview, and it is the same triple, so the swatch
// shows what the strip will do rather than what the camera saw.
function hsvToRgb(hue, saturation, lightness) {
  const channel = (n) => {
    const k = (n + hue * 6) % 6;
    return lightness - lightness * saturation * Math.max(0, Math.min(k, 4 - k, 1));
  };
  return [channel(5), channel(3), channel(1)].map((v) => Math.round(v * 255));
}

// The camera samples faster than Bluetooth can carry, so the two rates are
// measured separately and shown side by side. Without that, a change looks like
// it worked on the swatches while the strip is still three frames behind.
const stripMeter = { at: 0, writes: 0, skipped: 0, rate: 0, dropped: 0 };

function meterStrip(strip) {
  if (!strip) return null;
  const now = performance.now();

  // A restart zeroes the counters; a negative delta is that, not a rate.
  if (stripMeter.at && strip.writes_sent >= stripMeter.writes && strip.frames_skipped >= stripMeter.skipped) {
    const seconds = (now - stripMeter.at) / 1000;
    if (seconds >= 0.8) {
      stripMeter.rate = (strip.writes_sent - stripMeter.writes) / seconds;
      stripMeter.dropped = (strip.frames_skipped - stripMeter.skipped) / seconds;
      stripMeter.at = now;
      stripMeter.writes = strip.writes_sent;
      stripMeter.skipped = strip.frames_skipped;
    }
    return stripMeter;
  }

  stripMeter.at = now;
  stripMeter.writes = strip.writes_sent;
  stripMeter.skipped = strip.frames_skipped;
  return stripMeter;
}

async function refresh() {
  try {
    const res = await fetch('/api/ambient');
    if (res.ok) {
      latest = await res.json();
      if (view.width !== latest.frame.width || view.height !== latest.frame.height) {
        view.width = latest.frame.width;
        view.height = latest.frame.height;
      }
      const bits = [latest.fps.toFixed(1) + ' fps sampled'];
      const strip = meterStrip(latest.strip);
      if (!latest.strip || !latest.strip.connected) {
        bits.push('strip not connected');
      } else if (!latest.strip.preview) {
        bits.push('strip not following \u2014 a command took it over');
      } else if (strip && strip.rate > 0) {
        bits.push(strip.rate.toFixed(1) + '/s to the strip');
      } else {
        bits.push('nothing reaching the strip');
      }
      if (!latest.calibrated) bits.push('not calibrated');
      if (latest.clipped_zones > 0) bits.push(latest.clipped_zones + ' over-exposed');
      if (strip && strip.dropped >= 1) {
        bits.push(Math.round(strip.dropped) + '/s dropped as stale');
      }
      statusLine.textContent = bits.join(' \u00b7 ');
    } else if (res.status === 503) {
      statusLine.textContent = 'waiting for the camera';
    }
  } catch (err) {
    statusLine.textContent = 'cannot reach the daemon: ' + err;
  }
  draw();
  renderSwatches();
  setTimeout(refresh, 120);
}

async function saveCalibration() {
  if (corners.length !== 4) {
    statusLine.textContent = 'click four corners first';
    return;
  }
  const res = await fetch('/api/calibration', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      aspect,
      depth,
      linear,
      corners: corners.map((c) => [c.x, c.y]),
      render: {
        black_level: numberFrom('blacklevel', render.black_level),
        saturation_gain: numberFrom('satgain', render.saturation_gain),
        lightness_gain: numberFrom('lightgain', render.lightness_gain),
        lightness_floor: render.lightness_floor,
      },
    }),
  });
  if (res.ok) {
    statusLine.textContent = 'calibration saved';
  } else {
    const body = await res.json().catch(() => ({}));
    statusLine.textContent = 'not saved: ' + (body.error || res.status);
  }
}

function updateFields(white, black) {
  const marks = [
    white ? 'white \u2713' : 'white \u2014',
    black ? 'black \u2713' : 'black \u2014',
  ];
  document.getElementById('fields').textContent = 'flat field: ' + marks.join(' \u00b7 ');
}

const cameraBox = document.getElementById('camera');

// The camera's own controls. A range input for integers, a select for menus and a
// checkbox for booleans: enough to pin gain and exposure, which is the point.
function renderControls(controls) {
  cameraBox.replaceChildren();
  if (!controls || !controls.length) {
    cameraBox.textContent = 'this camera reports no controls';
    return;
  }

  controls.forEach((control) => {
    const row = document.createElement('label');
    row.className = 'ctrl';

    const name = document.createElement('span');
    name.textContent = control.name;
    row.append(name);

    if (control.kind === 'menu' && control.items.length) {
      const select = document.createElement('select');
      select.disabled = control.read_only;
      control.items.forEach(([value, label]) => {
        const option = document.createElement('option');
        option.value = String(value);
        option.textContent = label;
        option.selected = value === control.value;
        select.append(option);
      });
      select.addEventListener('change', () => setControl(control.id, Number(select.value)));
      row.append(select, document.createElement('em'));
    } else if (control.kind === 'boolean') {
      const box = document.createElement('input');
      box.type = 'checkbox';
      box.checked = control.value !== 0;
      box.disabled = control.read_only;
      box.addEventListener('change', () => setControl(control.id, box.checked ? 1 : 0));
      row.append(box, document.createElement('em'));
    } else {
      const slider = document.createElement('input');
      slider.type = 'range';
      slider.min = String(control.minimum);
      slider.max = String(control.maximum);
      slider.step = String(control.step || 1);
      slider.value = String(control.value);
      slider.disabled = control.read_only;
      const readout = document.createElement('em');
      readout.textContent = String(control.value);
      // Live while dragging, written on release: a slider drag is one setting,
      // not two hundred.
      slider.addEventListener('input', () => { readout.textContent = slider.value; });
      slider.addEventListener('change', () => setControl(control.id, Number(slider.value)));
      row.append(slider, readout);
    }

    if (control.inactive) {
      const note = row.lastElementChild;
      note.textContent = 'inactive';
      note.title = 'the driver will not accept this until the control it depends on changes';
    }
    cameraBox.append(row);
  });
}

async function setControl(id, value) {
  const res = await fetch('/api/camera', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id, value }),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    statusLine.textContent = 'camera refused that: ' + (body.error || res.status);
    return;
  }
  statusLine.textContent = 'camera setting stored with the calibration';
  if (body.controls) renderControls(body.controls);
}

async function loadCamera() {
  try {
    const res = await fetch('/api/camera');
    if (!res.ok) return;
    const body = await res.json();
    renderControls(body.controls);
  } catch (err) {
    // No camera, no card. The rest of the page still works.
  }
}

async function measureField(field) {
  statusLine.textContent = 'measuring the ' + field + ' field\u2026';
  const res = await fetch('/api/calibration/field', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ field }),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    statusLine.textContent = body.error || ('measuring failed: ' + res.status);
    return;
  }
  let line = field + ' field measured';
  let detail = [];
  if (field === 'white' && typeof body.spread === 'number') {
    detail.push('zones differed by ' + Math.round(body.spread * 100) + '%');
  }
  if (field === 'black' && typeof body.level === 'number') {
    detail.push('floor ' + (body.level * 100).toFixed(2) + '%');
  }
  statusLine.textContent = body.warning || (detail.length ? line + ' \u00b7 ' + detail.join(' \u00b7 ') : line);
  loadSaved();
}

function savePng() {
  const out = document.createElement('canvas');
  out.width = view.width;
  out.height = view.height;
  const box = out.getContext('2d');
  if (img.naturalWidth) box.drawImage(img, 0, 0, out.width, out.height);
  box.drawImage(view, 0, 0);
  out.toBlob((blob) => {
    if (!blob) return;
    const link = document.createElement('a');
    link.href = URL.createObjectURL(blob);
    link.download = 'neewertv-calibration-' + Date.now() + '.png';
    link.click();
    setTimeout(() => URL.revokeObjectURL(link.href), 5000);
  }, 'image/png');
}

view.addEventListener('pointerdown', (event) => {
  event.preventDefault();
  const at = pointAt(event);
  for (let index = 0; index < corners.length; index += 1) {
    const dx = corners[index].x - at.x, dy = corners[index].y - at.y;
    if (Math.hypot(dx, dy) < 0.03) {
      drag = index;
      view.setPointerCapture(event.pointerId);
      return;
    }
  }
  if (corners.length < 4) {
    corners.push(at);
    draw();
  }
});

view.addEventListener('pointermove', (event) => {
  if (drag < 0) return;
  corners[drag] = pointAt(event);
  draw();
});

['pointerup', 'pointercancel'].forEach((name) => {
  view.addEventListener(name, () => { drag = -1; });
});

view.addEventListener('contextmenu', (event) => {
  event.preventDefault();
  corners.pop();
  draw();
});

document.getElementById('depth').addEventListener('input', (event) => {
  depth = parseInt(event.target.value, 10) / 100;
  document.getElementById('depthv').textContent = event.target.value + '%';
  draw();
});
document.getElementById('linear').addEventListener('change', (event) => {
  linear = event.target.checked;
});
document.getElementById('save').addEventListener('click', saveCalibration);
document.getElementById('png').addEventListener('click', savePng);
document.getElementById('white').addEventListener('click', () => measureField('white'));
document.getElementById('black').addEventListener('click', () => measureField('black'));

async function loadSaved() {
  try {
    const res = await fetch('/api/calibration');
    if (!res.ok) return;
    const saved = await res.json();
    corners = saved.corners.map(([x, y]) => ({ x, y }));
    aspect = saved.aspect;
    depth = saved.depth;
    linear = saved.linear;
    if (saved.render) render = Object.assign(render, saved.render);
    document.getElementById('blacklevel').value = String(render.black_level);
    document.getElementById('satgain').value = String(render.saturation_gain);
    document.getElementById('lightgain').value = String(render.lightness_gain);
    updateFields(Boolean(saved.white), Boolean(saved.black));
    const percent = Math.round(depth * 100);
    document.getElementById('depth').value = String(percent);
    document.getElementById('depthv').textContent = percent + '%';
    document.getElementById('linear').checked = linear;
  } catch (err) {
    // Nothing saved yet: the first run starts with no corners, as it should.
  }
}

loadSaved().then(loadCamera).then(refresh);
</script>
"#;

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

/// The camera as a stream a plain `<img>` can consume.
///
/// `multipart/x-mixed-replace` is the oldest trick in the book and still the
/// right one here: no WebSocket, no JavaScript decoder, and the browser drops the
/// connection the moment the tab stops looking at it, which is also how the
/// camera stops being read twice.
async fn preview(State(state): State<AppState>) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Holding this guard is what tells the rest of the daemon that somebody is
    // looking. The strip follows the camera for exactly as long as the stream is
    // open, and goes back to what it was doing when the stream is dropped — which
    // happens even if the browser disappears without closing cleanly.
    let guard = ambient.begin_viewing();

    let stream = futures::stream::unfold(
        (ambient.frames(), guard),
        |(mut frames, guard)| async move {
            loop {
                if frames.changed().await.is_err() {
                    return None;
                }
                let frame = {
                    let borrowed = frames.borrow_and_update();
                    (*borrowed).clone()
                };
                let Some(frame) = frame else {
                    continue;
                };

                let mut part = Vec::with_capacity(frame.preview.len() + 96);
                part.extend_from_slice(
                    format!(
                        "--{PREVIEW_BOUNDARY}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
                        frame.preview_type,
                        frame.preview.len()
                    )
                    .as_bytes(),
                );
                part.extend_from_slice(&frame.preview);
                part.extend_from_slice(b"\r\n");
                return Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(part)),
                    (frames, guard),
                ));
            }
        },
    );

    match Response::builder()
        .header(
            header::CONTENT_TYPE,
            format!("multipart/x-mixed-replace; boundary={PREVIEW_BOUNDARY}"),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
    {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The latest reading: twenty colours plus the outlines they were taken from.
///
/// The outlines are sent rather than the homography because the page only has to
/// draw them: one implementation of the geometry, in Rust, which cannot drift
/// from the one that did the sampling.
async fn api_ambient(State(state): State<AppState>) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(latest) = ambient.latest() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "the camera has not produced a frame yet" })),
        )
            .into_response();
    };

    // The swatches show what the strip will be sent, not what the camera saw:
    // they are the only way to see what the tuning is doing without walking over
    // to the television. `strip` is the segment entry as (lightness, hue,
    // saturation), normalised to 0–1 here so the page does not carry the wire
    // scale.
    let strip = ambient::render::entries(&latest.colours, latest.render);
    let zones: Vec<Value> = latest
        .outlines
        .iter()
        .zip(latest.colours.iter())
        .zip(latest.clipped.iter())
        .zip(strip.iter())
        .map(|(((outline, colour), clipped), entry)| {
            let (lightness, hue, saturation) = *entry;
            json!({
                "colour": { "r": colour[0], "g": colour[1], "b": colour[2] },
                "strip": {
                    "lightness": f32::from(lightness) / 100.0,
                    "hue": f32::from(hue) / 255.0,
                    "saturation": f32::from(saturation) / 100.0,
                },
                "clipped": clipped,
                "outline": outline
                    .iter()
                    .map(|point| json!([point[0], point[1]]))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    Json(json!({
        "calibrated": latest.calibrated,
        "frame": { "width": latest.frame.width, "height": latest.frame.height },
        "fps": latest.fps,
        "clipped_zones": latest.clipped_zones,
        // What the link is actually managing. The camera samples faster than
        // Bluetooth can carry, so these two numbers are the honest answer to
        // "is the strip behind the page" — and they are the reason to look
        // at the strip rather than at the swatches when judging a change.
        "strip": {
            "connected": state.snapshot.borrow().connected,
            "writes_sent": state.snapshot.borrow().writes_sent,
            "frames_skipped": state.snapshot.borrow().frames_skipped,
            // Whether the strip is being driven from here at all. False while a
            // user command outranks the preview, which is otherwise invisible:
            // the swatches keep moving and nothing reaches the light.
            "preview": state.snapshot.borrow().preview,
        },
        "zones": zones,
    }))
    .into_response()
}

/// The calibration that is currently on disk and in use.
async fn api_calibration(State(state): State<AppState>) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match ambient.calibration() {
        Some(calibration) => {
            Json(serde_json::to_value(&*calibration).unwrap_or(Value::Null)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// What the page sends back after four corners have been clicked.
#[derive(Debug, Deserialize)]
struct CalibrationRequest {
    aspect: f32,
    depth: f32,
    #[serde(default)]
    linear: Option<bool>,
    corners: Vec<[f32; 2]>,
    /// Present when the page is editing the render tuning.
    #[serde(default)]
    render: Option<ambient::Render>,
}

/// Which field the operator is holding on screen.
#[derive(Debug, Deserialize)]
struct FieldRequest {
    field: String,
}

/// One camera control, set from the page.
#[derive(Debug, Deserialize)]
struct CameraSetRequest {
    id: u32,
    value: i64,
}

/// Everything the camera can be told, with what it is currently told.
async fn api_camera(State(state): State<AppState>) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match ambient.camera_controls().await {
        Ok(controls) => Json(json!({ "controls": controls })).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// Pin one camera control, and remember it.
///
/// The value goes to the device first and only then into the calibration file: a
/// setting that the driver refused is not one the next start should try to apply.
async fn api_camera_set(
    State(state): State<AppState>,
    Json(request): Json<CameraSetRequest>,
) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if let Err(err) = ambient.set_camera_control(request.id, request.value).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response();
    }

    if let Some(stored) = ambient.calibration() {
        let mut calibration = (*stored).clone();
        match calibration
            .camera
            .iter_mut()
            .find(|setting| setting.id == request.id)
        {
            Some(setting) => setting.value = request.value,
            None => calibration.camera.push(ambient::CameraSetting {
                id: request.id,
                value: request.value,
            }),
        }
        if let Err(err) = ambient.save(calibration) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    }

    // Answer with the whole list again: setting one control can activate or
    // deactivate another, and the page should not have to guess which.
    match ambient.camera_controls().await {
        Ok(controls) => Json(json!({ "ok": true, "controls": controls })).into_response(),
        Err(err) => Json(json!({ "ok": true, "error": err.to_string() })).into_response(),
    }
}

/// Persist a calibration and put it to work immediately.
async fn api_calibration_save(
    State(state): State<AppState>,
    Json(request): Json<CalibrationRequest>,
) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // The frame size is taken from the camera, not from the request: it is the
    // size the corners were clicked against, and the page's idea of it comes
    // from the same place anyway.
    let Some(latest) = ambient.latest() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "the camera has not produced a frame yet" })),
        )
            .into_response();
    };

    let stored = ambient.calibration();

    if request.corners.len() != ambient::CORNERS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "expected {} corners, got {}",
                    ambient::CORNERS,
                    request.corners.len()
                )
            })),
        )
            .into_response();
    }

    let mut corners = [[0.0_f32; 2]; ambient::CORNERS];
    for (slot, point) in corners.iter_mut().zip(request.corners) {
        *slot = point;
    }

    let calibration = ambient::Calibration {
        version: ambient::CALIBRATION_VERSION,
        frame: latest.frame,
        aspect: request.aspect,
        depth: request.depth,
        linear: request.linear.unwrap_or(true),
        corners,
        // Whatever was already measured or tuned is carried over: saving the
        // corners again must not throw away a flat field or a tuning that took
        // several minutes of standing in front of the television to get right.
        render: request
            .render
            .or_else(|| stored.as_ref().map(|saved| saved.render))
            .unwrap_or_default(),
        black: stored.as_ref().and_then(|saved| saved.black),
        white: stored.as_ref().and_then(|saved| saved.white),
        camera: stored
            .as_ref()
            .map(|saved| saved.camera.clone())
            .unwrap_or_default(),
    };

    match ambient.save(calibration) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        // A refused calibration is the operator's problem, not the server's, so
        // the reason goes back to the page rather than into the log alone.
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// A black field brighter than this, in linear light, is not a black field.
const MAX_BLACK_FIELD: f32 = 0.25;

/// Above this a "black" field is being displayed as something other than black.
///
/// 1 % in linear light is well above any panel's own black and well above the
/// camera's noise floor, so reaching it means the screen is genuinely lit: a grey
/// black field, which is what a studio-range video does on a player that reads it
/// as full range.
const LIFTED_BLACK: f32 = 0.01;

/// Measure the flat field the operator is holding on screen.
///
/// White is what turns the camera into an instrument: every zone sees the same
/// colour, so every difference between them is the camera, the lens or the way it
/// is mounted. Black is the floor it reports with nothing on screen. Both belong
/// to where the camera sits, which is why they are stored with the calibration
/// rather than passed in at startup.
async fn api_calibration_field(
    State(state): State<AppState>,
    Json(request): Json<FieldRequest>,
) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(stored) = ambient.calibration() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "set the four corners before measuring a field" })),
        )
            .into_response();
    };

    let field = request.field.as_str();
    if field != "white" && field != "black" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "field must be \"white\" or \"black\"" })),
        )
            .into_response();
    }

    let measurement = match ambient.measure().await {
        Ok(measurement) => measurement,
        Err(err) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    let mut calibration = (*stored).clone();
    let brightest = measurement.brightest();

    if field == "white" {
        // A clipped sample is a value that was cut off before it was measured, so
        // the gain it would produce is unknown rather than wrong. Refusing is the
        // only honest answer.
        if measurement.worst_clip() > ambient::CLIP_SAMPLE_FRACTION {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "the white field is over-exposed: lower the television's \
                              brightness or move the camera back, then try again",
                })),
            )
                .into_response();
        }
        if measurement.brightest() < 4.0 * sampling::MIN_MEASURED_WHITE {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "the screen does not look white: put the white field up \
                              and wait for it to fill the screen",
                })),
            )
                .into_response();
        }
        calibration.white = Some(measurement.linear);
    } else {
        if measurement.brightest() > MAX_BLACK_FIELD {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "the screen does not look black: put the black field up \
                              and wait for it to fill the screen",
                })),
            )
                .into_response();
        }
        calibration.black = Some(measurement.linear);
    }
    let spread = calibration
        .correction()
        .map(|correction| correction.spread());

    // A black field that measured well above the panel's floor is the one thing
    // that quietly poisons the whole calibration: the correction then subtracts a
    // number that is mostly the screen being grey. It is still stored — refusing
    // would leave the operator with no black field at all — but it is said out
    // loud, because nothing else in the pipeline can tell.
    let lifted = (field == "black" && brightest > LIFTED_BLACK).then(|| {
        format!(
            "the black field measured {:.1}% bright, so the screen is not showing black — \
             check the player's RGB range before trusting this",
            brightest * 100.0
        )
    });

    match ambient.save(calibration) {
        Ok(()) => Json(json!({
            "ok": true,
            "field": field,
            "white": ambient
                .calibration()
                .is_some_and(|saved| saved.white.is_some()),
            "black": ambient
                .calibration()
                .is_some_and(|saved| saved.black.is_some()),
            // How far apart the zones read on the white field, as a fraction of
            // the mean. Large numbers mean the correction is doing real work; a
            // number near zero means the camera was already even.
            "spread": spread,
            // What the brightest zone saw, in linear light. The number the black
            // field exists to establish.
            "level": brightest,
            "warning": lifted,
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// The calibration page.
///
async fn calibrate(State(state): State<AppState>) -> Response {
    if state.ambient.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut page = format!(
        "<!doctype html><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{name} &mdash; calibration</title>{style}{extra}",
        name = escape_html(&state.device_name),
        style = PAGE_STYLE,
        extra = CALIBRATION_STYLE,
    );
    page.push_str(CALIBRATION_TOP);
    page.push_str(TABS_CALIBRATE);
    page.push_str(CALIBRATION_BOTTOM);
    Html(page).into_response()
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
.tabs a{background:#20252c;color:#c3c9d2}
.tabs a.on{background:#e7e9ee;color:#101215}
}
.tabs{display:flex;gap:.4rem;margin:0 0 1.15rem;flex-wrap:wrap}
.tabs a{padding:.4rem .9rem;border-radius:999px;text-decoration:none;font-size:.9rem;font-weight:500;color:#4b5563;background:#eef1f5}
.tabs a.on{background:#14161a;color:#fff}
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
         {tabs}\
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
        tabs = if state.ambient.is_some() && state.mode.permits_calibration() {
            TABS_MONITOR
        } else {
            ""
        },
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
            ambient: None,
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
