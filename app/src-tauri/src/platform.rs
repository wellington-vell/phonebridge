//! Android side effects, implemented in Kotlin (PhonebridgePlugin.kt): permissions,
//! keep-screen-on, the foreground service, the Bluetooth RFCOMM bridge and
//! camera sharing (CameraStreamer.kt) with its preview (CameraPreview.kt).
//! No-ops on desktop, which has no camera.

use serde_json::{Value, json};

use crate::{CameraConfig, PreviewRect};

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

    /// Ask for the runtime permissions; fails if the microphone is refused.
    /// The camera has its own, [`Self::request_camera_permission`].
    pub fn request_permissions(&self, bluetooth: bool) -> Result<(), String> {
        let mut aliases = vec!["microphone", "notifications"];
        if bluetooth {
            aliases.push("bluetooth");
        }
        let states = self.call("requestPermissions", json!({ "permissions": aliases }))?;
        if !granted(&states, "microphone") {
            return Err("microphone permission denied".into());
        }
        Ok(())
    }

    /// Prompt for the camera permission unless already granted (Android
    /// answers without a prompt once it was refused twice).
    pub fn request_camera_permission(&self) -> Result<(), String> {
        let states = self.call("requestPermissions", json!({ "permissions": ["camera"] }))?;
        if granted(&states, "camera") {
            Ok(())
        } else {
            Err("camera permission denied".into())
        }
    }

    /// Keep the screen on while a PC is connected, if `enabled`: `mic` says
    /// whether a microphone client is; the camera side tracks its own client.
    pub fn keep_screen_on(&self, mic: bool, enabled: bool) {
        let _ = self.call("keepScreenOn", json!({ "mic": mic, "enabled": enabled }));
    }

    /// Foreground service so capture survives the screen turning off. Called
    /// again when camera sharing turns on or off, to add or drop the camera
    /// service type.
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

    /// H.264 camera server on `port`. The camera opens right away (for the
    /// preview) and stays open until [`Self::stop_camera`].
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

    /// {state: "off"|"starting"|"listening"|"streaming"|"error", peer?,
    /// error?, width?, height?, preview, permission}
    pub fn camera_status(&self) -> Value {
        match self.call("cameraStatus", json!({})) {
            Ok(v) if v.is_object() => v,
            Ok(_) => json!({ "state": "off" }),
            Err(e) => json!({ "state": "off", "error": e }),
        }
    }

    /// Place the native preview over the page's placeholder.
    pub fn set_preview_rect(&self, rect: &PreviewRect) {
        let _ = self.call("setPreviewRect", json!(rect));
    }

    pub fn open_app_settings(&self) -> Result<(), String> {
        self.call("openAppSettings", json!({})).map(drop)
    }
}

/// A permission state from requestPermissions; missing means not applicable.
fn granted(states: &Value, alias: &str) -> bool {
    matches!(
        states.get(alias).and_then(Value::as_str),
        None | Some("granted")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_states() {
        let states = json!({ "microphone": "granted", "camera": "denied" });
        assert!(granted(&states, "microphone"));
        assert!(!granted(&states, "camera"));
        assert!(granted(&states, "bluetooth"));
        assert!(!granted(&json!({ "camera": "prompt" }), "camera"));
    }
}
