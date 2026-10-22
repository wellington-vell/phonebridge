//! Phonebridge for Android: turns the phone into a microphone for the Phonebridge Linux
//! client (and anything else speaking the WO Mic protocol) over USB, Wi-Fi
//! or Bluetooth, and into a webcam (H.264 on control port + 1) over USB or
//! Wi-Fi.

mod capture;
mod encoder;
mod net;
mod platform;
mod server;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager, State};

use capture::Source;
use platform::Platform;
use server::{Shared, Status, Transport};

struct Worker {
    thread: JoinHandle<()>,
    transport: Transport,
}

struct App {
    shared: Arc<Shared>,
    worker: Mutex<Option<Worker>>,
    keep_screen_on: Arc<AtomicBool>,
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
    /// Camera sharing, or None when off.
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
    /// From CameraStreamer.kt: {state, peer?, error?, width?, height?}.
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
    state.shared.stop.store(true, Ordering::Relaxed);
    let _ = worker.thread.join();
    platform.keep_screen_on(false);
    platform.service(false, false);
}

#[tauri::command]
async fn start(app: AppHandle, state: State<'_, App>, config: StartConfig) -> Result<(), String> {
    stop_worker(&app, &state);
    if config.port == 0 {
        return Err("invalid control port".into());
    }
    let camera_port = match &config.camera {
        None => None,
        // Too slow for video.
        Some(_) if config.transport == Transport::Bluetooth => {
            return Err("camera sharing needs USB or Wi-Fi".into());
        }
        Some(_) => Some(
            config
                .port
                .checked_add(1)
                .ok_or("the control port must be below 65535 to share the camera")?,
        ),
    };
    let platform = app.state::<Platform>();
    platform.request_permissions(
        config.transport == Transport::Bluetooth,
        config.camera.is_some(),
    )?;

    let shared = &state.shared;
    shared.stop.store(false, Ordering::Relaxed);
    shared.set_gain(config.gain);
    shared.muted.store(config.muted, Ordering::Relaxed);
    *shared.source.lock().unwrap() = config.source;
    state
        .keep_screen_on
        .store(config.keep_screen_on, Ordering::Relaxed);
    shared.status.lock().unwrap().error = None;

    platform.service(true, config.camera.is_some());
    let thread = {
        let shared = Arc::clone(shared);
        let keep = Arc::clone(&state.keep_screen_on);
        let app = app.clone();
        let (transport, port) = (config.transport, config.port);
        thread::Builder::new()
            .name("phonebridge-server".into())
            .spawn(move || {
                let on_connected = move |connected: bool| {
                    if !connected || keep.load(Ordering::Relaxed) {
                        app.state::<Platform>().keep_screen_on(connected);
                    }
                };
                server::serve(transport, port, shared, Arc::new(on_connected))
            })
            .map_err(|e| e.to_string())?
    };
    *state.worker.lock().unwrap() = Some(Worker {
        thread,
        transport: config.transport,
    });

    if config.transport == Transport::Bluetooth
        && let Err(e) = platform.start_bluetooth(config.port)
    {
        stop_worker(&app, &state);
        return Err(e);
    }
    if let (Some(cam), Some(port)) = (&config.camera, camera_port)
        && let Err(e) = platform.start_camera(port, config.transport == Transport::Usb, cam)
    {
        stop_worker(&app, &state);
        return Err(e);
    }
    Ok(())
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
    if state.shared.status().state == server::State::Connected {
        app.state::<Platform>().keep_screen_on(on);
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
        camera: app.state::<Platform>().camera_status(),
    })
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
        })
        .invoke_handler(tauri::generate_handler![
            start,
            stop,
            set_gain,
            set_muted,
            set_source,
            set_keep_screen_on,
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
