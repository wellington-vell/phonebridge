const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const els = {
  app: $("app"),
  status: $("status"),
  substatus: $("substatus"),
  mute: $("mute"),
  muteLabel: $("mute-label"),
  gain: $("gain"),
  gainValue: $("gain-value"),
  toggle: $("toggle"),
  toggleLabel: $("toggle-label"),
  error: $("error"),
  port: $("port"),
  source: $("source"),
  keepScreen: $("keep-screen"),
  hintPrep: $("hint-prep"),
  hintSave: $("hint-save"),
  hintRun: $("hint-run"),
  token: $("token"),
  newCode: $("new-code"),
  allowUnauth: $("allow-unauth"),
  insecureNote: $("insecure-note"),
  runningNote: $("running-note"),
  camera: $("camera"),
  cameraToggle: $("camera-toggle"),
  cameraLabel: $("camera-label"),
  cameraState: $("camera-state"),
  cameraNote: $("camera-note"),
  cameraProblem: $("camera-problem"),
  cameraError: $("camera-error"),
  cameraSettings: $("camera-settings"),
  previewWrap: $("preview-wrap"),
  preview: $("preview"),
  previewText: $("preview-text"),
  flip: $("flip"),
  lens: $("lens"),
  resolution: $("resolution"),
  orientation: $("orientation"),
  sheet: $("sheet"),
  scrim: $("scrim"),
  openSheet: $("open-sheet"),
  closeSheet: $("close-sheet"),
  dockSummary: $("dock-summary"),
};
const transportInputs = [...document.querySelectorAll('input[name="transport"]')];

// ------------------------------------------------------------------ settings
const DEFAULTS = {
  transport: "usb",
  port: 8125,
  source: "mic",
  keepScreenOn: true,
  gain: 100,
  muted: false,
  // Plaintext clients without the pairing code (official WO Mic client).
  allowUnauthenticated: false,
  camera: false,
  lens: "back",
  resolution: 720,
  orientation: "landscape",
};

// 16:9 sizes asked of the camera; the phone picks the closest it supports.
const RESOLUTIONS = {
  480: { width: 854, height: 480 },
  720: { width: 1280, height: 720 },
  1080: { width: 1920, height: 1080 },
};
const CAMERA_FPS = 30;

const TRANSPORT_NAME = { usb: "USB", wifi: "Wi‑Fi", bluetooth: "Bluetooth" };

function load() {
  try {
    return { ...DEFAULTS, ...JSON.parse(localStorage.getItem("phonebridge") || "{}") };
  } catch {
    return { ...DEFAULTS };
  }
}

const settings = load();

function save() {
  try {
    localStorage.setItem("phonebridge", JSON.stringify(settings));
  } catch {
    /* storage unavailable: settings last for this session only */
  }
}

// ------------------------------------------------------------------ rendering
let running = false;
let addresses = [];
// Pairing code, display form ("ABCDE-FGHJK-MNPQR-STVWX"); "" until loaded.
let token = "";

/** Disclosure rows show the chosen option's text next to the hidden native select. */
function syncSelect(select) {
  const out = document.querySelector(`.row-value[data-for="${select.id}"]`);
  if (out) out.textContent = select.selectedOptions[0] ? select.selectedOptions[0].text : "";
}

function renderMute() {
  els.mute.setAttribute("aria-pressed", String(settings.muted));
  els.mute.setAttribute("aria-label", settings.muted ? "Unmute" : "Mute");
  els.muteLabel.textContent = settings.muted ? "Muted" : "Mic on";
  els.app.classList.toggle("muted", settings.muted);
}

function renderGain() {
  els.gain.value = settings.gain;
  els.gainValue.textContent = `${settings.gain}%`;
  els.gain.style.setProperty("--fill", `${(settings.gain / 400) * 100}%`);
}

// Video is too slow over Bluetooth.
const cameraOn = () => settings.camera && settings.transport !== "bluetooth";

/** Camera settings for start / set_camera, or null when sharing is off. */
function cameraConfig() {
  if (!cameraOn()) return null;
  return {
    lens: settings.lens,
    ...(RESOLUTIONS[settings.resolution] || RESOLUTIONS[720]),
    fps: CAMERA_FPS,
    orientation: settings.orientation,
  };
}

/** An element with text content; never parses HTML. */
function el(tag, ...children) {
  const node = document.createElement(tag);
  node.append(...children);
  return node;
}

/** The "Connect your PC" steps for the chosen transport. */
function renderHint() {
  const port = settings.port;
  const portArg = (port === 8125 ? "" : ` --port ${port}`) + (cameraOn() ? " --camera" : "");
  const code = token || "PAIRING-CODE";
  els.hintSave.textContent = `echo ${code} | install -Dm600 /dev/stdin ~/.config/phonebridge/token`;
  let prep;
  let cmds;
  switch (settings.transport) {
    case "wifi": {
      const ips = addresses.length ? addresses.map((a) => a.ip) : ["PHONE_IP"];
      prep = ["Put the phone and the PC on the same network."];
      cmds = ips.map((ip) => `phonebridge --wifi ${ip}${portArg}`);
      break;
    }
    case "bluetooth":
      prep = [
        "Pair the phone with the PC first. Its address is in ",
        el("code", "bluetoothctl devices"),
        ". Bluetooth carries the microphone only.",
      ];
      cmds = ["phonebridge --bluetooth AA:BB:CC:DD:EE:FF"];
      break;
    default:
      prep = ["Enable USB debugging and plug the phone into the PC."];
      cmds = [`phonebridge${portArg}`];
  }
  els.hintPrep.replaceChildren(...prep);
  els.hintRun.replaceChildren(...cmds.map((c) => el("code", c)));
  [...els.hintRun.children].forEach((c) => (c.className = "cmd"));
  renderDock();
}

function renderDock() {
  const parts = [TRANSPORT_NAME[settings.transport] || "USB"];
  if (settings.transport === "wifi" && addresses.length) parts.push(addresses[0].ip);
  if (settings.port !== 8125 && settings.transport !== "bluetooth") parts.push(`port ${settings.port}`);
  parts.push(token ? `code ${token.split("-")[0]}…` : "pairing code");
  els.dockSummary.textContent = parts.join(" · ");
}

function renderToken() {
  els.token.textContent = token || "…";
  renderDock();
}

function renderInsecure() {
  els.allowUnauth.checked = settings.allowUnauthenticated;
  els.insecureNote.hidden = !settings.allowUnauthenticated;
}

function renderControls() {
  els.toggleLabel.textContent = running ? "Stop" : "Start";
  els.toggle.setAttribute("aria-label", running ? "Stop" : "Start");
  els.toggle.classList.toggle("running", running);
  els.app.classList.toggle("running", running);
  // Transport and port only apply when (re)starting.
  els.port.disabled = running;
  transportInputs.forEach((i) => (i.disabled = running));
  els.allowUnauth.disabled = running;
  // The servers keep the code they started with.
  els.newCode.disabled = running;
  els.newCode.title = running ? "Stop to make a new code" : "";
  els.runningNote.hidden = !running;
  // Camera settings apply live, but the camera is USB / Wi-Fi only.
  const bluetooth = settings.transport === "bluetooth";
  els.camera.disabled = bluetooth;
  els.cameraToggle.classList.toggle("disabled", bluetooth);
  els.flip.disabled = bluetooth;
  [els.lens, els.resolution, els.orientation].forEach((s) => {
    s.disabled = bluetooth;
    s.closest(".row").classList.toggle("disabled", bluetooth);
  });
  [els.port, els.allowUnauth].forEach((i) => i.closest(".row").classList.toggle("disabled", running));
  els.cameraNote.hidden = !(bluetooth && settings.camera);
  els.cameraToggle.dataset.on = String(settings.camera && !bluetooth);
}

const CAMERA_LABEL = {
  off: "Off",
  starting: "Starting…",
  listening: "Waiting for PC",
  streaming: "Streaming",
  error: "Error",
};

let camera = { state: "off" };

function renderCamera(cam) {
  camera = cam && CAMERA_LABEL[cam.state] ? cam : { state: "off" };
  const state = camera.state;
  let label = CAMERA_LABEL[state];
  if (state === "streaming" && camera.height) label += ` · ${Math.min(camera.width, camera.height)}p`;
  if (!running && settings.camera && state === "off") label = "On at start";
  els.cameraState.textContent = label;
  els.cameraState.dataset.state = state;
  els.cameraLabel.textContent = cameraOn() ? "Camera on" : "Camera off";

  // Errors only matter while sharing is wanted.
  const error = cameraOn() && running ? camera.error : "";
  const denied = Boolean(error) && camera.permission === false;
  const msg = denied
    ? "Camera permission denied. Allow the camera for Phonebridge in Android settings."
    : error || "";
  els.cameraProblem.hidden = !msg;
  els.cameraError.textContent = msg;
  els.cameraSettings.hidden = !denied;

  renderPreview();
}

/** Placeholder text, and whether the native live preview may show. */
function renderPreview() {
  const state = camera.state;
  const open = running && cameraOn() && (state === "listening" || state === "streaming");
  let text;
  if (!settings.camera) text = "Camera off";
  else if (settings.transport === "bluetooth") text = "Camera sharing needs USB or Wi‑Fi";
  else if (!running) text = "Press Start to share the camera";
  else if (state === "error") text = "Camera unavailable";
  else if (state === "off") text = "Camera off";
  else text = "Starting camera…";
  els.previewText.textContent = text;
  els.preview.dataset.state = open || state === "starting" ? "active" : "off";
  // The floating preview card only exists while the camera is wanted.
  els.previewWrap.hidden = !cameraOn();
  els.app.classList.toggle("with-camera", cameraOn());
  previewVisible = open;
  reportPreview();
}

// --------------------------------------------------------------- preview
// The live preview is a native view over #preview (see CameraPreview.kt);
// tell it where #preview is, in physical pixels relative to the WebView.
// It is hidden while the settings sheet covers the home screen.
let previewVisible = false;
let sheetOpen = false;
let lastRect = "";

function reportPreview() {
  const r = els.preview.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const radius = parseFloat(getComputedStyle(els.preview).borderTopLeftRadius) || 0;
  const rect = {
    x: Math.round(r.left * dpr),
    y: Math.round(r.top * dpr),
    width: Math.round(r.width * dpr),
    height: Math.round(r.height * dpr),
    radius: radius * dpr,
    visible: previewVisible && !sheetOpen && r.width > 0 && r.height > 0,
  };
  const key = JSON.stringify(rect);
  if (key === lastRect) return;
  lastRect = key;
  invoke("set_preview_rect", { rect }).catch(() => {
    lastRect = "";
  });
}

if (window.ResizeObserver) new ResizeObserver(reportPreview).observe(els.preview);
window.addEventListener("resize", reportPreview);

// Camera switches while running, one at a time, in order.
let cameraQueue = Promise.resolve();
function applyCamera() {
  if (!running) return;
  cameraQueue = cameraQueue
    .then(() => invoke("set_camera", { camera: cameraConfig() }))
    .catch(() => {
      /* kept by the app, shown through status */
    });
}

function showError(msg) {
  els.error.hidden = !msg;
  els.error.textContent = msg || "";
}

const STATE_LABEL = {
  idle: "Ready when you are",
  listening: "Waiting for your PC…",
  connected: "Connected to PC",
};

function renderSubstatus(s) {
  const transport = s && s.transport ? s.transport : settings.transport;
  const name = TRANSPORT_NAME[transport] || "USB";
  if (!running) {
    els.substatus.textContent = `Tap to start over ${name}`;
    return;
  }
  const ip = transport === "wifi" && addresses.length ? ` · ${addresses[0].ip}` : "";
  els.substatus.textContent = `Over ${name}${ip}`;
}

async function poll() {
  try {
    const s = await invoke("status");
    running = s.state !== "idle" || s.transport !== null;
    // The microphone and the camera have their own PC connections (the PC
    // runs them as separate instances): show both.
    const mic = s.state === "connected";
    const cam = s.camera && s.camera.state === "streaming";
    let label = STATE_LABEL[s.state] || STATE_LABEL.idle;
    if (mic && cam) label = "Connected to PC · mic + camera";
    else if (cam) label = "Connected to PC · camera";
    else if (mic && s.peer) label = `Connected to ${s.peer} · mic`;
    else if (mic) label = "Connected to PC · mic";
    const state = mic || cam ? "connected" : s.state;
    els.status.textContent = label;
    els.status.dataset.state = state;
    els.app.dataset.state = state;
    const level = settings.muted ? 0 : Math.min(1, Math.sqrt(s.level || 0));
    els.app.style.setProperty("--level", level.toFixed(3));
    showError(s.error);
    const ips = JSON.stringify(s.addresses);
    if (ips !== JSON.stringify(addresses)) {
      addresses = s.addresses;
      renderHint();
    }
    renderCamera(s.camera);
    renderControls();
    renderSubstatus(s);
  } catch (e) {
    showError(String(e));
  }
}

// ------------------------------------------------------------------ sheet
// The settings sheet. Android's back gesture closes it (via history).
function setSheet(open) {
  if (open === sheetOpen) return;
  sheetOpen = open;
  document.body.classList.toggle("sheet-open", open);
  els.sheet.setAttribute("aria-hidden", String(!open));
  els.sheet.inert = !open;
  els.app.inert = open;
  reportPreview();
  if (open) els.sheet.focus({ preventScroll: true });
  else els.openSheet.focus({ preventScroll: true });
}

function openSheet() {
  if (sheetOpen) return;
  try {
    history.pushState({ sheet: true }, "");
  } catch {
    /* no history: Done and the scrim still close it */
  }
  setSheet(true);
}

function closeSheet() {
  if (!sheetOpen) return;
  if (history.state && history.state.sheet) history.back();
  else setSheet(false);
}

window.addEventListener("popstate", () => setSheet(Boolean(history.state && history.state.sheet)));
els.openSheet.addEventListener("click", openSheet);
els.closeSheet.addEventListener("click", closeSheet);
els.scrim.addEventListener("click", closeSheet);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeSheet();
});

// ------------------------------------------------------------------ actions
els.toggle.addEventListener("click", async () => {
  els.toggle.disabled = true;
  showError("");
  try {
    if (running) {
      await invoke("stop");
    } else {
      await invoke("start", {
        config: {
          transport: settings.transport,
          port: settings.port,
          source: settings.source,
          keepScreenOn: settings.keepScreenOn,
          gain: settings.gain / 100,
          muted: settings.muted,
          allowUnauthenticated: settings.allowUnauthenticated,
          camera: cameraConfig(),
        },
      });
    }
  } catch (e) {
    showError(String(e));
  } finally {
    els.toggle.disabled = false;
  }
});

els.mute.addEventListener("click", () => {
  settings.muted = !settings.muted;
  save();
  renderMute();
  invoke("set_muted", { muted: settings.muted });
});

els.gain.addEventListener("input", () => {
  settings.gain = Number(els.gain.value);
  save();
  renderGain();
  invoke("set_gain", { gain: settings.gain / 100 });
});

transportInputs.forEach((input) =>
  input.addEventListener("change", () => {
    settings.transport = input.value;
    save();
    renderHint();
    renderControls();
    renderCamera(camera);
    renderSubstatus();
  }),
);

els.port.addEventListener("change", () => {
  const port = Number(els.port.value);
  if (Number.isInteger(port) && port > 0 && port < 65536) {
    settings.port = port;
    save();
  }
  els.port.value = settings.port;
  renderHint();
});

els.source.addEventListener("change", () => {
  settings.source = els.source.value;
  save();
  syncSelect(els.source);
  invoke("set_source", { source: settings.source });
});

els.keepScreen.addEventListener("change", () => {
  settings.keepScreenOn = els.keepScreen.checked;
  save();
  invoke("set_keep_screen_on", { on: settings.keepScreenOn });
});

els.camera.addEventListener("change", () => {
  settings.camera = els.camera.checked;
  save();
  renderHint();
  renderControls();
  renderCamera(camera);
  applyCamera();
});

function renderLens() {
  els.lens.value = settings.lens;
  syncSelect(els.lens);
  els.flip.setAttribute(
    "aria-label",
    settings.lens === "front" ? "Switch to the back camera" : "Switch to the front camera",
  );
}

els.flip.addEventListener("click", () => {
  settings.lens = settings.lens === "front" ? "back" : "front";
  save();
  renderLens();
  if (cameraOn()) applyCamera();
});

for (const key of ["lens", "resolution", "orientation"]) {
  els[key].addEventListener("change", () => {
    settings[key] = key === "resolution" ? Number(els[key].value) : els[key].value;
    save();
    syncSelect(els[key]);
    renderLens();
    if (cameraOn()) applyCamera();
  });
}

els.allowUnauth.addEventListener("change", () => {
  settings.allowUnauthenticated = els.allowUnauth.checked;
  save();
  renderInsecure();
});

els.newCode.addEventListener("click", async () => {
  els.newCode.disabled = true;
  showError("");
  try {
    token = await invoke("reset_token");
    renderToken();
    renderHint();
  } catch (e) {
    showError(String(e));
  } finally {
    els.newCode.disabled = running;
  }
});

async function loadToken() {
  try {
    token = await invoke("get_token");
  } catch (e) {
    showError(String(e));
  }
  renderToken();
  renderHint();
}

els.cameraSettings.addEventListener("click", () => {
  invoke("open_app_settings").catch((e) => showError(String(e)));
});

// ------------------------------------------------------------------ init
transportInputs.forEach((i) => (i.checked = i.value === settings.transport));
els.port.value = settings.port;
els.source.value = settings.source;
els.keepScreen.checked = settings.keepScreenOn;
els.camera.checked = settings.camera;
renderLens();
els.resolution.value = String(settings.resolution);
els.orientation.value = settings.orientation;
[els.source, els.resolution, els.orientation].forEach(syncSelect);
renderMute();
renderGain();
renderInsecure();
renderToken();
renderHint();
loadToken();
renderControls();
renderCamera(camera);
renderSubstatus();

// Screenshot / deep-link hook: index.html?panel=open starts with settings open.
if (new URLSearchParams(location.search).get("panel") === "open") {
  document.body.classList.add("no-motion");
  setSheet(true);
  requestAnimationFrame(() => requestAnimationFrame(() => document.body.classList.remove("no-motion")));
}

// One status request at a time: overlapping IPC calls pile up when the
// WebView is busy. The preview rect is re-sent too (deduplicated), since the
// home layout can shift without #preview itself resizing.
(async function loop() {
  await poll();
  reportPreview();
  setTimeout(loop, 150);
})();
