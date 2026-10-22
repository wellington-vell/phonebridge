//! Phonebridge: use an Android phone as a microphone and webcam on Linux.
//!
//! The microphone speaks the WO Mic protocol (USB, Wi-Fi and Bluetooth) and
//! feeds a PipeWire virtual mic. The camera is a separate H.264 stream (USB and
//! Wi-Fi) decoded into a v4l2loopback webcam; see `video.rs`.
//!
//! Audio protocol reverse engineered from WOMicClient.exe 6.3:
//!
//!   Transport (USB): `adb forward tcp:P tcp:P`, then TCP to 127.0.0.1:P.
//!   Transport (Wi-Fi): TCP to phone:P; media inline, or UDP if a port is set.
//!   Transport (Bluetooth): RFCOMM, channel found through SDP.
//!   Control messages (both directions): [type u8][len u32 BE][payload len bytes]
//!     0x65 hello       -> payload 04 06 03 00 00 00
//!                      <- payload [status u8][media version u8][...]
//!     0x66 set codec   -> payload 02 02 [media UDP port u32 BE]  (0: inline)
//!                      <- payload [status u8][...]
//!     0x67 start       -> no payload
//!                      <- payload [status u8][...]
//!   Media frames (V4, same TCP socket after start):
//!     [0x04][0x00][len u16 BE][seq u16 BE][timestamp u32 BE][flags u8][opus: len-7 bytes]
//!   Codec: Opus, 48 kHz, mono, 20 ms (960 samples) frames.

use std::fs;
use std::io;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;

mod bluetooth;
mod control;
mod opus;
mod output;
mod proto;
mod transport;
mod video;

use transport::{Media, Transport};

macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[phonebridge] {}", format_args!($($arg)*)) };
}
pub(crate) use log;

const USAGE: &str = "\
usage: phonebridge [OPTIONS]                 run the client in the foreground
       phonebridge toggle                   start in the background, or stop it
       phonebridge start [OPTIONS]          start in the background
       phonebridge stop
       phonebridge mute                     toggle mute (the input stays, sending silence)
       phonebridge status                   print connected | muted | connecting | off

Use your Android phone as a microphone and webcam (USB via adb, Wi-Fi or Bluetooth)

options:
  --port PORT        control port (default 8125)
  --wifi HOST        connect over Wi-Fi to the phone's address instead of USB
  --udp PORT         Wi-Fi: receive media on this UDP port (default: inline TCP)
  --bluetooth ADDR   connect over Bluetooth to the paired phone AA:BB:CC:DD:EE:FF
  --bt-channel N     Bluetooth: RFCOMM channel (default: looked up with SDP)
  --name NAME        virtual microphone name (default \"Phonebridge\")
  --stdout           write raw s16le 48k mono PCM to stdout instead
  --no-adb           USB: skip adb forward (ports already forwarded)
  --once             exit on the first disconnect (of either stream) instead of retrying
  -h, --help         show this help

webcam (USB or Wi-Fi; needs ffmpeg and the v4l2loopback kernel module):
  --camera           also stream the phone's camera to a v4l2loopback webcam
  --no-mic           camera only: no audio connection, no virtual microphone
  --video-port PORT  video port (default: control port + 1, i.e. 8126)
  --video-device DEV v4l2loopback device (default: the one labelled \"Phonebridge\",
                     else the first v4l2loopback device)";

struct Args {
    port: u16,
    transport: Transport,
    udp: u16,
    name: String,
    stdout: bool,
    no_adb: bool,
    once: bool,
    mic: bool,
    camera: bool,
    video_port: Option<u16>,
    video_device: Option<PathBuf>,
}

impl Args {
    fn video_port(&self) -> u16 {
        self.video_port.unwrap_or(self.port.wrapping_add(1))
    }
}

fn parse_args(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        port: 8125,
        transport: Transport::Usb,
        udp: 0,
        name: "Phonebridge".into(),
        stdout: false,
        no_adb: false,
        once: false,
        mic: true,
        camera: false,
        video_port: None,
        video_device: None,
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
            "--wifi" | "--ip" => a.transport = Transport::Wifi(value()?),
            "--udp" => {
                let v = value()?;
                a.udp = v.parse().map_err(|_| format!("invalid UDP port: {v}"))?;
            }
            "--bluetooth" | "--bt" => {
                let channel = match a.transport {
                    Transport::Bluetooth(_, c) => c,
                    _ => None,
                };
                a.transport = Transport::Bluetooth(bluetooth::parse_addr(&value()?)?, channel);
            }
            "--bt-channel" => {
                let v = value()?;
                let c = v
                    .parse()
                    .ok()
                    .filter(|c| (1..=30).contains(c))
                    .ok_or(format!("invalid RFCOMM channel: {v}"))?;
                match &mut a.transport {
                    Transport::Bluetooth(_, channel) => *channel = Some(c),
                    _ => return Err("--bt-channel needs --bluetooth ADDR first".into()),
                }
            }
            "--name" => a.name = value()?,
            "--stdout" => a.stdout = true,
            "--no-adb" => a.no_adb = true,
            "--once" => a.once = true,
            "--camera" => a.camera = true,
            "--no-mic" => a.mic = false,
            "--video-port" => {
                let v = value()?;
                a.video_port = Some(v.parse().map_err(|_| format!("invalid video port: {v}"))?);
            }
            "--video-device" => a.video_device = Some(value()?.into()),
            "-h" | "--help" => {
                println!("{USAGE}");
                process::exit(0);
            }
            _ => return Err(format!("unrecognized argument: {flag}")),
        }
    }
    if a.udp != 0 && !matches!(a.transport, Transport::Wifi(_)) {
        return Err("--udp only works with --wifi".into());
    }
    if !a.camera {
        if !a.mic {
            return Err("--no-mic needs --camera (nothing to stream otherwise)".into());
        }
        if a.video_port.is_some() || a.video_device.is_some() {
            return Err("--video-port and --video-device need --camera".into());
        }
    }
    if a.camera && matches!(a.transport, Transport::Bluetooth(..)) {
        return Err("--camera works over USB or Wi-Fi, not Bluetooth".into());
    }
    if !a.mic && (a.stdout || a.udp != 0) {
        return Err("--stdout and --udp are audio options; drop --no-mic".into());
    }
    if a.camera && a.mic && a.video_port() == a.port {
        return Err("--video-port must differ from --port".into());
    }
    if !matches!(a.transport, Transport::Usb) {
        a.no_adb = true;
    }
    Ok(a)
}

// --------------------------------------------------------------------------- mute
/// SIGUSR1 toggles mute. While muted, silence is sent so the source stays alive.
///
/// State is mirrored to $XDG_RUNTIME_DIR/phonebridge/muted for status queries.
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
    /// The camera's ffmpeg, replaced on every connection by the video thread.
    video: Mutex<Option<Child>>,
    adb_ports: Vec<u16>,
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
        let video = self.video.lock().unwrap().take();
        let children = std::mem::take(&mut *self.children.lock().unwrap());
        video
            .into_iter()
            .chain(children)
            .for_each(video::stop_child);
        for port in &self.adb_ports {
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

fn setup_adb(ports: &[u16]) -> Result<(), String> {
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
    for (i, port) in ports.iter().enumerate() {
        if let Err(e) = adb(&["forward", &format!("tcp:{port}"), &format!("tcp:{port}")]) {
            for done in &ports[..i] {
                let _ = adb(&["forward", "--remove", &format!("tcp:{done}")]);
            }
            return Err(e);
        }
        log!("adb forward tcp:{port} ready");
    }
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
    let connect = || -> io::Result<(transport::Conn, Media, bool)> {
        let mut conn = transport::connect(&args.transport, args.port)?;
        conn.set_timeout(Duration::from_secs(5))?;
        let media = if args.udp != 0 {
            let sock = UdpSocket::bind(("0.0.0.0", args.udp))?;
            sock.set_read_timeout(Some(Duration::from_secs(10)))?;
            Media::Udp(sock, conn.peer_ip())
        } else {
            Media::Inline
        };
        let session = proto::handshake(&mut conn, args.udp)?;
        conn.set_timeout(Duration::from_secs(10))?;
        Ok((conn, media, session.keepalive))
    };
    let (mut conn, mut media, keepalive) = match connect() {
        Ok(c) => c,
        Err(e) => return Ok(e),
    };

    // Keepalives until this function returns (the flag drops with it).
    let alive = Arc::new(AtomicBool::new(true));
    struct Done(Arc<AtomicBool>);
    impl Drop for Done {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    let _done = Done(Arc::clone(&alive));
    if keepalive && let Ok(mut ka) = conn.try_clone() {
        thread::spawn(move || {
            while alive.load(Ordering::Relaxed) && proto::keepalive(&mut ka).is_ok() {
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

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
        let frame = match media.next(&mut conn) {
            Ok(f) => f,
            Err(e) => return Ok(e),
        };
        if let Some(last) = last_seq {
            let ahead = frame.seq.wrapping_sub(last);
            if ahead == 0 || ahead > 0x8000 {
                continue; // duplicate or reordered (UDP)
            }
            let lost = ahead - 1;
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
    let video = if args.camera {
        Some(video::Config {
            transport: args.transport.clone(),
            port: args.video_port(),
            device: match &args.video_device {
                Some(d) => d.clone(),
                None => video::find_device(Path::new("/sys/class/video4linux"))?,
            },
            once: args.once,
        })
    } else {
        None
    };
    let mut ports = Vec::new();
    if args.mic {
        ports.push(args.port);
    }
    if let Some(v) = &video {
        ports.push(v.port);
    }
    if !args.no_adb {
        setup_adb(&ports)?;
    }
    let cleanup = Arc::new(Cleanup {
        mute: Mute::new(),
        children: Mutex::new(Vec::new()),
        video: Mutex::new(None),
        adb_ports: if args.no_adb { Vec::new() } else { ports },
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

    // The camera has its own reconnect loop; when it ends for good (--once or
    // a fatal error) the whole client exits, as it does for audio.
    let video_loop =
        |cfg: video::Config, c: &Cleanup| video::run(&cfg, &c.video).map(|()| ExitCode::FAILURE);
    let video = match video {
        Some(cfg) if args.mic => {
            let c = Arc::clone(&cleanup);
            thread::spawn(move || {
                if let Err(e) = video_loop(cfg, &c) {
                    log!("{e}");
                }
                c.finish();
                process::exit(1);
            });
            None
        }
        other => other,
    };

    let result = (|| {
        if let Some(cfg) = video {
            return video_loop(cfg, &cleanup);
        }
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
            log!("retrying in 2s ({})", args.transport.hint());
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
