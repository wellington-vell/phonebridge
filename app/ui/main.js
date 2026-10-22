const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const els = {
  status: $("status"),
  mute: $("mute"),
  muteLabel: $("mute-label"),
  level: $("level"),
  gain: $("gain"),
  gainValue: $("gain-value"),
  toggle: $("toggle"),
  error: $("error"),
  port: $("port"),
  source: $("source"),
  keepScreen: $("keep-screen"),
  hint: $("hint"),
  camera: $("camera"),
  cameraState: $("camera-state"),
  cameraNote: $("camera-note"),
  cameraError: $("camera-error"),
  lens: $("lens"),
  resolution: $("resolution"),
  orientation: $("orientation"),
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

// ------------------------------------------------------------------ rendering
let running = false;
let addresses = [];

function renderMute() {
  els.mute.setAttribute("aria-pressed", String(settings.muted));
  els.mute.setAttribute("aria-label", settings.muted ? "Unmute" : "Mute");
  els.muteLabel.textContent = settings.muted ? "Muted" : "Live";
}

function renderGain() {
  els.gain.value = settings.gain;
  els.gainValue.textContent = `${settings.gain}%`;
}

// Video is too slow over Bluetooth.
const cameraOn = () => settings.camera && settings.transport !== "bluetooth";

function renderHint() {
  const port = settings.port;
  const portArg = (port === 8125 ? "" : ` --port ${port}`) + (cameraOn() ? " --camera" : "");
  let html;
  switch (settings.transport) {
    case "usb":
      html = `<p>Enable USB debugging, plug the phone in and run on the PC:</p>
        <code>phonebridge${portArg}</code>`;
      break;
    case "wifi": {
      const ips = addresses.length ? addresses.map((a) => a.ip) : ["PHONE_IP"];
      html = `<p>Same network as the PC. Run on the PC:</p>
        ${ips.map((ip) => `<code>phonebridge --wifi ${ip}${portArg}</code>`).join("")}`;
      break;
    }
    case "bluetooth":
      html = `<p>Pair the phone with the PC first, then run (phone address from
        <em>bluetoothctl devices</em>):</p>
        <code>phonebridge --bluetooth AA:BB:CC:DD:EE:FF</code>`;
      break;
  }
  els.hint.innerHTML = html;
}

function renderControls() {
  els.toggle.textContent = running ? "Stop" : "Start";
  els.toggle.classList.toggle("running", running);
  // Transport and port only apply when (re)starting.
  els.port.disabled = running;
  transportInputs.forEach((i) => (i.disabled = running));
  // Camera settings too; the camera is USB / Wi-Fi only.
  const bluetooth = settings.transport === "bluetooth";
  els.camera.disabled = running || bluetooth;
  [els.lens, els.resolution, els.orientation].forEach((el) => (el.disabled = running || bluetooth));
  els.cameraNote.hidden = !bluetooth;
}

const CAMERA_LABEL = {
  off: "Off",
  listening: "Waiting for PC",
  streaming: "Streaming",
};

function renderCamera(cam) {
  const state = cam && CAMERA_LABEL[cam.state] ? cam.state : "off";
  let label = CAMERA_LABEL[state];
  if (state === "streaming" && cam.width) label += ` ${cam.width}×${cam.height}`;
  els.cameraState.textContent = label;
  els.cameraState.dataset.state = state;
  els.cameraError.hidden = !cam?.error;
  els.cameraError.textContent = cam?.error || "";
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
    const label = s.state === "connected" && s.peer ? `Connected · ${s.peer}` : STATE_LABEL[s.state];
    els.status.textContent = label;
    els.status.dataset.state = s.state;
    els.level.style.width = `${Math.min(100, Math.sqrt(s.level) * 100)}%`;
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
          camera: cameraOn()
            ? {
                lens: settings.lens,
                ...(RESOLUTIONS[settings.resolution] || RESOLUTIONS[720]),
                fps: CAMERA_FPS,
                orientation: settings.orientation,
              }
            : null,
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
});

for (const key of ["lens", "resolution", "orientation"]) {
  els[key].addEventListener("change", () => {
    settings[key] = key === "resolution" ? Number(els[key].value) : els[key].value;
    save();
  });
}

// ------------------------------------------------------------------ init
transportInputs.forEach((i) => (i.checked = i.value === settings.transport));
els.port.value = settings.port;
els.source.value = settings.source;
els.keepScreen.checked = settings.keepScreenOn;
els.camera.checked = settings.camera;
els.lens.value = settings.lens;
els.resolution.value = String(settings.resolution);
els.orientation.value = settings.orientation;
renderMute();
renderGain();
renderHint();
renderControls();

// One status request at a time: overlapping IPC calls pile up when the
// WebView is busy.
(async function loop() {
  await poll();
  setTimeout(loop, 150);
})();
