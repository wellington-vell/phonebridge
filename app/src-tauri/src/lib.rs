//! Phonebridge for Android: turns the phone into a microphone for the Phonebridge Linux
//! client (and anything else speaking the WO Mic protocol) over USB, Wi-Fi
//! or Bluetooth, and into a webcam (H.264 on control port + 1) over USB or
//! Wi-Fi, with a live camera preview on the phone.

mod capture;
mod encoder;
mod net;
mod platform;
mod secure;
mod server;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager, State};

use capture::Source;
use platform::Platform;
use server::{Auth, Shared, Status, Transport};

struct Worker {
    thread: JoinHandle<()>,
    transport: Transport,
    port: u16,
    /// Pairing code the servers were started with, display form.
    token: String,
    allow_unauthenticated: bool,
}

struct App {
    shared: Arc<Shared>,
    worker: Mutex<Option<Worker>>,
    keep_screen_on: Arc<AtomicBool>,
    /// Microphone clients currently connected (a replaced one may report
    /// its end after the new one started).
    mic_clients: Arc<AtomicUsize>,
    /// Camera sharing was started (CameraStreamer exists on the Kotlin side).
    camera_started: AtomicBool,
    /// Why camera sharing could not be turned on. Also held while the camera
    /// is being switched, so switches never overlap.
    camera_error: Mutex<Option<String>>,
    /// Pairing code on desktop, where there is no Android storage: random
    /// per run.
    desktop_token: Mutex<Option<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartConfig {
    transport: Transport,
    port: u16,
    source: Source,
    keep_screen_on: bool,
    gain: f32,
    muted: bool,
    /// Also accept clients without the pairing code (plaintext, e.g. the
    /// official WO Mic client).
    #[serde(default)]
    allow_unauthenticated: bool,
    /// Camera sharing, or None when off. Can change while running
    /// (set_camera).
    camera: Option<CameraConfig>,
}

/// Camera sharing settings, passed through to CameraStreamer.kt.
#[derive(Deserialize)]
pub struct CameraConfig {
    /// "back" or "front".
    pub lens: String,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    /// "landscape" or "portrait": how the phone is held, for the rotation
    /// the PC applies.
    pub orientation: String,
    /// Virtual background the PC puts behind the person: "" off, a built-in
    /// one ("studio", "office", "daylight", "fireplace") or "custom" (the
    /// user's photo).
    #[serde(default)]
    pub background: String,
}

#[derive(Deserialize, Serialize)]
pub struct PreviewRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Corner radius, physical pixels.
    pub radius: f32,
    pub visible: bool,
}

#[derive(Serialize)]
struct Address {
    interface: String,
    ip: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusReply {
    #[serde(flatten)]
    status: Status,
    level: f32,
    muted: bool,
    gain: f32,
    transport: Option<Transport>,
    addresses: Vec<Address>,
    /// From CameraStreamer.kt: {state, peer?, error?, width?, height?,
    /// preview, permission}, with the error of the last switch.
    camera: Value,
}

fn stop_worker(app: &AppHandle, state: &App) {
    let Some(worker) = state.worker.lock().unwrap().take() else {
        return;
    };
    let platform = app.state::<Platform>();
    if worker.transport == Transport::Bluetooth {
        platform.stop_bluetooth();
    }
    platform.stop_camera();
    state.camera_started.store(false, Ordering::Relaxed);
    state.shared.stop.store(true, Ordering::Relaxed);
    let _ = worker.thread.join();
    state.mic_clients.store(0, Ordering::Relaxed);
    platform.keep_screen_on(false, false);
    platform.service(false, false);
    *state.camera_error.lock().unwrap() = None;
}

/// Turn camera sharing on (or restart it with new settings) or off while
/// running, without touching the audio stream. Does nothing when stopped:
/// the settings then apply on start. A failure is also kept for the UI.
fn apply_camera(app: &AppHandle, state: &App, cam: Option<&CameraConfig>) -> Result<(), String> {
    let mut error = state.camera_error.lock().unwrap();
    let result = switch_camera(app, state, cam);
    if let Err(e) = &result {
        eprintln!("[phonebridge] camera: {e}");
    }
    *error = result.as_ref().err().cloned();
    result
}

fn switch_camera(app: &AppHandle, state: &App, cam: Option<&CameraConfig>) -> Result<(), String> {
    let Some((transport, port, token, allow_unauthenticated)) =
        state.worker.lock().unwrap().as_ref().map(|w| {
            (
                w.transport,
                w.port,
                w.token.clone(),
                w.allow_unauthenticated,
            )
        })
    else {
        return Ok(());
    };
    let platform = app.state::<Platform>();
    platform.stop_camera();
    state.camera_started.store(false, Ordering::Relaxed);
    let Some(cam) = cam else {
        // Drop the camera from the foreground service types.
        platform.service(true, false);
        return Ok(());
    };
    if transport == Transport::Bluetooth {
        // Too slow for video.
        return Err("camera sharing needs USB or Wi-Fi".into());
    }
    let video_port = port
        .checked_add(1)
        .ok_or("the control port must be below 65535 to share the camera")?;
    platform.request_camera_permission()?;
    // The service needs the camera type to keep the camera with the screen
    // off or the app in the background.
    platform.service(true, true);
    platform.start_camera(
        video_port,
        transport == Transport::Usb,
        &token,
        allow_unauthenticated,
        cam,
    )?;
    state.camera_started.store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
async fn start(app: AppHandle, state: State<'_, App>, config: StartConfig) -> Result<(), String> {
    stop_worker(&app, &state);
    if config.port == 0 {
        return Err("invalid control port".into());
    }
    let platform = app.state::<Platform>();
    // The camera's permission is asked for by apply_camera: refusing it only
    // keeps the camera off, the microphone still starts.
    platform.request_permissions(config.transport == Transport::Bluetooth)?;
    let token = pairing_code(&app, &state, false)?;
    let auth = Auth {
        code: secure::canonical(&token).ok_or("invalid pairing code")?,
        allow_unauthenticated: config.allow_unauthenticated,
    };

    let shared = &state.shared;
    shared.stop.store(false, Ordering::Relaxed);
    shared.set_gain(config.gain);
    shared.muted.store(config.muted, Ordering::Relaxed);
    *shared.source.lock().unwrap() = config.source;
    state
        .keep_screen_on
        .store(config.keep_screen_on, Ordering::Relaxed);
    state.mic_clients.store(0, Ordering::Relaxed);
    // The camera side keeps the screen on by itself while a PC watches,
    // so it needs the setting even if no microphone client ever connects.
    platform.keep_screen_on(false, config.keep_screen_on);
    shared.status.lock().unwrap().error = None;

    platform.service(true, false);
    let thread = {
        let shared = Arc::clone(shared);
        let keep = Arc::clone(&state.keep_screen_on);
        let clients = Arc::clone(&state.mic_clients);
        let app = app.clone();
        let (transport, port) = (config.transport, config.port);
        thread::Builder::new()
            .name("phonebridge-server".into())
            .spawn(move || {
                let on_connected = move |connected: bool| {
                    let n = if connected {
                        clients.fetch_add(1, Ordering::Relaxed) + 1
                    } else {
                        clients
                            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                                Some(n.saturating_sub(1))
                            })
                            .map_or(0, |n| n.saturating_sub(1))
                    };
                    app.state::<Platform>()
                        .keep_screen_on(n > 0, keep.load(Ordering::Relaxed));
                };
                server::serve(transport, port, auth, shared, Arc::new(on_connected))
            })
            .map_err(|e| e.to_string())?
    };
    *state.worker.lock().unwrap() = Some(Worker {
        thread,
        transport: config.transport,
        port: config.port,
        token,
        allow_unauthenticated: config.allow_unauthenticated,
    });

    if config.transport == Transport::Bluetooth
        && let Err(e) = platform.start_bluetooth(config.port)
    {
        stop_worker(&app, &state);
        return Err(e);
    }
    if let Some(cam) = &config.camera {
        // Shown in the camera section; the microphone is up regardless.
        let _ = apply_camera(&app, &state, Some(cam));
    }
    Ok(())
}

/// The pairing code from the app's storage (or this run's on desktop),
/// display form; `reset` replaces it with a new one first.
fn pairing_code(app: &AppHandle, state: &App, reset: bool) -> Result<String, String> {
    let platform = app.state::<Platform>();
    let stored = if reset {
        platform.reset_token()?
    } else {
        platform.get_token()?
    };
    let token = match stored {
        Some(t) => t,
        None => {
            let mut t = state.desktop_token.lock().unwrap();
            if reset || t.is_none() {
                *t = Some(secure::generate_code().map_err(|e| e.to_string())?);
            }
            t.clone().unwrap()
        }
    };
    let canonical = secure::canonical(&token).ok_or("invalid pairing code")?;
    Ok(secure::display(&canonical))
}

/// The pairing code the PC needs (`phonebridge --token`).
#[tauri::command]
async fn get_token(app: AppHandle, state: State<'_, App>) -> Result<String, String> {
    pairing_code(&app, &state, false)
}

/// Make a new pairing code, so PCs with the old one can no longer connect.
/// Refused while running: the servers keep the code they started with.
#[tauri::command]
async fn reset_token(app: AppHandle, state: State<'_, App>) -> Result<String, String> {
    if state.worker.lock().unwrap().is_some() {
        return Err("stop before making a new pairing code".into());
    }
    pairing_code(&app, &state, true)
}

/// Turn camera sharing on/off or change its settings while running.
#[tauri::command]
async fn set_camera(
    app: AppHandle,
    state: State<'_, App>,
    camera: Option<CameraConfig>,
) -> Result<(), String> {
    apply_camera(&app, &state, camera.as_ref())
}

/// Where the page's preview placeholder is, in physical pixels relative to
/// the WebView, and whether the live preview should show there.
#[tauri::command]
async fn set_preview_rect(app: AppHandle, rect: PreviewRect) -> Result<(), String> {
    app.state::<Platform>().set_preview_rect(&rect);
    Ok(())
}

/// {chosen, thumbnail?}: whether the user chose a photo of their own, and a
/// small data URL of it for the picker.
#[tauri::command]
async fn background_status(app: AppHandle) -> Result<Value, String> {
    app.state::<Platform>().background_status()
}

/// Choose the virtual background picture; false if cancelled. Takes effect
/// on the next camera (re)start.
#[tauri::command]
async fn pick_background(app: AppHandle) -> Result<bool, String> {
    app.state::<Platform>().pick_background()
}

#[tauri::command]
async fn clear_background(app: AppHandle) -> Result<(), String> {
    app.state::<Platform>().clear_background()
}

/// The app's system settings page, to grant a refused permission.
#[tauri::command]
async fn open_app_settings(app: AppHandle) -> Result<(), String> {
    app.state::<Platform>().open_app_settings()
}

#[tauri::command]
async fn stop(app: AppHandle, state: State<'_, App>) -> Result<(), String> {
    stop_worker(&app, &state);
    Ok(())
}

#[tauri::command]
fn set_gain(state: State<'_, App>, gain: f32) {
    state.shared.set_gain(gain);
}

#[tauri::command]
fn set_muted(state: State<'_, App>, muted: bool) {
    state.shared.muted.store(muted, Ordering::Relaxed);
}

#[tauri::command]
fn set_source(state: State<'_, App>, source: Source) {
    *state.shared.source.lock().unwrap() = source;
}

#[tauri::command]
fn set_keep_screen_on(app: AppHandle, state: State<'_, App>, on: bool) {
    state.keep_screen_on.store(on, Ordering::Relaxed);
    if state.worker.lock().unwrap().is_some() {
        let mic = state.mic_clients.load(Ordering::Relaxed) > 0;
        app.state::<Platform>().keep_screen_on(mic, on);
    }
}

/// Async so the Android plugin call for the camera status runs off the main
/// thread.
#[tauri::command]
async fn status(app: AppHandle, state: State<'_, App>) -> Result<StatusReply, String> {
    let shared = &state.shared;
    Ok(StatusReply {
        status: shared.status(),
        level: shared.level(),
        muted: shared.muted.load(Ordering::Relaxed),
        gain: shared.gain(),
        transport: state.worker.lock().unwrap().as_ref().map(|w| w.transport),
        addresses: net::addresses()
            .into_iter()
            .map(|(interface, ip)| Address { interface, ip })
            .collect(),
        camera: camera_status(&app, &state),
    })
}

fn camera_status(app: &AppHandle, state: &App) -> Value {
    // Busy while switching (maybe waiting on the permission prompt).
    let Ok(error) = state.camera_error.try_lock() else {
        return serde_json::json!({ "state": "starting" });
    };
    let error = error.clone();
    // Skip the plugin call (one per UI poll) while there is no camera to ask
    // about; with an error it still tells whether the permission is missing.
    if error.is_none() && !state.camera_started.load(Ordering::Relaxed) {
        return serde_json::json!({ "state": "off" });
    }
    let mut camera = app.state::<Platform>().camera_status();
    if let (Some(e), Some(obj)) = (error, camera.as_object_mut()) {
        obj.insert("error".into(), e.into());
        if obj.get("state").and_then(Value::as_str) == Some("off") {
            obj.insert("state".into(), "error".into());
        }
    }
    camera
}

fn platform_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("phonebridge")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            app.manage(Platform(_api.register_android_plugin(
                "io.github.wellingtonvell.phonebridge",
                "PhonebridgePlugin",
            )?));
            #[cfg(not(target_os = "android"))]
            app.manage(Platform);
            Ok(())
        })
        .build()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(platform_plugin())
        .manage(App {
            shared: Arc::new(Shared::new()),
            worker: Mutex::new(None),
            keep_screen_on: Arc::new(AtomicBool::new(true)),
            mic_clients: Arc::new(AtomicUsize::new(0)),
            camera_started: AtomicBool::new(false),
            camera_error: Mutex::new(None),
            desktop_token: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            start,
            stop,
            set_gain,
            set_muted,
            set_source,
            set_keep_screen_on,
            set_camera,
            set_preview_rect,
            open_app_settings,
            background_status,
            pick_background,
            clear_background,
            get_token,
            reset_token,
            status
        ])
        .build(tauri::generate_context!())
        .expect("error while building the app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                stop_worker(app, &app.state::<App>());
            }
        });
}
