//! Android side effects, implemented in Kotlin (PhonebridgePlugin.kt): permissions,
//! keep-screen-on, the foreground service, the Bluetooth RFCOMM bridge and
//! camera sharing (CameraStreamer.kt). No-ops on desktop, which has no camera.

use serde_json::{Value, json};

use crate::CameraConfig;

#[cfg(target_os = "android")]
pub struct Platform(pub tauri::plugin::PluginHandle<tauri::Wry>);
#[cfg(not(target_os = "android"))]
pub struct Platform;

impl Platform {
    #[cfg(target_os = "android")]
    fn call(&self, cmd: &str, args: Value) -> Result<Value, String> {
        self.0
            .run_mobile_plugin::<Value>(cmd, args)
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "android"))]
    fn call(&self, _cmd: &str, _args: Value) -> Result<Value, String> {
        Ok(Value::Null)
    }

    /// Ask for the runtime permissions; fails if the microphone (or the
    /// camera, when sharing it) is refused.
    pub fn request_permissions(&self, bluetooth: bool, camera: bool) -> Result<(), String> {
        let mut aliases = vec!["microphone", "notifications"];
        if bluetooth {
            aliases.push("bluetooth");
        }
        if camera {
            aliases.push("camera");
        }
        let states = self.call("requestPermissions", json!({ "permissions": aliases }))?;
        let granted = |alias| {
            matches!(
                states.get(alias).and_then(Value::as_str),
                None | Some("granted")
            )
        };
        if !granted("microphone") {
            return Err("microphone permission denied".into());
        }
        if camera && !granted("camera") {
            return Err("camera permission denied".into());
        }
        Ok(())
    }

    pub fn keep_screen_on(&self, on: bool) {
        let _ = self.call("keepScreenOn", json!({ "on": on }));
    }

    /// Foreground service so capture survives the screen turning off. It
    /// claims the camera type up front: the camera opens later, when a PC
    /// connects, possibly with the app in the background.
    pub fn service(&self, running: bool, camera: bool) {
        let cmd = if running {
            "startService"
        } else {
            "stopService"
        };
        if let Err(e) = self.call(cmd, json!({ "camera": camera })) {
            eprintln!("[phonebridge] {cmd}: {e}");
        }
    }

    /// RFCOMM server that relays each Bluetooth client to 127.0.0.1:`port`.
    pub fn start_bluetooth(&self, port: u16) -> Result<(), String> {
        self.call("startBluetooth", json!({ "port": port }))
            .map(drop)
    }

    pub fn stop_bluetooth(&self) {
        let _ = self.call("stopBluetooth", json!({}));
    }

    /// H.264 camera server on `port`; the camera itself opens only while a
    /// PC is connected.
    pub fn start_camera(
        &self,
        port: u16,
        local_only: bool,
        cam: &CameraConfig,
    ) -> Result<(), String> {
        let args = json!({
            "port": port,
            "localOnly": local_only,
            "lens": cam.lens,
            "width": cam.width,
            "height": cam.height,
            "fps": cam.fps,
            "orientation": cam.orientation,
        });
        self.call("startCamera", args).map(drop)
    }

    pub fn stop_camera(&self) {
        let _ = self.call("stopCamera", json!({}));
    }

    /// {state: "off"|"listening"|"streaming", peer?, error?, width?, height?}
    pub fn camera_status(&self) -> Value {
        match self.call("cameraStatus", json!({})) {
            Ok(v) if v.is_object() => v,
            Ok(_) => json!({ "state": "off" }),
            Err(e) => json!({ "state": "off", "error": e }),
        }
    }
}
