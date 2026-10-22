//! Background control: run the client detached and query / signal it.
//!
//!   phonebridge toggle            start, or stop if already running
//!   phonebridge start [OPTIONS]   start in the background (OPTIONS go to the client)
//!   phonebridge stop
//!   phonebridge mute              toggle mute (keeps the "Phonebridge" input alive)
//!   phonebridge status            prints connected | muted | connecting | off
//!
//!   phonebridge camera toggle|start|stop|status [OPTIONS]
//!                                 the same for a separate camera-only instance
//!                                 (`--camera --no-mic`); status prints
//!                                 streaming | connecting | off
//!   phonebridge camera device     print the v4l2loopback webcam device
//!
//! The microphone and the camera are independent instances, so each can be
//! toggled on its own. State lives in $XDG_RUNTIME_DIR/phonebridge:
//! phonebridge.pid, phonebridge.log and muted for the microphone, camera.pid
//! and camera.log for the camera.
//!
//! `start` / `toggle` without OPTIONS read default options from
//! $XDG_CONFIG_HOME/phonebridge/args (e.g. `--wifi 192.168.1.20`), so desktop
//! toggles that pass no options can still use Wi-Fi.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

pub const COMMANDS: &[&str] = &["toggle", "start", "stop", "mute", "status"];
pub const CAMERA_COMMANDS: &[&str] = &["toggle", "start", "stop", "status"];

/// Which background instance a command controls.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mic,
    Camera,
}

pub fn state_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(dir).join("phonebridge")
}

fn config_file() -> Option<PathBuf> {
    let env = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let dir = env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?;
    Some(dir.join("phonebridge").join("args"))
}

/// Whitespace-separated options; `#` starts a comment.
fn parse_defaults(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(str::split_whitespace)
        .map(String::from)
        .collect()
}

/// The client arguments for a background command: the given options, or the
/// defaults file when starting without any; camera-only flags for the camera.
pub fn client_args(kind: Kind, cmd: &str, given: &[String]) -> Vec<String> {
    let mut args = given.to_vec();
    if args.is_empty() && matches!(cmd, "start" | "toggle") {
        args = config_file()
            .and_then(|f| fs::read_to_string(f).ok())
            .map(|t| parse_defaults(&t))
            .unwrap_or_default();
    }
    if kind == Kind::Camera {
        args.extend(["--camera".into(), "--no-mic".into()]);
    }
    args
}

struct Paths {
    kind: Kind,
    pid: PathBuf,
    log: PathBuf,
    muted: PathBuf,
}

impl Paths {
    fn new(kind: Kind) -> Self {
        let dir = state_dir();
        let _ = fs::create_dir_all(&dir);
        let name = match kind {
            Kind::Mic => "phonebridge",
            Kind::Camera => "camera",
        };
        Paths {
            kind,
            pid: dir.join(format!("{name}.pid")),
            log: dir.join(format!("{name}.log")),
            muted: dir.join("muted"),
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            Kind::Mic => "Phonebridge",
            Kind::Camera => "Phonebridge camera",
        }
    }

    /// Pid of the background client, if it is still alive.
    fn running(&self) -> Option<libc::pid_t> {
        let pid: libc::pid_t = fs::read_to_string(&self.pid).ok()?.trim().parse().ok()?;
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        (pid > 0 && unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
    }

    fn status(&self) -> &'static str {
        match (self.kind, self.running().is_some()) {
            (_, false) => "off",
            (Kind::Mic, _) if self.muted.exists() => "muted",
            (Kind::Mic, _) if self.connected() => "connected",
            (Kind::Camera, _) if self.connected() => "streaming",
            _ => "connecting",
        }
    }

    /// Connected if the microphone or the camera is live. For each, the last
    /// of its connection events in the log decides.
    fn connected(&self) -> bool {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let live = |up: &str, down: &str| {
            log.lines()
                .rev()
                .find(|l| l.contains(up) || l.contains(down))
                .is_some_and(|l| l.contains(up))
        };
        let camera = live("camera: streaming", "camera: stopped");
        match self.kind {
            Kind::Mic => live("streaming started", "disconnected") || camera,
            Kind::Camera => camera,
        }
    }

    fn log_tail(&self, n: usize) -> String {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

fn notify(critical: bool, title: &str, body: &str) {
    let icon =
        std::env::var("PHONEBRIDGE_ICON").unwrap_or_else(|_| "audio-input-microphone".into());
    let mut cmd = Command::new("notify-send");
    if critical {
        cmd.args(["-u", "critical"]);
    }
    let _ = cmd
        .args(["-a", "Phonebridge", "-i", &icon, title, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn send(pid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: plain kill(2) on the pid from our pidfile.
    unsafe { libc::kill(pid, sig) };
}

fn stop(p: &Paths) -> ExitCode {
    let Some(pid) = p.running() else {
        let _ = fs::remove_file(&p.pid);
        return ExitCode::FAILURE;
    };
    send(pid, libc::SIGTERM);
    // Wait for its cleanup (adb forwards, ffmpeg), so an immediate restart
    // doesn't race it for ports, the webcam device or the log file.
    // SAFETY: kill(2) with signal 0 only checks that the process exists.
    for _ in 0..30 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(&p.pid);
    notify(false, p.title(), "Stopped");
    ExitCode::SUCCESS
}

fn start(p: &Paths, args: &[String]) -> Result<ExitCode, String> {
    if p.running().is_some() {
        return Ok(ExitCode::SUCCESS);
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
    let log = File::create(&p.log).map_err(|e| format!("{}: {e}", p.log.display()))?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(log).stderr(err);
    // SAFETY: setsid(2) is async-signal-safe; detaches the client from our session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start client: {e}"))?;
    fs::write(&p.pid, child.id().to_string()).map_err(|e| format!("{}: {e}", p.pid.display()))?;
    let hint = match p.kind {
        Kind::Mic => "Started. Open Phonebridge on the phone and press Start.",
        Kind::Camera => "Started. Open Phonebridge on the phone and turn the camera on.",
    };
    notify(false, p.title(), hint);

    // Report the outcome of the handshake once.
    for _ in 0..30 {
        thread::sleep(Duration::from_secs(1));
        if !matches!(child.try_wait(), Ok(None)) {
            notify(true, &format!("{} failed", p.title()), &p.log_tail(3));
            let _ = fs::remove_file(&p.pid);
            return Ok(ExitCode::FAILURE);
        }
        if p.connected() {
            let has = |o: &str| args.iter().any(|a| a == o);
            let body = match (has("--camera"), has("--no-mic")) {
                (true, true) => "Streaming — select the \"Phonebridge\" webcam",
                (true, false) => "Connected — select the \"Phonebridge\" input and webcam",
                _ => "Connected — select the \"Phonebridge\" input",
            };
            notify(false, p.title(), body);
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Run a background command. `args` are the full client arguments (see
/// [`client_args`]), already validated.
pub fn run(kind: Kind, cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    let p = Paths::new(kind);
    Ok(match cmd {
        "status" => {
            println!("{}", p.status());
            ExitCode::SUCCESS
        }
        "stop" => stop(&p),
        "mute" => match p.running() {
            Some(pid) => {
                send(pid, libc::SIGUSR1);
                ExitCode::SUCCESS
            }
            None => ExitCode::FAILURE,
        },
        "start" => start(&p, args)?,
        "toggle" if p.running().is_some() => stop(&p),
        "toggle" => start(&p, args)?,
        _ => unreachable!("unknown control command {cmd}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_file() {
        let text = "# Wi-Fi by default\n--wifi 192.168.1.20  # phone\n\n--port=8125\n";
        assert_eq!(
            parse_defaults(text),
            ["--wifi", "192.168.1.20", "--port=8125"]
        );
    }

    #[test]
    fn camera_args() {
        let given = vec!["--wifi".to_string(), "10.0.0.2".to_string()];
        assert_eq!(
            client_args(Kind::Camera, "start", &given),
            ["--wifi", "10.0.0.2", "--camera", "--no-mic"]
        );
        assert_eq!(client_args(Kind::Mic, "stop", &[]), Vec::<String>::new());
    }
}
