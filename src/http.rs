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
/// Set the camera up from the white field, then measure it.
pub const PATH_CALIBRATION_AUTO: &str = "/api/calibration/auto";
/// The camera's own controls: gain, exposure, white balance.
pub const PATH_CAMERA: &str = "/api/camera";
/// Turn every correction off, to tell a bad calibration from a camera that
/// cannot see.
pub const PATH_AMBIENT_BYPASS: &str = "/api/ambient/bypass";
/// Leave the strip following the camera, or stop.
pub const PATH_AMBIENT_MODE: &str = "/api/ambient/mode";
/// Everything needed to explain a calibration from somewhere else.
pub const PATH_DIAGNOSTIC: &str = "/api/diagnostic";
/// Start or stop capturing frames into that bundle.
pub const PATH_DIAGNOSTIC_RECORD: &str = "/api/diagnostic/record";

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
            .route(PATH_CALIBRATION_AUTO, post(api_calibration_auto))
            .route(PATH_CAMERA, get(api_camera).post(api_camera_set))
            .route(PATH_AMBIENT_BYPASS, post(api_ambient_bypass))
            .route(PATH_AMBIENT_MODE, post(api_ambient_mode))
            .route(PATH_DIAGNOSTIC, get(api_diagnostic))
            .route(PATH_DIAGNOSTIC_RECORD, post(api_diagnostic_record));
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
.tune{display:grid;grid-template-columns:max-content 1fr;gap:.4rem .9rem;margin:.9rem 0 0;font-size:.85rem}
.tune dt{color:#4b5563;font-weight:600;white-space:nowrap}
.tune dt span{font-weight:400;color:#9aa3af;margin-left:.35rem}
.tune dd{margin:0;color:#6b7280}
.controls{display:grid;gap:.35rem}
.ctrl{display:grid;grid-template-columns:minmax(8rem,13rem) 1fr auto;gap:.6rem;align-items:center;font-size:.85rem;color:#4b5563}
.ctrl em{font-style:normal;font-size:.72rem;color:#9aa3af}
.ctrl input[type=range]{width:100%}
.ctrl select{font:inherit;padding:.2rem .4rem;border-radius:8px;border:1px solid #d7dbe0;background:#fff;color:inherit}
/* The answer to a button press belongs next to the button, not at the top of a
   page you may have scrolled away from. */
.toast{position:fixed;right:1.25rem;bottom:1.25rem;z-index:20;max-width:min(30rem,calc(100vw - 2.5rem));
  padding:.85rem 1.1rem;border-radius:12px;font-size:.9rem;line-height:1.45;color:#fff;
  background:#14161a;box-shadow:0 10px 30px rgba(0,0,0,.28);cursor:pointer}
.toast[data-kind=ok]{background:#11663c}
.toast[data-kind=warn]{background:#8a5a00}
.toast[data-kind=error]{background:#9f1d13}
@media (prefers-color-scheme:dark){
.row label,.hint,.ctrl,.tune dd{color:#9aa3af}
.tune dt{color:#c8ccd4}
button{background:#1b1f25;color:#e7e9ee;border-color:#2b313a}
button.primary{background:#e7e9ee;color:#101215;border-color:#e7e9ee}
.ctrl select{background:#1b1f25;color:#e7e9ee;border-color:#2b313a}
}
</style>";

/// The page above the tab row.
const CALIBRATION_TOP: &str = r#"<main>
<header><div class="ident"><h1>Ambient Calibration (Experimental)</h1>
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
<label>Depth <input id="depth" type="range" min="3" max="30" value="26"><span id="depthv">26%</span></label>
<label><input id="mode" type="checkbox"> Ambient (this page)</label>
<button id="save" class="primary">Save calibration</button>
<button id="png">Save PNG</button>
</div>
<div class="row">
<label>Black level <input id="blacklevel" type="number" min="0" max="64" step="1" value="10"></label>
<label>Colour gain <input id="satgain" type="number" min="1" max="8" step="0.1" value="2"></label>
<label>Light gain <input id="lightgain" type="number" min="1" max="8" step="0.1" value="1.4"></label>
</div>
<div class="row">
<label>Max brightness <input id="maxbright" type="number" min="0" max="100" step="1" value="100"></label>
<label>Base glow <input id="minglow" type="number" min="0" max="100" step="1" value="1"></label>
<label>Smoothing <input id="smoothing" type="number" min="0" max="0.95" step="0.05" value="0.2"></label>
</div>
<dl class="tune">
<dt>Black level <span>0&ndash;64</span></dt>
<dd>Anything darker than this is treated as black, and everything above it is spread back
out so white stays white. Put a black screen up and raise it until the swatches stop
glowing. If the strip never goes dark when the film does, this is the number.</dd>
<dt>Colour gain <span>1&ndash;8</span></dt>
<dd>How hard a colour is pushed away from grey. The strip's firmware turns anything
faintly coloured into white, so this is what keeps a red car red. Too high and reds drift
pink while the shadows start to shimmer.</dd>
<dt>Light gain <span>1&ndash;8</span></dt>
<dd>Brightens everything above a floor of 8, so a dim scene gets lifted without a black
one being lit at the same time. Nudge it up for a dark picture, bring it down when bright
scenes come out white.</dd>
<dt>Max brightness <span>0&ndash;100</span></dt>
<dd>A ceiling for the whole strip. Home Assistant's brightness slider still works
underneath it. Lower it if the strip is brighter than the screen it is copying.</dd>
<dt>Base glow <span>0&ndash;100</span></dt>
<dd>The least any segment will ever be lit. One keeps the bottom end of the strip
from flickering between on and off as a scene creeps towards black; raise it further
if you like a faint glow behind the screen all the time.</dd>
<dt>Smoothing <span>0&ndash;0.95</span></dt>
<dd>How quickly the strip keeps up. 0 follows every frame and can look jittery, 0.2
is a step behind and still reacts instantly to a cut. Push it much higher and it
starts to feel like lag.</dd>
</dl>
<p class="hint">Black level, colour gain and light gain are the three that change how a
picture reaches the strip. The other three are personal taste. All six live in the
calibration file rather than in the container's environment, and each box snaps back if
you type a number outside the range beside it.</p>
<p class="hint">Click the four corners of the screen in order: top-left, top-right,
bottom-right, bottom-left. Drag a handle to adjust it; right-click removes the last
corner. The swatches below come from the same numbers the twenty segments are sent, so
the gains above show up there as you change them.</p>
</section>
<section class="card"><h2>Flat field</h2>
<div class="row">
<button id="auto" class="primary">Auto-calibrate camera</button>
<button id="black">Measure black</button>
<button id="white">Measure white</button>
</div>
<p class="hint">Put the <strong>white field</strong> up full-screen and press
<strong>Auto-calibrate camera</strong>. It pins the white balance, steps the exposure
until the white field lands in a usable window, and stores the white field with those
camera settings — so the screen's colour reaches the strip instead of being cancelled
out. It refuses anything that does not read evenly across the zones, because that is a
picture rather than a field. Then put the <strong>black field</strong> up and press
<strong>Measure black</strong>; auto-calibration never looks at black, and a black field
measured at a different exposure describes a different camera.</p>
<p class="hint">No reference video to hand? Search YouTube for <em>white screen</em> or
<em>black screen</em> &mdash; there are ten-hour versions of both &mdash; and cast it to
the television full-screen. Give it a moment to settle before pressing anything: a player
that fades in will have you measuring the fade.</p>
<p class="hint"><strong>Measure white</strong> is the fallback for a camera with no
absolute exposure control, where auto-calibration cannot run: it takes the white field at
whatever the camera is set to and changes nothing else. Measuring a field is what turns
a camera into an instrument — without a white field the strip shows the camera's own
shading, and without a black field it shows the room.</p>
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
<section class="card"><h2>Diagnostics</h2>
<div class="row">
<span class="hint" id="recstate">not recording</span>
<button id="record">Record</button>
<button id="download" class="primary">Download diagnostic</button>
</div>
<p class="hint">Record captures what the camera reported and the exact numbers sent to
the strip, ten times a second, for up to two minutes. Download bundles that with the
calibration, the camera settings, the link state and the log into one file. Nothing in
it identifies anything beyond this light.</p>
</section>
<section class="card"><h2>Segment colours</h2><div id="swatches" class="swatches"></div></section>
"#;

/// What closes the page: the toast, and the script that drives all of it.
///
/// Split from [`CALIBRATION_BOTTOM`] because the footer belongs inside `<main>` and
/// is the one part of the page that is not specific to calibration.
const CALIBRATION_SCRIPT: &str = r#"
<div id="toast" class="toast" hidden></div>
</main>
<script>
const view = document.getElementById('view');
const ctx = view.getContext('2d');
const img = document.getElementById('preview');
const statusLine = document.getElementById('status');
const swatchBox = document.getElementById('swatches');
const labels = ['TL', 'TR', 'BR', 'BL'];

// One place for the answer to a button press, because the buttons are below the
// fold and the status line is at the top of the page. An error stays up longer
// than a success, and a click dismisses either.
const toastBox = document.getElementById('toast');
const toast = (message, kind) => {
  toastBox.textContent = message;
  toastBox.dataset.kind = kind || 'info';
  toastBox.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => {
    toastBox.hidden = true;
  }, kind === 'error' ? 12000 : 7000);
};
toastBox.addEventListener('click', () => {
  toastBox.hidden = true;
});

let corners = [];
let latest = null;
let drag = -1;
let pointer = null;
let aspect = 1.7778;
let depth = 0.26;
// Kept so the tuning can be sent back whole: the page does not edit the floor, but
// dropping it would silently reset it to the default on every save.
let render = { black_level: 10, saturation_gain: 2, lightness_gain: 1.4, lightness_floor: 8,
               maximum_brightness: 100, minimum_lightness: 1, smoothing: 0.2 };

// Every tuning box, so one list drives the clamping and the display.
const TUNING = ['blacklevel', 'satgain', 'lightgain', 'maxbright', 'minglow', 'smoothing'];

const numberFrom = (id, fallback) => {
  const input = document.getElementById(id);
  const value = parseFloat(input.value);
  return Number.isFinite(value) ? value : fallback;
};

// Pull every box back inside the range printed beside it.
//
// The daemon refuses an out-of-range value with "the render settings are outside
// their ranges", which is the wrong thing to show someone who typed 800 into a
// box labelled 0-95. Clamping here keeps a stray keystroke from becoming a
// refused save, and says what happened rather than leaving a silently different
// number in the box.
const clampTuning = (announce) => {
  let clamped = null;
  TUNING.forEach((id) => {
    const input = document.getElementById(id);
    const min = parseFloat(input.min);
    const max = parseFloat(input.max);
    const value = parseFloat(input.value);
    const fixed = Number.isFinite(value) ? Math.min(max, Math.max(min, value)) : min;
    if (fixed !== value) {
      input.value = String(fixed);
      clamped = clamped || id;
    }
  });
  if (announce && clamped) {
    const input = document.getElementById(clamped);
    statusLine.textContent = 'clamped to ' + input.value +
      ' (allowed ' + input.min + ' to ' + input.max + ')';
  }
};

TUNING.forEach((id) => {
  document.getElementById(id).addEventListener('change', () => clampTuning(true));
});

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

  drawLoupe();
}

/// Draw an enlargement of what is under the pointer, next to it.
///
/// Placing a corner is the one job on this page that has to be exact: the clicks
/// define the whole sampling geometry, and a few pixels of error at a corner
/// becomes a band that reads the bezel instead of the picture. The canvas is a
/// scaled copy of a 640-wide frame, so what looks like a pixel in the browser can
/// be three on the camera — which is why the loupe is nearest-neighbour and shows
/// the actual pixels rather than a smooth enlargement of them.
function drawLoupe() {
  if (!pointer || !img.naturalWidth || !img.complete) return;

  const radius = Math.max(34, Math.round(view.width / 16));
  const zoom = 6;
  const cx = pointer.x * view.width;
  const cy = pointer.y * view.height;

  // Offset from the pointer, flipping to the other side when there is no room,
  // so the loupe never sits over the spot being aimed at.
  const gap = radius + 18;
  const lx = (cx + gap + radius <= view.width ? cx + gap : cx - gap);
  const ly = (cy + gap + radius <= view.height ? cy + gap : cy - gap);

  // Source rectangle in camera pixels: the canvas is a scaled copy of the frame,
  // so one canvas pixel is `naturalWidth / view.width` camera pixels.
  const half = (radius / zoom) * (img.naturalWidth / view.width);
  const sx = pointer.x * img.naturalWidth - half;
  const sy = pointer.y * img.naturalHeight - half;

  ctx.save();
  ctx.beginPath();
  ctx.arc(lx, ly, radius, 0, Math.PI * 2);
  ctx.clip();
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(img, sx, sy, half * 2, half * 2, lx - radius, ly - radius, radius * 2, radius * 2);
  ctx.restore();

  ctx.beginPath();
  ctx.arc(lx, ly, radius, 0, Math.PI * 2);
  ctx.strokeStyle = 'rgba(90,220,140,0.95)';
  ctx.lineWidth = 2;
  ctx.stroke();

  // Crosshair, so the centre of the loupe is unambiguous.
  ctx.beginPath();
  ctx.moveTo(lx - radius, ly);
  ctx.lineTo(lx - 8, ly);
  ctx.moveTo(lx + 8, ly);
  ctx.lineTo(lx + radius, ly);
  ctx.moveTo(lx, ly - radius);
  ctx.lineTo(lx, ly - 8);
  ctx.moveTo(lx, ly + 8);
  ctx.lineTo(lx, ly + radius);
  ctx.strokeStyle = 'rgba(90,220,140,0.55)';
  ctx.lineWidth = 1;
  ctx.stroke();
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
      if (latest.bypass) bits.push('raw camera');
      // The page's own switch, not the standing mode: that one belongs to Home
      // Assistant, and this page only borrows the strip while it is open.
      document.getElementById('mode').checked = Boolean(latest.page_mode);
      if (latest.page_mode) bits.push('ambient, this page only');
      else if (latest.mode) bits.push('ambient mode');
      if (latest.recording && latest.recording.running) {
        recordState.textContent = 'recording\u2026 ' + latest.recording.frames + ' frames';
      }
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
  clampTuning(false);
  const res = await fetch('/api/calibration', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      aspect,
      depth,
      corners: corners.map((c) => [c.x, c.y]),
      render: {
        black_level: numberFrom('blacklevel', render.black_level),
        saturation_gain: numberFrom('satgain', render.saturation_gain),
        lightness_gain: numberFrom('lightgain', render.lightness_gain),
        lightness_floor: render.lightness_floor,
        maximum_brightness: numberFrom('maxbright', render.maximum_brightness),
        minimum_lightness: numberFrom('minglow', render.minimum_lightness),
        smoothing: numberFrom('smoothing', render.smoothing),
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

const recordButton = document.getElementById('record');
const recordState = document.getElementById('recstate');
let recording = false;

async function setRecording(on) {
  const res = await fetch('/api/diagnostic/record', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ on }),
  });
  const body = await res.json().catch(() => ({}));
  recording = Boolean(body.recording);
  recordButton.textContent = recording ? 'Stop' : 'Record';
  recordButton.classList.toggle('primary', recording);
  recordState.textContent = recording ? 'recording\u2026' : (body.frames || 0) + ' frames captured';
}

async function downloadDiagnostic() {
  statusLine.textContent = 'collecting\u2026';
  const res = await fetch('/api/diagnostic');
  if (!res.ok) {
    statusLine.textContent = 'could not collect the diagnostic: ' + res.status;
    return;
  }
  const text = await res.text();
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const blob = new Blob([text], { type: 'application/json' });
  const link = document.createElement('a');
  link.href = URL.createObjectURL(blob);
  link.download = 'neewertv-diagnostic-' + stamp + '.json';
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href), 10000);
  statusLine.textContent = 'diagnostic downloaded (' + Math.round(text.length / 1024) + ' KB)';
}

async function autoCalibrate() {
  statusLine.textContent = 'setting the camera up from the white field \u2014 this takes a few seconds\u2026';
  const res = await fetch('/api/calibration/auto', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: '{}',
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    toast('Auto-calibration failed: ' + (body.error || res.status), 'error');
    statusLine.textContent = 'auto-calibration failed';
    return;
  }
  const percent = (v) => (typeof v === 'number' ? (v * 100).toFixed(0) + '%' : '?');
  if (body.reached) {
    toast('Camera set up. White field stored at ' + percent(body.brightness) +
      ' on exposure ' + body.exposure + '. Now put the black field up and press Measure black.',
      'ok');
    statusLine.textContent = 'camera set up';
  } else {
    toast('The white field could not be brought into range: it settled at ' +
      percent(body.brightness) + ' with ' + percent(body.clipped) + ' clipped. ' +
      (body.steps || []).slice(-1)[0] + '. The camera has been put back as it was.', 'warn');
    statusLine.textContent = 'auto-calibration did not reach the target';
  }
  loadSaved();
  loadCamera();
}

async function measureField(field) {
  toast('Measuring the ' + field + ' field\u2026', 'info');
  const res = await fetch('/api/calibration/field', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ field }),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    toast('Could not measure ' + field + ': ' + (body.error || res.status), 'error');
    return;
  }
  let line = field + ' field stored';
  const detail = [];
  if (field === 'white' && typeof body.spread === 'number') {
    detail.push('zones differ by ' + Math.round(body.spread * 100) + '%');
  }
  if (field === 'black' && typeof body.level === 'number') {
    detail.push('floor ' + (body.level * 100).toFixed(2) + '%');
  }
  if (detail.length) line += ' \u00b7 ' + detail.join(' \u00b7 ');
  toast(body.warning || line, body.warning ? 'warn' : 'ok');
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
  pointer = pointAt(event);
  if (drag < 0) return;
  corners[drag] = pointer;
  draw();
});

view.addEventListener('pointerleave', () => {
  pointer = null;
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
document.getElementById('save').addEventListener('click', saveCalibration);
document.getElementById('png').addEventListener('click', savePng);
recordButton.addEventListener('click', () => setRecording(!recording));
document.getElementById('download').addEventListener('click', downloadDiagnostic);
document.getElementById('mode').addEventListener('change', async (event) => {
  await fetch('/api/ambient/mode', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ on: event.target.checked }),
  });
  statusLine.textContent = event.target.checked
    ? 'ambient while this page is open \u2014 close it and Home Assistant decides again'
    : 'ambient off for this page';
});
document.getElementById('white').addEventListener('click', () => measureField('white'));
document.getElementById('black').addEventListener('click', () => measureField('black'));
document.getElementById('auto').addEventListener('click', autoCalibrate);

async function loadSaved() {
  try {
    const res = await fetch('/api/calibration');
    if (!res.ok) return;
    const saved = await res.json();
    corners = saved.corners.map(([x, y]) => ({ x, y }));
    aspect = saved.aspect;
    depth = saved.depth;
    if (saved.render) render = Object.assign(render, saved.render);
    // Rounded on the way in: these come back from a 32-bit float, and a box
    // reading 1.2999999523162842 is the first thing anyone asks about.
    const show = (id, value) => {
      document.getElementById(id).value = String(Math.round(value * 100) / 100);
    };
    show('blacklevel', render.black_level);
    show('satgain', render.saturation_gain);
    show('lightgain', render.lightness_gain);
    show('maxbright', render.maximum_brightness);
    show('minglow', render.minimum_lightness);
    show('smoothing', render.smoothing);
    const percent = Math.round(depth * 100);
    document.getElementById('depth').value = String(percent);
    document.getElementById('depthv').textContent = percent + '%';
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

/// A hue in degrees, carried to the page as a fraction of a *turn* rather than of
/// a byte.
///
/// The page's hue-to-RGB conversion multiplies by six, so a fraction of a turn is
/// the only scale it can consume: dividing by 255 stretched every hue by 1.41 and
/// wrapped anything past 181°, which drew a blue zone magenta and made the
/// swatches describe a colour the strip was never sent.
fn hue_fraction(degrees: u16) -> f32 {
    f32::from(degrees) / 360.0
}

/// How far apart the brightest and dimmest samples of one measurement read, as a
/// fraction of the brightest. Zero is perfectly even.
///
/// This is what separates a white field from a picture: a white field is even
/// across every zone whatever the exposure, and a picture never is.
fn spread_of(linear: &[[f32; 3]; ambient::SEGMENTS]) -> f32 {
    let mut lowest = f32::MAX;
    let mut highest = 0.0_f32;
    for zone in linear {
        for value in zone {
            lowest = lowest.min(*value);
            highest = highest.max(*value);
        }
    }
    if highest <= 0.0 {
        0.0
    } else {
        1.0 - lowest / highest
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
    // saturation), normalised so the page does not carry the wire scale.
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
                    "hue": hue_fraction(hue),
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
        "bypass": ambient.bypassing(),
        // The standing mode, which only Home Assistant sets, and the page's own
        // override, which lasts as long as the page does.
        "mode": ambient.mode(),
        "page_mode": ambient.page_mode(),
        "recording": {
            "running": ambient.recording().0,
            "frames": ambient.recording().1,
        },
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

/// Whether to stop correcting, or to leave the strip following the camera.
#[derive(Debug, Deserialize)]
struct BypassRequest {
    on: bool,
}

/// Switch the flat field and the transfer off, or back on.
async fn api_ambient_bypass(
    State(state): State<AppState>,
    Json(request): Json<BypassRequest>,
) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ambient.set_bypass(request.on);
    Json(json!({ "ok": true, "bypass": ambient.bypassing() })).into_response()
}

/// Leave the strip following the camera, or stop.
///
/// The same switch Home Assistant reaches through the effect list. Being able to
/// flip it here is what makes the mode testable without a working MQTT broker.
async fn api_ambient_mode(
    State(state): State<AppState>,
    Json(request): Json<BypassRequest>,
) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ambient.set_page_mode(request.on);
    Json(json!({ "ok": true, "page_mode": ambient.page_mode() })).into_response()
}

/// Start or stop capturing preview frames into the diagnostic bundle.
async fn api_diagnostic_record(
    State(state): State<AppState>,
    Json(request): Json<BypassRequest>,
) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ambient.set_recording(request.on);
    let (recording, frames) = ambient.recording();
    Json(json!({ "ok": true, "recording": recording, "frames": frames })).into_response()
}

/// Everything needed to explain what the pipeline did, in one file.
///
/// Seconds since the epoch.
///
/// Deliberately not a formatted date: the browser knows what time it is, and a
/// daemon that formats dates would need a calendar crate to get it wrong.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Deliberately one JSON document rather than an archive: it is meant to be
/// attached to a message and read, and a zip of four files is more work for both
/// ends than a single object.
async fn api_diagnostic(State(state): State<AppState>) -> Response {
    let snapshot = state.snapshot.borrow().clone();
    let Some(ambient) = state.ambient.as_ref() else {
        return Json(json!({
            "generated_unix": now_unix(),
            "device": state.device_name,
            "ambient": Value::Null,
            "note": "ambient capture is off for this deployment",
        }))
        .into_response();
    };

    let (recording, frames) = ambient.recording();
    let controls = ambient.camera_controls().await.unwrap_or_default();

    Json(json!({
        "generated_unix": now_unix(),
        "device": state.device_name,
        "version": env!("CARGO_PKG_VERSION"),
        "link": {
            "connected": snapshot.connected,
            "address": snapshot.address,
            "preview": snapshot.preview,
            "writes_sent": snapshot.writes_sent,
            "frames_skipped": snapshot.frames_skipped,
            "commands_received": snapshot.commands_received,
            "drops": snapshot.drops,
            "last_error": snapshot.last_error,
            "on": snapshot.on,
            "brightness": snapshot.brightness,
        },
        "calibration": ambient.calibration().map(|stored| (*stored).clone()),
        "bypass": ambient.bypassing(),
        "recording": { "running": recording, "frames": frames },
        "camera": controls,
        "samples": ambient.recorded(),
        "latest": ambient.latest().map(|latest| json!({
            "frame": { "width": latest.frame.width, "height": latest.frame.height },
            "fps": latest.fps,
            "calibrated": latest.calibrated,
            "clipped_zones": latest.clipped_zones,
            "colours": latest.colours,
            "render": latest.render,
        })),
        "logs": state.logs.snapshot(),
    }))
    .into_response()
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
        // Always linear light. It used to be a checkbox, but there is no reading
        // for which averaging gamma-encoded bytes is the right arithmetic — mixed
        // zones come out too dark — and a correction only exists in linear light
        // anyway, so the box could do nothing but make the picture wrong.
        linear: true,
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
        ambient: ambient.mode(),
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

/// A white field this flat is not a picture of a lens.
///
/// Every real camera is a few percent darker at the edges than in the middle,
/// and sits at an angle to the screen. A white field that comes back the same in
/// all twenty zones means the camera clipped to a plateau, or the screen was not
/// the white field at all — and either way the gains it produces are all 1.0, so
/// the half of the flat field that is supposed to flatten the camera does nothing.
const SUSPICIOUSLY_FLAT: f32 = 1.02;

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
    let dimmest = measurement
        .linear
        .iter()
        .flat_map(|zone| zone.iter())
        .copied()
        .fold(f32::MAX, f32::min);

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
    let flat =
        (field == "white" && dimmest > 0.0 && brightest < dimmest * SUSPICIOUSLY_FLAT).then(|| {
            format!(
                "every zone read {:.0}%, so there is nothing to flatten — many cameras correct \
                 their own shading in firmware, and this one does. The white field is stored, \
                 but it will cancel nothing; the black field is doing the work",
                brightest * 100.0
            )
        });
    let warning = lifted.or(flat);

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
            "warning": warning,
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// Where a properly exposed white field should land, in linear light.
///
/// **Below the camera's own ceiling, not near it.** This was 0.75 — about sRGB
/// 225 — on the theory that a highlight in the picture should still have room. It
/// does not work out that way: this camera tops out around sRGB 204, and its ISP
/// desaturates as it approaches that, so a blue sky came back as `204,204,204`
/// and the strip painted it white. Nothing caught it, because clipping is counted
/// at 252 and this camera can never reach 252 to be counted.
///
/// 0.45 is about sRGB 180, which leaves the highlights somewhere to go and keeps
/// the white field off the shoulder that throws colour away.
const WHITE_TARGET: f32 = 0.45;
/// Below this the field is under-exposed and the search opens up.
const WHITE_LOW: f32 = 0.30;
/// Above this it is close enough to the desaturation shoulder to throw colour away.
const WHITE_HIGH: f32 = 0.65;

/// How far apart the zones may read and still be a white field, as a fraction.
///
/// A white field is even across every zone at any exposure — that is the premise
/// the whole flat field rests on — while a picture is not. This camera corrects
/// its own shading, so a real white field measures within a couple of percent;
/// twenty is loose enough to let a badly exposed field through and still refuse a
/// scene, which measured fifty.
const WHITE_FIELD_SPREAD: f32 = 0.20;

/// Control names this relies on.
///
/// V4L2 names its own controls, and every driver for a UVC camera uses these
/// strings. Matching on the name rather than the id keeps it working across
/// cameras whose ids differ, at the cost of falling back gracefully when a name
/// is missing.
const NAME_EXPOSURE: &str = "exposure time, absolute";
const NAME_GAIN: &str = "gain";
const NAME_AUTO_EXPOSURE: &str = "auto exposure";
const NAME_AUTO_WHITE_BALANCE: &str = "white balance, automatic";
const NAME_WHITE_BALANCE: &str = "white balance temperature";

/// How many measure-and-adjust rounds the search takes before giving up.
const AUTO_STEPS: usize = 6;

/// Point the camera at the white field and set it up from the reading.
///
/// The point of this is that a camera left alone cannot be trusted: it hunts the
/// scene, its white balance chases whatever colour is on screen, and its exposure
/// is a guess. The operator can fix all three by hand, which is what the sliders
/// are for — but the measurement itself is the better authority. It steps the
/// exposure until the field lands in a usable window, pins the white balance so
/// the screen's colour reaches the strip, and then takes the white field.
async fn api_calibration_auto(State(state): State<AppState>) -> Response {
    let Some(ambient) = state.ambient.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(stored) = ambient.calibration() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "set the four corners before calibrating the camera" })),
        )
            .into_response();
    };

    let controls = match ambient.camera_controls().await {
        Ok(controls) => controls,
        Err(err) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    let find = |needle: &str| {
        controls
            .iter()
            .find(|control| control.name.to_lowercase().contains(needle))
    };

    let mut steps: Vec<String> = Vec::new();

    // Where the camera was before any of this. A search that never reaches the
    // window used to stop wherever it ran out — maximum exposure and gain, when
    // the field in front of it is darker than the target can be reached from —
    // and the next ordinary picture then came back as a white wash with nothing
    // in the log to explain it. The instrument is put back instead.
    let original_camera: Vec<ambient::CameraSetting> = controls
        .iter()
        .filter(|control| !control.read_only && !control.inactive)
        .map(|control| ambient::CameraSetting {
            id: control.id,
            value: control.value,
        })
        .collect();

    // White balance first: left automatic it chases the screen's own colour and
    // cancels it out, so the strip is told the picture is grey. A fixed daylight
    // temperature is the neutral choice for a screen.
    if let Some(auto) = find(NAME_AUTO_WHITE_BALANCE)
        && auto.value != 0
    {
        if let Err(err) = ambient.set_camera_control(auto.id, 0).await {
            steps.push(format!("white balance could not be pinned: {err}"));
        } else {
            steps.push("white balance pinned".to_owned());
        }
    }
    if let Some(wb) = find(NAME_WHITE_BALANCE)
        && !wb.read_only
        && wb.value != 6500
        && (wb.minimum..=wb.maximum).contains(&6500)
    {
        let _ = ambient.set_camera_control(wb.id, 6500).await;
        steps.push("white balance set to 6500K".to_owned());
    }

    // Exposure has to be manual before it can be stepped.
    if let Some(auto) = find(NAME_AUTO_EXPOSURE)
        && auto.kind == "menu"
    {
        let manual = auto
            .items
            .iter()
            .find(|(_, label)| label.to_lowercase().contains("manual"))
            .map(|(value, _)| i64::from(*value));
        if let Some(manual) = manual
            && auto.value != manual
        {
            let _ = ambient.set_camera_control(auto.id, manual).await;
            steps.push("exposure set to manual".to_owned());
        }
    }

    let Some(exposure) = find(NAME_EXPOSURE) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "this camera has no absolute exposure control, so it cannot be \
                          set up automatically",
                "steps": steps,
            })),
        )
            .into_response();
    };

    let mut value = exposure.value;
    let mut brightest = 0.0_f32;
    let mut clipped = 0.0_f32;
    let mut reached = false;
    let mut accepted: Option<[[f32; 3]; ambient::SEGMENTS]> = None;

    for step in 0..AUTO_STEPS {
        // The strip is switched off for the measurement, exactly as a manual one
        // is: the light that would otherwise land on the screen is its own.
        let measurement = match ambient.measure().await {
            Ok(measurement) => measurement,
            Err(err) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": err.to_string(), "steps": steps })),
                )
                    .into_response();
            }
        };
        brightest = measurement.brightest();
        clipped = measurement.worst_clip();

        // Before anything is touched: is this a white field at all? The search
        // optimises whatever it is given, so a scene it cannot satisfy drives it
        // to the camera's limit instead of stopping. `Measure white` has always
        // refused a field that does not look white; this is the same evidence,
        // taken before the camera is changed rather than after.
        if step == 0 {
            let spread = spread_of(&measurement.linear);
            if brightest >= 4.0 * sampling::MIN_MEASURED_WHITE && spread > WHITE_FIELD_SPREAD {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!(
                            "this does not look like a white field: the zones read {:.0}% apart, \
                             and a white field reads evenly at any exposure. Put the white field \
                             up full-screen and try again",
                            spread * 100.0
                        ),
                        "steps": steps,
                    })),
                )
                    .into_response();
            }
        }

        if clipped <= 0.02 && (WHITE_LOW..=WHITE_HIGH).contains(&brightest) {
            reached = true;
            accepted = Some(measurement.linear);
            break;
        }

        // Exposure is close to linear in brightness, so the ratio is the step.
        let ratio = if brightest > 0.01 {
            WHITE_TARGET / brightest
        } else {
            2.0
        };
        let next = (value as f32 * ratio.clamp(0.3, 3.0)).round() as i64;
        let next = next.max(exposure.minimum.max(1));

        if next == value {
            // The control will not move any further. Gain is the lever left, and
            // it costs noise — which is why it is second.
            if let Some(gain) = find(NAME_GAIN) {
                let wanted = (gain.value as f32 * ratio.clamp(1.0, 3.0)).round() as i64;
                let wanted = wanted.clamp(gain.minimum, gain.maximum.max(gain.minimum));
                if wanted != gain.value {
                    let _ = ambient.set_camera_control(gain.id, wanted).await;
                    steps.push(format!("exposure is at its limit; gain raised to {wanted}"));
                    continue;
                }
            }
            steps.push("the camera will not go any brighter".to_owned());
            break;
        }

        if let Err(err) = ambient.set_camera_control(exposure.id, next).await {
            steps.push(format!("exposure could not be set: {err}"));
            break;
        }
        steps.push(format!("exposure {value} -> {next}"));
        value = next;
    }

    let mut calibration = (*stored).clone();
    let stored_white = accepted.is_some();
    if let Some(linear) = accepted {
        calibration.white = Some(linear);
    }

    // The camera this calibration describes is whatever the search settled on, so
    // it is written with it. A restart then comes back to the same instrument
    // rather than to whatever the camera decided today.
    if let Ok(controls) = ambient.camera_controls().await {
        calibration.camera = controls
            .iter()
            .filter(|control| !control.read_only && !control.inactive)
            .map(|control| ambient::CameraSetting {
                id: control.id,
                value: control.value,
            })
            .collect();
    }
    if stored_white {
        let _ = ambient.save(calibration);
    } else {
        // Nothing was stored, so nothing is left behind either. The search is the
        // only thing that ever moves this camera without being asked to, and a
        // failure is exactly when it must give the settings back.
        for setting in &original_camera {
            let _ = ambient.set_camera_control(setting.id, setting.value).await;
        }
        steps.push("the camera was put back where it started".to_owned());
    }

    Json(json!({
        "ok": true,
        "reached": reached,
        "white_stored": stored_white,
        "brightness": brightest,
        "clipped": clipped,
        "exposure": value,
        "target": WHITE_TARGET,
        "steps": steps,
    }))
    .into_response()
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
    page.push_str("<footer class=\"pagefoot\">");
    page.push_str(&page_footer(&state));
    page.push_str("</footer>");
    page.push_str(CALIBRATION_SCRIPT);
    Html(page).into_response()
}

/// The line at the foot of a page: where to look when it misbehaves, who wrote it,
/// and which version is running.
///
/// One function rather than two copies, because the calibration page needs the same
/// things for the same reason, and a version number that drifts between two pages is
/// worse than one that is missing.
fn page_footer(state: &AppState) -> String {
    format!(
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
    )
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
/* The tab row. These have to come BEFORE the dark block: an equally specific rule
   that comes after it wins, and having them last left the unselected tab white and
   the selected one dark in dark mode — the opposite of what the class says. */
.tabs{display:flex;gap:.4rem;margin:0 0 1.15rem;flex-wrap:wrap}
.tabs a{padding:.4rem .9rem;border-radius:999px;text-decoration:none;font-size:.9rem;font-weight:500;color:#4b5563;background:#eef1f5}
.tabs a.on{background:#14161a;color:#fff}
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

    let footer = page_footer(&state);

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
    fn a_hue_reaches_the_page_as_a_fraction_of_a_turn() {
        // The page converts with `hue * 6`, so the six sectors of the wheel have
        // to land on 0, 2 and 4 — not on 5.6, which is where dividing degrees by
        // 255 put blue. That bug drew a blue zone magenta, so the swatches said
        // the strip was being sent a colour it never was, and the one place that
        // can be checked against the camera picture stopped agreeing with it.
        for (degrees, sector) in [(0_u16, 0.0_f32), (120, 2.0), (240, 4.0), (300, 5.0)] {
            let landed = hue_fraction(degrees) * 6.0;
            assert!(
                (landed - sector).abs() < 1e-6,
                "{degrees} degrees landed in sector {landed}, not {sector}"
            );
        }
        // 360 degrees is a full turn, and the conversion has to accept it as one.
        assert_eq!(hue_fraction(360), 1.0);
    }

    #[test]
    fn a_picture_is_told_apart_from_a_white_field() {
        // A white field reads the same everywhere, at any exposure. That is the
        // premise the flat field rests on, and it is the only thing separating the
        // field from a scene — which the exposure search will otherwise chase to
        // the camera's limit and then leave there.
        let even = [[0.5_f32; 3]; ambient::SEGMENTS];
        assert_eq!(spread_of(&even), 0.0);

        // Half the samples at half the level: the shape of a picture, and fifty
        // percent apart rather than the couple this camera's own shading gives.
        let mut uneven = [[0.5_f32; 3]; ambient::SEGMENTS];
        uneven[0] = [0.25; 3];
        let spread = spread_of(&uneven);
        assert!((spread - 0.5).abs() < 1e-6, "spread was {spread}");
        assert!(spread > WHITE_FIELD_SPREAD);

        // Black is perfectly even, and says nothing about what is on the screen —
        // which is why the spread is only consulted once the field is lit.
        assert_eq!(spread_of(&[[0.0_f32; 3]; ambient::SEGMENTS]), 0.0);
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
