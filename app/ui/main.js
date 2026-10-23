const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const els = {
  app: $("app"),
  status: $("status"),
  mute: $("mute"),
  muteShape: $("mute-shape"),
  muteLabel: $("mute-label"),
  muteHint: $("mute-hint"),
  level: $("level"),
  gain: $("gain"),
  gainValue: $("gain-value"),
  toggle: $("toggle"),
  toggleLabel: $("toggle-label"),
  error: $("error"),
  port: $("port"),
  keepScreen: $("keep-screen"),
  hint: $("hint"),
  token: $("token"),
  newCode: $("new-code"),
  allowUnauth: $("allow-unauth"),
  insecureNote: $("insecure-note"),
  navBadge: $("nav-badge"),
  stage: $("stage"),
  camera: $("camera"),
  cameraState: $("camera-state"),
  cameraNote: $("camera-note"),
  cameraProblem: $("camera-problem"),
  cameraError: $("camera-error"),
  cameraSettings: $("camera-settings"),
  preview: $("preview"),
  previewText: $("preview-text"),
  flip: $("flip"),
  scroll: $("scroll"),
  sumTransport: $("sum-transport"),
  sumTransportSub: $("sum-transport-sub"),
  sumTransportIcon: $("sum-transport-icon"),
  sumToken: $("sum-token"),
  sumSource: $("sum-source"),
};

/** Radio groups (segmented buttons, chips, radio lists) by input name. */
const radios = (name) => [...document.querySelectorAll(`input[name="${name}"]`)];
const groups = {
  transport: radios("transport"),
  lens: radios("lens"),
  resolution: radios("resolution"),
  orientation: radios("orientation"),
  source: radios("source"),
};
function setRadio(name, value) {
  groups[name].forEach((i) => (i.checked = i.value === String(value)));
}

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

// ------------------------------------------------------------------ navigation
// Home / Camera / Connection / Audio. The stage (camera preview) belongs to
// Home and Camera only; elsewhere the native preview is told to hide.
const TABS = ["home", "camera", "connection", "audio"];
const navButtons = [...document.querySelectorAll(".nav button")];
const panels = [...document.querySelectorAll("[data-panel]")];
let tab = "home";

const stageShown = () => tab === "home" || tab === "camera";

function setTab(next) {
  if (!TABS.includes(next)) next = "home";
  tab = next;
  els.app.dataset.tab = tab;
  panels.forEach((p) => {
    p.hidden = p.dataset.panel !== tab;
    p.classList.remove("enter");
    // Expressive entrance on user navigation only.
    if (!p.hidden && document.body.classList.contains("ready")) {
      void p.offsetWidth;
      p.classList.add("enter");
    }
  });
  navButtons.forEach((b) =>
    b.dataset.tab === tab ? b.setAttribute("aria-current", "page") : b.removeAttribute("aria-current"),
  );
  els.stage.hidden = !stageShown();
  els.scroll.scrollTop = 0;
  reportPreview();
}

navButtons.forEach((b) => b.addEventListener("click", () => setTab(b.dataset.tab)));
document.querySelectorAll("[data-goto]").forEach((b) =>
  b.addEventListener("click", () => setTab(b.dataset.goto)),
);

// ------------------------------------------------------------------ mic shape
// Expressive mute button: a "cookie" while live, morphing to a rounded
// square when muted (CSS transitions the path's `d`; same point count).
function shapePath(r) {
  const n = 96;
  let d = "";
  for (let k = 0; k < n; k++) {
    const t = (k / n) * Math.PI * 2;
    const rr = r(t);
    d += `${k ? "L" : "M"}${(50 + rr * Math.cos(t)).toFixed(2)} ${(50 + rr * Math.sin(t)).toFixed(2)}`;
  }
  return `path("${d}Z")`;
}
els.muteShape.style.setProperty("--shape-live", shapePath((t) => 45 + 3.6 * Math.cos(9 * t)));
els.muteShape.style.setProperty(
  "--shape-muted",
  shapePath((t) => 46 / Math.pow(Math.abs(Math.cos(t)) ** 5 + Math.abs(Math.sin(t)) ** 5, 1 / 5)),
);

// ------------------------------------------------------------------ rendering
let running = false;
let addresses = [];
// Pairing code, display form ("ABCDE-FGHJK-MNPQR-STVWX"); "" until loaded.
let token = "";

function renderMute() {
  els.mute.setAttribute("aria-pressed", String(settings.muted));
  els.mute.setAttribute("aria-label", settings.muted ? "Unmute" : "Mute");
  els.muteLabel.textContent = settings.muted ? "Muted" : "Live";
  els.muteHint.textContent = settings.muted ? "Tap the mic to unmute" : "Tap the mic to mute";
}

function renderGain() {
  els.gain.value = settings.gain;
  els.gain.style.setProperty("--pct", `${(settings.gain / 400) * 100}%`);
  els.gainValue.textContent = `${settings.gain}%`;
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

const TRANSPORT = {
  usb: { name: "USB", icon: "#i-usb" },
  wifi: { name: "Wi‑Fi", icon: "#i-wifi" },
  bluetooth: { name: "Bluetooth", icon: "#i-bt" },
};

function renderHint() {
  const port = settings.port;
  const portArg = (port === 8125 ? "" : ` --port ${port}`) + (cameraOn() ? " --camera" : "");
  const code = token || "PAIRING-CODE";
  const file = "~/.config/phonebridge/token";
  const nodes = [
    el("p", "Save the pairing code on the PC once:"),
    el("code", `echo ${code} | install -Dm600 /dev/stdin ${file}`),
  ];
  switch (settings.transport) {
    case "usb":
      nodes.push(
        el("p", "Enable USB debugging, plug the phone in and run on the PC:"),
        el("code", `phonebridge${portArg}`),
      );
      break;
    case "wifi": {
      const ips = addresses.length ? addresses.map((a) => a.ip) : ["PHONE_IP"];
      nodes.push(
        el("p", "Same network as the PC. Run on the PC:"),
        ...ips.map((ip) => el("code", `phonebridge --wifi ${ip}${portArg}`)),
      );
      break;
    }
    case "bluetooth":
      nodes.push(
        el(
          "p",
          "Pair the phone with the PC first, then run (phone address from ",
          el("em", "bluetoothctl devices"),
          "):",
        ),
        el("code", "phonebridge --bluetooth AA:BB:CC:DD:EE:FF"),
      );
      break;
  }
  nodes.push(el("p", "Or pass the code with --token, or in PHONEBRIDGE_TOKEN."));
  els.hint.replaceChildren(...nodes);
  renderSummary();
}

/** Home's quick-settings list. */
function renderSummary() {
  const t = TRANSPORT[settings.transport] || TRANSPORT.usb;
  els.sumTransport.textContent = t.name;
  els.sumTransportIcon.setAttribute("href", t.icon);
  let sub;
  if (settings.transport === "bluetooth") sub = "Microphone only · PC command";
  else if (settings.transport === "wifi" && addresses.length)
    sub = `${addresses.map((a) => a.ip).join(", ")} · port ${settings.port}`;
  else sub = `Port ${settings.port} · PC command`;
  els.sumTransportSub.textContent = sub;
  els.sumToken.textContent = token || "…";
  const src = groups.source.find((i) => i.value === settings.source);
  els.sumSource.textContent = src
    ? src.closest("label").querySelector(".headline").textContent
    : settings.source;
}

/** The code in large monospace groups. */
function renderToken() {
  if (!token) {
    els.token.textContent = "…";
  } else {
    const parts = [];
    token.split("-").forEach((g, i) => {
      if (i) parts.push(Object.assign(el("span", "-"), { className: "sep" }));
      parts.push(Object.assign(el("span", g), { className: "group" }));
    });
    els.token.replaceChildren(...parts);
  }
  renderSummary();
}

function renderInsecure() {
  els.allowUnauth.checked = settings.allowUnauthenticated;
  els.insecureNote.hidden = !settings.allowUnauthenticated;
  els.navBadge.hidden = !settings.allowUnauthenticated;
}

function renderControls() {
  els.toggleLabel.textContent = running ? "Stop" : "Start";
  els.toggle.classList.toggle("running", running);
  els.app.dataset.running = String(running);
  // Transport and port only apply when (re)starting.
  els.port.disabled = running;
  groups.transport.forEach((i) => (i.disabled = running));
  els.allowUnauth.disabled = running;
  // The servers keep the code they started with.
  els.newCode.disabled = running;
  els.newCode.title = running ? "Stop to make a new code" : "";
  // Camera settings apply live, but the camera is USB / Wi-Fi only.
  const bluetooth = settings.transport === "bluetooth";
  els.camera.disabled = bluetooth;
  els.flip.disabled = bluetooth;
  [...groups.lens, ...groups.resolution, ...groups.orientation].forEach(
    (i) => (i.disabled = bluetooth),
  );
  els.cameraNote.hidden = !(bluetooth && settings.camera);
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
  // A compact placeholder while sharing is off, full size otherwise.
  els.stage.dataset.camera = cameraOn() ? "on" : "off";
  previewVisible = open;
  reportPreview();
}

// --------------------------------------------------------------- preview
// The live preview is a native view over #preview (see CameraPreview.kt);
// tell it where #preview is, in physical pixels relative to the WebView.
let previewVisible = false;
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
    visible: previewVisible && stageShown() && r.width > 0 && r.height > 0,
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
// The preview's height animates when sharing is toggled; follow it to the end.
els.preview.addEventListener("transitionend", reportPreview);

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
  idle: "Stopped",
  listening: "Waiting for PC",
  connected: "Connected",
};

async function poll() {
  try {
    const s = await invoke("status");
    running = s.state !== "idle" || s.transport !== null;
    // The microphone and the camera have their own PC connections (the PC
    // runs them as separate instances): show both.
    const mic = s.state === "connected";
    const cam = s.camera && s.camera.state === "streaming";
    let label = STATE_LABEL[s.state];
    if (mic && cam) label = "Connected · mic + camera";
    else if (cam) label = "Connected · camera";
    else if (mic && s.peer) label = `Connected · ${s.peer}`;
    els.status.textContent = label;
    els.status.dataset.state = mic || cam ? "connected" : s.state;
    const level = Math.min(1, Math.sqrt(s.level));
    els.level.style.width = `${level * 100}%`;
    els.mute.style.setProperty("--level", level.toFixed(3));
    showError(s.error);
    renderCamera(s.camera);
    const ips = JSON.stringify(s.addresses);
    if (ips !== JSON.stringify(addresses)) {
      addresses = s.addresses;
      renderHint();
    }
    renderControls();
  } catch (e) {
    showError(String(e));
  }
}

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

groups.transport.forEach((input) =>
  input.addEventListener("change", () => {
    settings.transport = input.value;
    save();
    renderHint();
    renderControls();
    renderCamera(camera);
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

groups.source.forEach((input) =>
  input.addEventListener("change", () => {
    settings.source = input.value;
    save();
    renderSummary();
    invoke("set_source", { source: settings.source });
  }),
);

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
  setRadio("lens", settings.lens);
  els.flip.setAttribute(
    "aria-label",
    settings.lens === "front" ? "Switch to the back camera" : "Switch to the front camera",
  );
  els.flip.dataset.lens = settings.lens;
}

els.flip.addEventListener("click", () => {
  settings.lens = settings.lens === "front" ? "back" : "front";
  save();
  renderLens();
  if (cameraOn()) applyCamera();
});

for (const key of ["lens", "resolution", "orientation"]) {
  groups[key].forEach((input) =>
    input.addEventListener("change", () => {
      settings[key] = key === "resolution" ? Number(input.value) : input.value;
      save();
      renderLens();
      if (cameraOn()) applyCamera();
    }),
  );
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
setRadio("transport", settings.transport);
els.port.value = settings.port;
setRadio("source", settings.source);
els.keepScreen.checked = settings.keepScreenOn;
els.camera.checked = settings.camera;
renderLens();
setRadio("resolution", settings.resolution);
setRadio("orientation", settings.orientation);
renderMute();
renderGain();
renderInsecure();
renderToken();
renderHint();
loadToken();
renderControls();
renderCamera(camera);
// Deep link to a destination, e.g. index.html#/connection.
setTab(location.hash.replace(/^#\/?/, ""));
// Enable transitions only after the first paint (no animation on load).
requestAnimationFrame(() => requestAnimationFrame(() => document.body.classList.add("ready")));

// One status request at a time: overlapping IPC calls pile up when the
// WebView is busy.
(async function loop() {
  await poll();
  setTimeout(loop, 150);
})();
