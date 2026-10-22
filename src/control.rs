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
//!   phonebridge logfile           print the log file (also `camera logfile`)
//!
//! The microphone and the camera are independent instances, so each can be
//! toggled on its own. State lives in a private per-user directory (see
//! [`state_dir`]): phonebridge.pid, phonebridge.log, phonebridge.state and
//! muted for the microphone, camera.pid, camera.log and camera.state for the
//! camera.
//!
//! `start` / `toggle` without OPTIONS read default options from
//! $XDG_CONFIG_HOME/phonebridge/args (e.g. `--wifi 192.168.1.20`), so desktop
//! toggles that pass no options can still use Wi-Fi.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::secure;

pub const COMMANDS: &[&str] = &["toggle", "start", "stop", "mute", "status", "logfile"];
pub const CAMERA_COMMANDS: &[&str] = &["toggle", "start", "stop", "status", "logfile"];

/// Environment variable with the pairing code (also how `start` hands it to
/// the background client, so it stays out of `ps`).
pub const TOKEN_ENV: &str = "PHONEBRIDGE_TOKEN";

/// Which background instance a command controls.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mic,
    Camera,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Mic => "phonebridge",
            Kind::Camera => "camera",
        }
    }
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Private per-user state directory: $XDG_RUNTIME_DIR/phonebridge, else
/// $XDG_STATE_HOME/phonebridge (~/.local/state/phonebridge). Never a shared
/// directory like /tmp; created 0700 and checked to be ours.
pub fn state_dir() -> Result<PathBuf, String> {
    let dir = env_dir("XDG_RUNTIME_DIR")
        .or_else(|| env_dir("XDG_STATE_HOME"))
        .or_else(|| env_dir("HOME").map(|h| h.join(".local").join("state")))
        .ok_or("no directory for phonebridge's state: set XDG_RUNTIME_DIR or HOME")?
        .join("phonebridge");
    private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn private_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    // SAFETY: geteuid(2) cannot fail.
    if m.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other(
            "owned by another user; refusing to use it",
        ));
    }
    if m.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Options for files in the state directory: private, never through a symlink.
fn open_opts() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    o
}

/// Create or truncate `path` (not following symlinks) with `data`.
pub fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    open_opts()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?
        .write_all(data)
}

fn read_file(path: &Path) -> io::Result<String> {
    let mut s = String::new();
    open_opts().read(true).open(path)?.read_to_string(&mut s)?;
    Ok(s)
}

fn remove(path: &Path) -> io::Result<()> {
    fs::remove_file(path).or_else(|e| match e.kind() {
        io::ErrorKind::NotFound => Ok(()),
        _ => Err(e),
    })
}

fn config_dir() -> Option<PathBuf> {
    let env = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let dir = env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?;
    Some(dir.join("phonebridge"))
}

/// The pairing code: `given` (--token, already canonical), else
/// $PHONEBRIDGE_TOKEN, else the token file in the config directory, which
/// must be ours and private (chmod 600). None: no code, plaintext.
pub fn token(given: Option<String>) -> Result<Option<String>, String> {
    if given.is_some() {
        return Ok(given);
    }
    if let Some(t) = std::env::var_os(TOKEN_ENV).filter(|t| !t.is_empty()) {
        let t = t.to_str().ok_or(format!("{TOKEN_ENV}: not UTF-8"))?;
        return secure::normalize(t)
            .map(Some)
            .map_err(|e| format!("{TOKEN_ENV}: {e}"));
    }
    let Some(path) = config_dir().map(|d| d.join("token")) else {
        return Ok(None);
    };
    let at = |e: String| format!("{}: {e}", path.display());
    let mut f = match File::open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        f => f.map_err(|e| at(e.to_string()))?,
    };
    let m = f.metadata().map_err(|e| at(e.to_string()))?;
    // SAFETY: geteuid(2) cannot fail.
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(at(format!(
            "must be owned by you and private; run: chmod 600 {}",
            path.display()
        )));
    }
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(|e| at(e.to_string()))?;
    secure::normalize(&text).map(Some).map_err(at)
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
        args = config_dir()
            .and_then(|d| fs::read_to_string(d.join("args")).ok())
            .map(|t| parse_defaults(&t))
            .unwrap_or_default();
    }
    if kind == Kind::Camera {
        args.extend(["--camera".into(), "--no-mic".into()]);
    }
    args
}

/// Take `--token CODE` / `--token=CODE` out of `args`, so the background
/// client gets the code through its environment instead of its command line.
fn split_token(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut token = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--token" {
            token = it.next().cloned();
        } else if let Some(t) = a.strip_prefix("--token=") {
            token = Some(t.to_string());
        } else {
            rest.push(a.clone());
        }
    }
    (rest, token)
}

/// Connection state of a running client, kept in `<instance>.state` for
/// `status` (the log has text the phone chooses, so it isn't parsed).
pub struct Live {
    file: Option<PathBuf>,
    kind: Kind,
    audio: AtomicBool,
    camera: AtomicBool,
    lock: Mutex<()>,
}

impl Live {
    pub fn new(kind: Kind, dir: Option<&Path>) -> Self {
        let live = Live {
            file: dir.map(|d| d.join(format!("{}.state", kind.name()))),
            kind,
            audio: AtomicBool::new(false),
            camera: AtomicBool::new(false),
            lock: Mutex::new(()),
        };
        live.save();
        live
    }

    pub fn audio(&self, on: bool) {
        self.audio.store(on, Ordering::SeqCst);
        self.save();
    }

    pub fn camera(&self, on: bool) {
        self.camera.store(on, Ordering::SeqCst);
        self.save();
    }

    /// Write the state atomically (temporary file, then rename).
    fn save(&self) {
        let Some(file) = &self.file else { return };
        let _guard = self.lock.lock();
        let up = self.audio.load(Ordering::SeqCst) || self.camera.load(Ordering::SeqCst);
        let state = match (up, self.kind) {
            (false, _) => "connecting",
            (true, Kind::Mic) => "connected",
            (true, Kind::Camera) => "streaming",
        };
        let tmp = file.with_extension("state.tmp");
        let _ = write_file(&tmp, state.as_bytes()).and_then(|()| fs::rename(&tmp, file));
    }

    pub fn clear(&self) {
        if let Some(file) = &self.file {
            let _guard = self.lock.lock();
            let _ = remove(file);
        }
    }
}

/// Whether `pid` runs this program, not some process that reused the pid.
/// Compares executable names, not paths: under Nix both run as
/// `.phonebridge-wrapped`, and an upgraded or rebuilt binary has another
/// store path or shows as "… (deleted)".
fn ours(pid: libc::pid_t) -> bool {
    let name = |p: &Path| {
        p.file_name().map(|n| {
            n.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .trim_start_matches('.')
                .trim_end_matches("-wrapped")
                .to_string()
        })
    };
    let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let me = std::env::current_exe().ok();
    name(&exe).is_some_and(|n| me.as_deref().and_then(name) == Some(n))
}

struct Paths {
    kind: Kind,
    pid: PathBuf,
    log: PathBuf,
    state: PathBuf,
    muted: PathBuf,
}

impl Paths {
    fn new(kind: Kind) -> Result<Self, String> {
        let dir = state_dir()?;
        let name = kind.name();
        Ok(Paths {
            kind,
            pid: dir.join(format!("{name}.pid")),
            log: dir.join(format!("{name}.log")),
            state: dir.join(format!("{name}.state")),
            muted: dir.join("muted"),
        })
    }

    fn title(&self) -> &'static str {
        match self.kind {
            Kind::Mic => "Phonebridge",
            Kind::Camera => "Phonebridge camera",
        }
    }

    /// Pid of the background client, if it is still alive (a stale pidfile
    /// is removed).
    fn running(&self) -> Option<libc::pid_t> {
        let pid: libc::pid_t = read_file(&self.pid).ok()?.trim().parse().ok()?;
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        let alive = pid > 0 && unsafe { libc::kill(pid, 0) } == 0 && ours(pid);
        if !alive {
            let _ = remove(&self.pid);
        }
        alive.then_some(pid)
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

    /// Whether the client reports a live stream (for the microphone
    /// instance: the microphone or, with --camera, the camera).
    fn connected(&self) -> bool {
        read_file(&self.state).is_ok_and(|s| matches!(s.trim(), "connected" | "streaming"))
    }

    fn log_tail(&self, n: usize) -> String {
        let log = read_file(&self.log).unwrap_or_default();
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
    let _ = remove(&p.pid);
    notify(false, p.title(), "Stopped");
    ExitCode::SUCCESS
}

fn start(p: &Paths, args: &[String]) -> Result<ExitCode, String> {
    if p.running().is_some() {
        return Ok(ExitCode::SUCCESS);
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
    let at = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    let log = open_opts()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&p.log)
        .map_err(|e| at(&p.log, e))?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    // Fresh state: nothing left over from an earlier run counts as connected.
    remove(&p.state).map_err(|e| at(&p.state, e))?;
    remove(&p.pid).map_err(|e| at(&p.pid, e))?;
    let (args, token) = split_token(args);
    let mut cmd = Command::new(exe);
    cmd.args(&args).stdin(Stdio::null()).stdout(log).stderr(err);
    if let Some(t) = token {
        cmd.env(TOKEN_ENV, t);
    }
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
    let pidfile = open_opts()
        .write(true)
        .create_new(true)
        .open(&p.pid)
        .and_then(|mut f| f.write_all(child.id().to_string().as_bytes()));
    if let Err(e) = pidfile {
        let _ = child.kill();
        let _ = child.wait();
        return Err(at(&p.pid, e));
    }
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
            let _ = remove(&p.pid);
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
    let p = Paths::new(kind)?;
    Ok(match cmd {
        "logfile" => {
            println!("{}", p.log.display());
            ExitCode::SUCCESS
        }
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

    #[test]
    fn token_leaves_the_command_line() {
        let args: Vec<String> = ["--wifi", "h", "--token", "AB-CD", "--token=EF", "--once"]
            .map(String::from)
            .into();
        let (rest, token) = split_token(&args);
        assert_eq!(rest, ["--wifi", "h", "--once"]);
        assert_eq!(token.as_deref(), Some("EF"));
    }

    #[test]
    fn private_state_dir() {
        let root =
            std::env::temp_dir().join(format!("phonebridge-test-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("state");
        private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        // Symlinks are refused, for the directory and for files in it.
        let link = root.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(private_dir(&link).is_err());
        let target = root.join("target");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("x.log")).unwrap();
        assert!(write_file(&dir.join("x.log"), b"oops").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn own_process_is_recognised() {
        assert!(ours(std::process::id() as libc::pid_t));
        // pid 1 is init, not phonebridge.
        assert!(!ours(1));
    }
}
