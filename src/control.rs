//! Background control: run the client detached and query / signal it.
//!
//!   wemic toggle            start, or stop if already running
//!   wemic start [OPTIONS]   start in the background (OPTIONS go to the client)
//!   wemic stop
//!   wemic mute              toggle mute (keeps the "WeMic" input alive)
//!   wemic status            prints connected | muted | connecting | off
//!
//! State lives in $XDG_RUNTIME_DIR/wemic: wemic.pid, wemic.log and muted.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

pub const COMMANDS: &[&str] = &["toggle", "start", "stop", "mute", "status"];

pub fn state_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(dir).join("wemic")
}

struct Paths {
    pid: PathBuf,
    log: PathBuf,
    muted: PathBuf,
}

impl Paths {
    fn new() -> Self {
        let dir = state_dir();
        let _ = fs::create_dir_all(&dir);
        Paths {
            pid: dir.join("wemic.pid"),
            log: dir.join("wemic.log"),
            muted: dir.join("muted"),
        }
    }

    /// Pid of the background client, if it is still alive.
    fn running(&self) -> Option<libc::pid_t> {
        let pid: libc::pid_t = fs::read_to_string(&self.pid).ok()?.trim().parse().ok()?;
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        (pid > 0 && unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
    }

    fn status(&self) -> &'static str {
        if self.running().is_none() {
            "off"
        } else if self.muted.exists() {
            "muted"
        } else if self.connected() {
            "connected"
        } else {
            "connecting"
        }
    }

    /// The last connection event in the log decides: a later "disconnected" wins.
    fn connected(&self) -> bool {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        log.lines()
            .rev()
            .find(|l| l.contains("streaming started") || l.contains("disconnected"))
            .is_some_and(|l| l.contains("streaming started"))
    }

    fn log_tail(&self, n: usize) -> String {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

fn notify(critical: bool, title: &str, body: &str) {
    let icon = std::env::var("WEMIC_ICON").unwrap_or_else(|_| "audio-input-microphone".into());
    let mut cmd = Command::new("notify-send");
    if critical {
        cmd.args(["-u", "critical"]);
    }
    let _ = cmd
        .args(["-a", "WeMic", "-i", &icon, title, body])
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
    let _ = fs::remove_file(&p.pid);
    notify(false, "WeMic", "Stopped");
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
    notify(
        false,
        "WeMic",
        "Started. Open WeMic on the phone and press Start.",
    );

    // Report the outcome of the handshake once.
    for _ in 0..30 {
        thread::sleep(Duration::from_secs(1));
        if !matches!(child.try_wait(), Ok(None)) {
            notify(true, "WeMic failed", &p.log_tail(3));
            let _ = fs::remove_file(&p.pid);
            return Ok(ExitCode::FAILURE);
        }
        if p.connected() {
            notify(false, "WeMic", "Connected — select the \"WeMic\" input");
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    let p = Paths::new();
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
