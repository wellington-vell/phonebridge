//! WO Mic Linux client (USB mode).
//!
//! Protocol reverse engineered from WOMicClient.exe 6.3:
//!
//!   Transport (USB): `adb forward tcp:P tcp:P`, then TCP to 127.0.0.1:P.
//!   Control messages (both directions): [type u8][len u32 BE][payload len bytes]
//!     0x65 hello       -> payload 04 06 03 00 00 00
//!                      <- payload [status u8][media version u8][...]
//!     0x66 set codec   -> payload 02 02 [media UDP port u32 BE]  (0 for USB)
//!                      <- payload [status u8][...]
//!     0x67 start       -> no payload
//!                      <- payload [status u8][...]
//!   Media frames (V4, same TCP socket after start):
//!     [0x04][0x00][len u16 BE][seq u16 BE][timestamp u32 BE][flags u8][opus: len-7 bytes]
//!   Codec: Opus, 48 kHz, mono, 20 ms (960 samples) frames.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{self, Child, Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;

mod control;
mod opus;
mod output;
mod proto;

macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[womic] {}", format_args!($($arg)*)) };
}
pub(crate) use log;

const USAGE: &str = "\
usage: womic [OPTIONS]                 run the client in the foreground
       womic toggle                   start in the background, or stop it
       womic start [OPTIONS]          start in the background
       womic stop
       womic mute                     toggle mute (the input stays, sending silence)
       womic status                   print connected | muted | connecting | off

WO Mic client for Linux (USB via adb)

options:
  --port PORT  control port (default 8125)
  --name NAME  virtual microphone name (default \"WO Mic\")
  --stdout     write raw s16le 48k mono PCM to stdout instead
  --no-adb     skip adb forward (port already forwarded)
  --once       exit on disconnect instead of retrying
  -h, --help   show this help";

struct Args {
    port: u16,
    name: String,
    stdout: bool,
    no_adb: bool,
    once: bool,
}

fn parse_args(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        port: 8125,
        name: "WO Mic".into(),
        stdout: false,
        no_adb: false,
        once: false,
    };
    let mut it = argv;
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or(format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--port" => {
                let v = value()?;
                a.port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
            }
            "--name" => a.name = value()?,
            "--stdout" => a.stdout = true,
            "--no-adb" => a.no_adb = true,
            "--once" => a.once = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                process::exit(0);
            }
            _ => return Err(format!("unrecognized argument: {flag}")),
        }
    }
    Ok(a)
}

// --------------------------------------------------------------------------- mute
/// SIGUSR1 toggles mute. While muted, silence is sent so the source stays alive.
///
/// State is mirrored to $XDG_RUNTIME_DIR/womic/muted for status queries.
struct Mute {
    on: AtomicBool,
    flag: PathBuf,
}

impl Mute {
    fn new() -> Self {
        let m = Mute {
            on: AtomicBool::new(false),
            flag: control::state_dir().join("muted"),
        };
        m.set(false);
        m
    }

    fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        let _ = if on {
            self.flag.parent().map(fs::create_dir_all);
            fs::write(&self.flag, b"")
        } else {
            fs::remove_file(&self.flag).or_else(|e| match e.kind() {
                io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })
        };
    }

    fn toggle(&self) {
        let on = !self.is_on();
        self.set(on);
        log!("{}", if on { "muted" } else { "unmuted" });
    }
}

// ----------------------------------------------------------------------- shutdown
/// Everything to undo on exit, reachable from both the main loop and the signal
/// thread (which exits right away instead of waiting for a blocked read).
/// Whichever thread starts it owns the exit; the other one just parks.
struct Cleanup {
    mute: Mute,
    children: Mutex<Vec<Child>>,
    adb_port: Option<u16>,
    done: AtomicBool,
}

impl Cleanup {
    /// Undo everything once. Returns false if another thread got there first.
    fn run(&self) -> bool {
        if self.done.swap(true, Ordering::SeqCst) {
            return false;
        }
        if self.mute.is_on() {
            self.mute.toggle();
        }
        let children = std::mem::take(&mut *self.children.lock().unwrap());
        for mut c in children {
            // SAFETY: plain kill(2) on a pid we spawned and have not reaped.
            unsafe { libc::kill(c.id() as libc::pid_t, libc::SIGTERM) };
            for _ in 0..20 {
                if !matches!(c.try_wait(), Ok(None)) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(port) = self.adb_port {
            let _ = adb(&["forward", "--remove", &format!("tcp:{port}")]);
        }
        true
    }

    /// Run cleanup, or wait forever for the thread that is already running it.
    fn finish(&self) {
        if !self.run() {
            loop {
                thread::park();
            }
        }
    }
}

// ---------------------------------------------------------------------- transport
fn adb(args: &[&str]) -> Result<String, String> {
    let out = Command::new("adb")
        .args(args)
        .output()
        .map_err(|e| format!("adb: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("adb {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn setup_adb(port: u16) -> Result<(), String> {
    let devices = adb(&["devices"]).map_err(|e| {
        if e.contains("No such file") {
            "adb not found (nix-shell -p android-tools)".into()
        } else {
            e
        }
    })?;
    if !devices
        .lines()
        .skip(1)
        .any(|l| l.trim().ends_with("device"))
    {
        return Err(
            "no adb device; enable USB debugging and accept the prompt on the phone".into(),
        );
    }
    adb(&["forward", &format!("tcp:{port}"), &format!("tcp:{port}")])?;
    log!("adb forward tcp:{port} ready");
    Ok(())
}

/// One connection: handshake, then decode frames until the phone goes away.
/// Network errors come back as `Ok(err)` (retryable); output errors are fatal.
fn stream(
    args: &Args,
    dec: &mut opus::Decoder,
    out: &mut output::Output,
    mute: &Mute,
) -> Result<io::Error, String> {
    let connect = || -> io::Result<TcpStream> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, args.port));
        let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        proto::handshake(&mut sock)?;
        sock.set_read_timeout(Some(Duration::from_secs(10)))?;
        sock.set_nodelay(true)?;
        Ok(sock)
    };
    let mut sock = match connect() {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    let mut bytes = Vec::new();
    let mut emit = |pcm: &[i16], out: &mut output::Output| -> Result<(), String> {
        bytes.clear();
        if mute.is_on() {
            bytes.resize(pcm.len() * 2, 0);
        } else {
            bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        }
        out.write(&bytes)
            .map_err(|e| format!("audio output closed: {e}"))
    };

    let mut last_seq: Option<u16> = None;
    loop {
        let frame = match proto::next_frame(&mut sock) {
            Ok(f) => f,
            Err(e) => return Ok(e),
        };
        if let Some(last) = last_seq {
            let lost = frame.seq.wrapping_sub(last).wrapping_sub(1);
            for _ in 0..lost.min(5) {
                match dec.decode(None) {
                    Ok(pcm) => emit(pcm, out)?,
                    Err(e) => log!("opus concealment failed: {e}"),
                }
            }
        }
        last_seq = Some(frame.seq);
        match dec.decode(Some(&frame.opus)) {
            Ok(pcm) => emit(pcm, out)?,
            Err(e) => log!("dropping undecodable frame {}: {e}", frame.seq),
        }
    }
}

// --------------------------------------------------------------------------- main
fn run(args: Args) -> Result<ExitCode, String> {
    let mut dec = opus::Decoder::new()?;
    if !args.no_adb {
        setup_adb(args.port)?;
    }
    let cleanup = Arc::new(Cleanup {
        mute: Mute::new(),
        children: Mutex::new(Vec::new()),
        adb_port: (!args.no_adb).then_some(args.port),
        done: AtomicBool::new(false),
    });

    let mut signals =
        Signals::new([SIGUSR1, SIGTERM, SIGINT, SIGHUP]).map_err(|e| e.to_string())?;
    let sig_cleanup = Arc::clone(&cleanup);
    thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGUSR1 {
                sig_cleanup.mute.toggle();
            } else {
                sig_cleanup.finish();
                process::exit(0);
            }
        }
    });

    let result = (|| {
        let mut out = if args.stdout {
            output::Output::Stdout(io::stdout())
        } else {
            let (out, children) = output::virtual_mic(&args.name).map_err(|e| e.to_string())?;
            cleanup.children.lock().unwrap().extend(children);
            out
        };
        loop {
            let e = stream(&args, &mut dec, &mut out, &cleanup.mute)?;
            log!("disconnected: {e}");
            if args.once {
                return Ok(ExitCode::FAILURE);
            }
            log!("retrying in 2s (is WO Mic started on the phone with transport=USB?)");
            thread::sleep(Duration::from_secs(2));
        }
    })();
    cleanup.finish();
    result
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = match argv.first().map(String::as_str) {
        Some(cmd) if control::COMMANDS.contains(&cmd) => {
            // Validate client options up front so `start` fails loudly, not in the log.
            parse_args(argv[1..].iter().cloned()).and_then(|_| control::run(cmd, &argv[1..]))
        }
        _ => parse_args(argv.into_iter()).and_then(run),
    };
    result.unwrap_or_else(|e| {
        eprintln!("{e}");
        ExitCode::FAILURE
    })
}
