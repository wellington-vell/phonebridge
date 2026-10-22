//! Android side effects, implemented in Kotlin (WemicPlugin.kt): permissions,
//! keep-screen-on, the foreground service and the Bluetooth RFCOMM bridge.
//! No-ops on desktop.

use serde_json::{Value, json};

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
    pub fn request_permissions(&self, bluetooth: bool) -> Result<(), String> {
        let mut aliases = vec!["microphone", "notifications"];
        if bluetooth {
            aliases.push("bluetooth");
        }
        let states = self.call("requestPermissions", json!({ "permissions": aliases }))?;
        match states.get("microphone").and_then(Value::as_str) {
            None | Some("granted") => Ok(()),
            Some(_) => Err("microphone permission denied".into()),
        }
    }

    pub fn keep_screen_on(&self, on: bool) {
        let _ = self.call("keepScreenOn", json!({ "on": on }));
    }

    /// Foreground service so capture survives the screen turning off.
    pub fn service(&self, running: bool) {
        let cmd = if running {
            "startService"
        } else {
            "stopService"
        };
        if let Err(e) = self.call(cmd, json!({})) {
            eprintln!("[wemic] {cmd}: {e}");
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
}
