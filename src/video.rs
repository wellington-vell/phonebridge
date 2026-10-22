//! Webcam: the phone's camera as H.264, decoded by ffmpeg into a v4l2loopback device.
//!
//! A separate TCP stream from the audio one, phone = server, so either can drop
//! without taking the other down. Over USB it goes through its own `adb
//! forward`; Bluetooth has no video (too little bandwidth).
//!
//! Browsers (Chrome with `exclusive_caps=1`, Firefox) only list a loopback
//! device as a camera while something writes to it, and a reader breaks if
//! the format changes under it. So one ffmpeg *writer* feeds the device for
//! the whole run at a fixed size and pixel format (YUYV), and each phone
//! connection gets its own *decoder* ffmpeg that rotates and fits the picture
//! into that size and hands raw frames over (newest frame wins). Between
//! connections the writer repeats the last frame briefly, then shows a
//! placeholder at a low rate.
//!
//! Wire protocol (big-endian):
//!   -> "PBV1"
//!   <- "PBV1" [status u8][codec u8][width u16][height u16][rotation u16][fps u8]
//!        status != 0: [msg_len u16][UTF-8 msg] follows and the phone closes
//!        (1: camera sharing disabled on the phone, 2: camera unavailable)
//!        codec 1: H.264 Annex-B; rotation: degrees clockwise to display upright
//!   <- frames: [len u32][pts_us u64][flags u8][len bytes of H.264 Annex-B]
//!        flags: bit0 keyframe, bit1 codec config (SPS/PPS, always sent first)

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::log;
use crate::transport::{self, Transport};

const MAGIC: &[u8; 4] = b"PBV1";
const CODEC_H264: u8 = 1;
/// "PBV1" + status + codec + width + height + rotation + fps.
const REPLY: usize = 13;
const FRAME_HEADER: usize = 13;
/// Far above any sane H.264 access unit; guards against a desynced stream.
const MAX_FRAME: usize = 16 << 20;
/// Card label set by the NixOS module / suggested modprobe options.
pub const CARD_LABEL: &str = "Phonebridge";
const RETRY: Duration = Duration::from_secs(3);
/// Webcam size unless --video-size says otherwise.
pub const DEFAULT_SIZE: (u32, u32) = (1280, 720);
/// YUYV, the classic webcam format: Chromium and Firefox both read it from
/// v4l2loopback (as they do I420).
const PIX_FMT: &str = "yuyv422";
const BYTES_PER_PIXEL: usize = 2;
/// Frame interval while the phone isn't streaming (5 fps): enough for
/// readers that give up on a silent device, cheap otherwise.
const IDLE_INTERVAL: Duration = Duration::from_millis(200);
/// How long the last phone frame stays after the stream pauses (the phone
/// reconnects to change camera, resolution or orientation) before the
/// placeholder replaces it.
const GRACE: Duration = Duration::from_secs(3);
const WRITER_RETRY: Duration = Duration::from_secs(3);

pub struct Config {
    pub transport: Transport,
    pub port: u16,
    pub device: PathBuf,
    /// Return after the first session (or failed attempt) instead of retrying.
    pub once: bool,
    /// Fixed webcam size, whatever the phone sends.
    pub size: (u32, u32),
}

/// Stream parameters from the phone's reply.
#[derive(Debug, PartialEq)]
pub struct Info {
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    pub rotation: u16,
    pub fps: u8,
}

#[derive(Debug, PartialEq)]
enum Hello {
    Ready(Info),
    Refused { status: u8, msg: String },
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn recv_exact(sock: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    sock.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => io::Error::new(e.kind(), "connection closed by phone"),
        _ => e,
    })
}

/// Split the fixed-size reply into its status and stream parameters.
fn parse_reply(r: &[u8; REPLY]) -> io::Result<(u8, Info)> {
    if &r[..4] != MAGIC {
        return Err(invalid(format!(
            "not a Phonebridge video stream (got {:02x?})",
            &r[..4]
        )));
    }
    let be16 = |i: usize| u16::from_be_bytes([r[i], r[i + 1]]);
    Ok((
        r[4],
        Info {
            codec: r[5],
            width: be16(6),
            height: be16(8),
            rotation: be16(10),
            fps: r[12],
        },
    ))
}

fn hello(sock: &mut (impl Read + Write)) -> io::Result<Hello> {
    sock.write_all(MAGIC)?;
    let mut reply = [0u8; REPLY];
    recv_exact(sock, &mut reply)?;
    let (status, info) = parse_reply(&reply)?;
    if status == 0 {
        return Ok(Hello::Ready(info));
    }
    // The message is best effort: the phone closes right after it.
    let mut msg = String::new();
    let mut len = [0u8; 2];
    if sock.read_exact(&mut len).is_ok() {
        let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
        if sock.read_exact(&mut buf).is_ok() {
            msg = String::from_utf8_lossy(&buf).into_owned();
        }
    }
    Ok(Hello::Refused { status, msg })
}

#[derive(Debug, PartialEq)]
struct FrameHeader {
    len: usize,
    #[allow(dead_code)]
    pts_us: u64,
    flags: u8,
}

fn parse_frame_header(h: &[u8; FRAME_HEADER]) -> io::Result<FrameHeader> {
    let len = u32::from_be_bytes(h[0..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!(
            "video frame of {len} bytes; stream out of sync?"
        )));
    }
    Ok(FrameHeader {
        len,
        pts_us: u64::from_be_bytes(h[4..12].try_into().unwrap()),
        flags: h[12],
    })
}

/// Read the next frame's payload into `buf`.
fn next_frame(sock: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<FrameHeader> {
    let mut hdr = [0u8; FRAME_HEADER];
    recv_exact(sock, &mut hdr)?;
    let h = parse_frame_header(&hdr)?;
    buf.resize(h.len, 0);
    recv_exact(sock, buf)?;
    Ok(h)
}

// -------------------------------------------------------------------- ffmpeg
/// Parse `--video-size WxH`.
pub fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("invalid video size {s:?}: expected WxH with even numbers, e.g. 1280x720");
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u32 = w.trim().parse().map_err(|_| bad())?;
    let h: u32 = h.trim().parse().map_err(|_| bad())?;
    let ok = |n: u32| n.is_multiple_of(2) && (128..=4096).contains(&n);
    if !ok(w) || !ok(h) {
        return Err(bad());
    }
    Ok((w, h))
}

fn frame_size((w, h): (u32, u32)) -> usize {
    w as usize * h as usize * BYTES_PER_PIXEL
}

/// ffmpeg filter that turns the decoded image upright.
fn rotate_filter(rotation: u16) -> Option<&'static str> {
    match rotation {
        90 => Some("transpose=1"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=2"),
        _ => None,
    }
}

/// Rotate upright, then fit into the webcam size without distortion. When the
/// aspect ratios match that's a plain scale; otherwise (portrait into a
/// landscape webcam) the picture is centred over a blurred, zoomed copy of
/// itself instead of black bars, as phones and video-call apps do.
fn video_filter(info: &Info, (w, h): (u32, u32)) -> String {
    let mut f = rotate_filter(info.rotation)
        .map(|r| format!("{r},"))
        .unwrap_or_default();
    let (sw, sh) = match info.rotation {
        90 | 270 => (info.height, info.width),
        _ => (info.width, info.height),
    };
    let fit = format!("scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2");
    if u64::from(sw) * u64::from(h) == u64::from(sh) * u64::from(w) {
        f += &format!("{fit},pad={w}:{h}:(ow-iw)/2:(oh-ih)/2");
    } else {
        // The blur works on a 1/8 size copy: cheap, and blurrier.
        let (bw, bh) = ((w / 16).max(1) * 2, (h / 16).max(1) * 2);
        f += &format!(
            "split[a][b];\
             [a]scale={bw}:{bh}:force_original_aspect_ratio=increase,crop={bw}:{bh},\
             boxblur=4,scale={w}:{h}[bg];\
             [b]{fit}[fg];\
             [bg][fg]overlay=(W-w)/2:(H-h)/2"
        );
    }
    f + &format!(",setsar=1,format={PIX_FMT}")
}

fn strings<const N: usize>(a: [&str; N]) -> Vec<String> {
    a.map(String::from).into()
}

/// Per connection: raw Annex-B on stdin, fixed-size raw frames on stdout,
/// decoded with as little buffering as ffmpeg allows. low_delay disables
/// frame threading, which would hold frames back; with fpsprobesize 0 probing
/// ends at the first decoded frame instead of waiting for 20 to estimate a
/// rate we already know. (A tiny -probesize stops before the first full
/// access unit and fails; -fflags nobuffer was seen dropping frames.)
fn decoder_args(info: &Info, size: (u32, u32)) -> Vec<String> {
    let mut a = strings([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-flags",
        "low_delay",
        "-fpsprobesize",
        "0",
        "-f",
        "h264",
        "-framerate",
    ]);
    a.push(info.fps.max(1).to_string());
    a.extend(strings(["-i", "-", "-vf"]));
    a.push(video_filter(info, size));
    a.extend(strings([
        "-fps_mode",
        "passthrough",
        "-pix_fmt",
        PIX_FMT,
        "-f",
        "rawvideo",
        "-",
    ]));
    a
}

/// For the whole run: raw frames on stdin, written to the v4l2loopback
/// device as they come (the rate only sets timestamps nobody reads).
fn writer_args((w, h): (u32, u32), device: &Path) -> Vec<String> {
    let mut a = strings([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-f",
        "rawvideo",
        "-pix_fmt",
        PIX_FMT,
        "-video_size",
    ]);
    a.push(format!("{w}x{h}"));
    a.extend(strings([
        "-framerate",
        "30",
        "-i",
        "-",
        "-c:v",
        "copy",
        "-f",
        "v4l2",
    ]));
    a.push(device.display().to_string());
    a
}

/// One placeholder frame: dark grey with "Phonebridge / waiting for the
/// phone" when drawtext has a font (PHONEBRIDGE_FONT, set by the Nix
/// package, else fontconfig's default).
fn placeholder_args((w, h): (u32, u32), font: Option<&str>, text: bool) -> Vec<String> {
    let mut a = strings([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
    ]);
    a.push(format!("color=c=0x202124:s={w}x{h}:r=1"));
    if text {
        let font = font.map(|f| format!("fontfile={f}:")).unwrap_or_default();
        let (big, small, gap) = (h / 10, h / 24, h / 40);
        a.push("-vf".into());
        a.push(format!(
            "drawtext={font}text=Phonebridge:fontcolor=white:fontsize={big}:\
             x=(w-text_w)/2:y=h/2-text_h-{gap},\
             drawtext={font}text=waiting for the phone camera:fontcolor=0xa0a0a0:\
             fontsize={small}:x=(w-text_w)/2:y=h/2+{gap}"
        ));
    }
    a.extend(strings([
        "-frames:v",
        "1",
        "-pix_fmt",
        PIX_FMT,
        "-f",
        "rawvideo",
        "-",
    ]));
    a
}

fn placeholder(size: (u32, u32)) -> Vec<u8> {
    let font = std::env::var("PHONEBRIDGE_FONT").ok();
    for text in [true, false] {
        let out = Command::new("ffmpeg")
            .args(placeholder_args(size, font.as_deref(), text))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(o) = out
            && o.status.success()
            && o.stdout.len() == frame_size(size)
        {
            return o.stdout;
        }
    }
    // Same dark grey in YUYV (Y U Y V per 2 pixels).
    [44u8, 128].repeat(frame_size(size) / 2)
}

// ------------------------------------------------------------------ children
/// Whether the child exited (and was reaped) within `tenths` x 100 ms.
fn exited(c: &mut Child, tenths: u32) -> bool {
    for _ in 0..tenths {
        if !matches!(c.try_wait(), Ok(None)) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// SIGTERM, then SIGKILL if it lingers; always reaped.
pub fn stop_child(mut c: Child) {
    // SAFETY: plain kill(2) on a pid we spawned and have not reaped.
    unsafe { libc::kill(c.id() as libc::pid_t, libc::SIGTERM) };
    if !exited(&mut c, 20) {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// The camera's ffmpeg processes, also reachable from the shutdown path
/// (signal thread), which calls [`Procs::stop_all`].
#[derive(Default)]
pub struct Procs {
    stopped: AtomicBool,
    /// Writes to the webcam for the whole run.
    writer: Mutex<Option<Child>>,
    /// Decodes the current connection.
    decoder: Mutex<Option<Child>>,
}

impl Procs {
    /// Spawn into `slot`, returning its stdin and stdout. Refused once
    /// stop_all has run; checked under the slot's lock, so nothing spawned
    /// here can outlive the shutdown.
    fn spawn(
        &self,
        slot: &Mutex<Option<Child>>,
        cmd: &mut Command,
    ) -> io::Result<(ChildStdin, Option<ChildStdout>)> {
        let mut guard = slot.lock().unwrap();
        if self.stopped.load(Ordering::SeqCst) {
            return Err(io::Error::other("shutting down"));
        }
        let mut child = cmd.stdin(Stdio::piped()).spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take();
        *guard = Some(child);
        Ok((stdin, stdout))
    }

    /// Wait up to `tenths` x 100 ms for the child in `slot` to exit (after
    /// its stdin was closed), then stop it. The exit status if it exited by
    /// itself.
    fn reap(&self, slot: &Mutex<Option<Child>>, tenths: u32) -> Option<ExitStatus> {
        let mut c = slot.lock().unwrap().take()?;
        if exited(&mut c, tenths) {
            c.try_wait().ok().flatten()
        } else {
            stop_child(c);
            None
        }
    }

    pub fn stop_all(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        for slot in [&self.decoder, &self.writer] {
            let child = slot.lock().unwrap().take();
            if let Some(c) = child {
                stop_child(c);
            }
        }
    }

    fn spawn_writer(&self, cfg: &Config) -> io::Result<ChildStdin> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(writer_args(cfg.size, &cfg.device))
            .stdout(Stdio::null());
        self.spawn(&self.writer, &mut cmd).map(|(stdin, _)| stdin)
    }
}

// ---------------------------------------------------------------------- feed
/// Hands decoded frames to the writer; only the newest one is kept, so a
/// slow consumer drops frames instead of building up delay.
#[derive(Default)]
struct Feed {
    state: Mutex<FeedState>,
    cv: Condvar,
}

#[derive(Default)]
struct FeedState {
    frame: Vec<u8>,
    fresh: bool,
    last: Option<Instant>,
    stop: bool,
}

/// What the writer sends next.
#[derive(Debug, PartialEq)]
enum Next {
    /// The new phone frame, now in the writer's buffer.
    Frame,
    /// The previous phone frame again (stream paused, e.g. reconnecting).
    Repeat,
    Placeholder,
    Stop,
}

impl Feed {
    /// Publish `buf` as the newest frame; `buf` gets back a spare buffer.
    fn push(&self, buf: &mut Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        std::mem::swap(&mut st.frame, buf);
        st.fresh = true;
        st.last = Some(Instant::now());
        self.cv.notify_one();
    }

    fn stop(&self) {
        self.state.lock().unwrap().stop = true;
        self.cv.notify_one();
    }

    /// Wait up to `idle` for a frame; swaps a new one into `out`.
    fn next(&self, out: &mut Vec<u8>, idle: Duration, grace: Duration) -> Next {
        let st = self.state.lock().unwrap();
        let (mut st, _) = self
            .cv
            .wait_timeout_while(st, idle, |s| !s.fresh && !s.stop)
            .unwrap();
        if st.stop {
            Next::Stop
        } else if st.fresh {
            std::mem::swap(&mut st.frame, out);
            st.fresh = false;
            Next::Frame
        } else if st.last.is_some_and(|t| t.elapsed() < grace) {
            Next::Repeat
        } else {
            Next::Placeholder
        }
    }
}

/// Keep the webcam fed for the whole run: phone frames as they arrive,
/// otherwise the last one (briefly) or the placeholder at a low rate, so the
/// device never loses its producer and consumers never see a format change.
/// Restarts the writer if it dies.
fn write_loop(cfg: &Config, procs: &Procs, feed: &Feed, placeholder: &[u8], stdin: ChildStdin) {
    let mut stdin = Some(stdin);
    let mut out = Vec::new();
    let mut failing = false;
    loop {
        let data = match feed.next(&mut out, IDLE_INTERVAL, GRACE) {
            Next::Stop => break,
            Next::Frame | Next::Repeat if out.len() == placeholder.len() => &out[..],
            _ => placeholder,
        };
        let Some(w) = &mut stdin else {
            // Writer down: retry now and then.
            match procs.spawn_writer(cfg) {
                Ok(s) => stdin = Some(s),
                Err(e) if !failing => {
                    log!("camera: cannot restart the webcam writer: {e}");
                    failing = true;
                }
                Err(_) => {}
            }
            if stdin.is_none() {
                thread::sleep(WRITER_RETRY);
            }
            continue;
        };
        match w.write_all(data) {
            Ok(()) => failing = false,
            Err(_) if procs.stopped.load(Ordering::SeqCst) => return, // shutting down
            Err(e) => {
                stdin = None;
                let status = procs.reap(&procs.writer, 10);
                if !failing {
                    log!(
                        "camera: webcam writer (ffmpeg) stopped: {e}, {}; restarting it \
                         every {}s",
                        describe(status),
                        WRITER_RETRY.as_secs()
                    );
                }
                failing = true;
                thread::sleep(WRITER_RETRY);
            }
        }
    }
    drop(stdin);
    procs.reap(&procs.writer, 10);
}

// ------------------------------------------------------------------- device
/// The v4l2loopback device to write to: the one labelled "Phonebridge" if
/// any, else the first loopback device. `sys` is /sys/class/video4linux.
pub fn find_device(sys: &Path) -> Result<PathBuf, String> {
    let mut nodes: Vec<(u32, String, bool)> = fs::read_dir(sys)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let node = e.file_name().into_string().ok()?;
            let n: u32 = node.strip_prefix("video")?.parse().ok()?;
            let dir = e.path();
            let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
            Some((n, name.trim().to_string(), is_loopback(&dir, name.trim())))
        })
        .collect();
    nodes.sort();
    nodes
        .iter()
        .find(|(_, name, lo)| *lo && name == CARD_LABEL)
        .or_else(|| nodes.iter().find(|(_, _, lo)| *lo))
        .map(|(n, _, _)| PathBuf::from(format!("/dev/video{n}")))
        .ok_or_else(|| {
            format!(
                "no v4l2loopback device found for the webcam. Load the module:\n  \
                 sudo modprobe v4l2loopback video_nr=10 card_label={CARD_LABEL} exclusive_caps=1\n\
                 (NixOS: programs.phonebridge.camera.enable from this flake's nixosModules.default),\n\
                 or pass --video-device /dev/videoN"
            )
        })
}

/// v4l2loopback exposes `max_openers` in sysfs, which real cameras don't; the
/// driver link and default card names are fallbacks for other versions.
fn is_loopback(dir: &Path, name: &str) -> bool {
    dir.join("max_openers").exists()
        || fs::read_link(dir.join("device/driver"))
            .is_ok_and(|d| d.file_name().is_some_and(|f| f == "v4l2loopback"))
        || name.contains("Dummy video device")
        || name.contains("Loopback")
}

// --------------------------------------------------------------------- loop
/// How a session ended. Only `streamed` ones are always logged; failed
/// attempts are logged when the reason changes, so a phone with camera
/// sharing off doesn't flood the log.
struct Outcome {
    streamed: bool,
    msg: String,
}

fn failed(msg: impl ToString) -> Result<Outcome, String> {
    Ok(Outcome {
        streamed: false,
        msg: msg.to_string(),
    })
}

/// "exit status: 1" / "signal: 9 (SIGKILL)", for the log.
fn describe(status: Option<ExitStatus>) -> String {
    match status {
        Some(s) => s.to_string(),
        None => "hung, stopped".into(),
    }
}

/// Connect and stream until the phone goes away, feeding decoded frames to
/// the writer. `Err` is fatal (ffmpeg missing).
fn session(cfg: &Config, procs: &Procs, feed: &Feed) -> Result<Outcome, String> {
    let mut conn = match transport::connect(&cfg.transport, cfg.port) {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let info = match conn
        .set_timeout(Duration::from_secs(5))
        .and_then(|()| hello(&mut conn))
    {
        Ok(Hello::Ready(info)) => info,
        Ok(Hello::Refused { status, msg }) => {
            let why = match status {
                1 => "camera sharing is disabled on the phone",
                2 => "camera unavailable on the phone",
                _ => "phone refused the camera",
            };
            let detail = if msg.is_empty() {
                msg
            } else {
                format!(": {msg}")
            };
            return failed(format!("{why} (status {status}){detail}"));
        }
        Err(e) => return failed(e),
    };
    if info.codec != CODEC_H264 {
        return failed(format!("unsupported video codec {}", info.codec));
    }
    let (w, h) = cfg.size;
    log!(
        "camera: streaming {}x{} at {} fps, rotation {}°, to {} as {w}x{h}",
        info.width,
        info.height,
        info.fps,
        info.rotation,
        cfg.device.display()
    );

    let mut cmd = Command::new("ffmpeg");
    cmd.args(decoder_args(&info, cfg.size))
        .stdout(Stdio::piped());
    let (mut stdin, stdout) = match procs.spawn(&procs.decoder, &mut cmd) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!("ffmpeg: {e} (needed for --camera)"));
        }
        Err(e) => return failed(format!("ffmpeg: {e}")),
    };
    let mut stdout = stdout.expect("piped stdout");

    let (msg, frames) = thread::scope(|s| {
        // Decoded frames -> writer, until the decoder exits.
        let reader = s.spawn(move || {
            let mut buf = vec![0u8; frame_size(cfg.size)];
            let mut n = 0u64;
            while stdout.read_exact(&mut buf).is_ok() {
                feed.push(&mut buf);
                buf.resize(frame_size(cfg.size), 0);
                n += 1;
            }
            n
        });
        // Phone -> decoder.
        let mut pump = || -> io::Result<()> {
            conn.set_timeout(Duration::from_secs(10))?;
            let mut buf = Vec::new();
            loop {
                next_frame(&mut conn, &mut buf)?;
                stdin.write_all(&buf).map_err(|e| {
                    io::Error::new(e.kind(), format!("decoder (ffmpeg) stopped: {e}"))
                })?;
            }
        };
        let msg = pump().unwrap_err().to_string();
        // EOF lets ffmpeg flush and exit by itself; only stop it if it lingers.
        drop(stdin);
        let status = procs.reap(&procs.decoder, 10);
        let msg = match status {
            Some(s) if !s.success() && msg.contains("decoder") => {
                format!("{msg} ({})", describe(Some(s)))
            }
            _ => msg,
        };
        (msg, reader.join().unwrap_or(0))
    });
    Ok(Outcome {
        streamed: true,
        msg: if frames == 0 {
            format!("{msg} (no picture decoded)")
        } else {
            msg
        },
    })
}

/// The camera: a writer that keeps the webcam fed for the whole run, and a
/// reconnect loop for the phone's stream, independent of the audio one.
/// Returns after one attempt with `once`; `Err` only for fatal problems.
pub fn run(cfg: &Config, procs: &Procs) -> Result<(), String> {
    let placeholder = placeholder(cfg.size);
    let stdin = procs
        .spawn_writer(cfg)
        .map_err(|e| format!("ffmpeg: {e} (needed for --camera)"))?;
    let (w, h) = cfg.size;
    log!(
        "camera: webcam {} ready, {w}x{h} YUYV; placeholder until the phone streams",
        cfg.device.display()
    );
    let feed = Feed::default();
    thread::scope(|s| {
        s.spawn(|| write_loop(cfg, procs, &feed, &placeholder, stdin));
        let result = (|| {
            let mut last = String::new();
            // Attempts since the last stream: the phone reconnects right away
            // after a camera / resolution / orientation change, so retry fast
            // at first.
            let mut fails = 0;
            loop {
                let o = session(cfg, procs, &feed)?;
                if o.streamed {
                    log!("camera: stopped: {}", o.msg);
                    last.clear();
                    fails = 0;
                } else {
                    if o.msg != last {
                        log!(
                            "camera: {} (retrying quietly every {}s)",
                            o.msg,
                            RETRY.as_secs()
                        );
                    }
                    last = o.msg;
                    fails += 1;
                }
                if cfg.once {
                    return Ok(());
                }
                thread::sleep(match fails {
                    0 => Duration::from_millis(200),
                    1..=5 => Duration::from_millis(500),
                    _ => RETRY,
                });
            }
        })();
        feed.stop();
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u8) -> Vec<u8> {
        let mut r = b"PBV1".to_vec();
        r.extend([status, 1, 0x05, 0x00, 0x02, 0xd0, 0x00, 90, 30]);
        r
    }

    /// Scripted phone: reads come from `input`, writes are collected.
    struct Fake {
        input: io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }
    impl Read for Fake {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.input.read(b)
        }
    }
    impl Write for Fake {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.sent.write(b)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn fake(input: Vec<u8>) -> Fake {
        Fake {
            input: io::Cursor::new(input),
            sent: Vec::new(),
        }
    }

    #[test]
    fn hello_ready() {
        let mut f = fake(reply(0));
        let h = hello(&mut f).unwrap();
        assert_eq!(f.sent, b"PBV1");
        assert_eq!(
            h,
            Hello::Ready(Info {
                codec: 1,
                width: 1280,
                height: 720,
                rotation: 90,
                fps: 30
            })
        );
    }

    #[test]
    fn hello_refused_with_message() {
        let mut r = reply(1);
        r.extend([0, 3]);
        r.extend(b"off");
        let h = hello(&mut fake(r)).unwrap();
        assert_eq!(
            h,
            Hello::Refused {
                status: 1,
                msg: "off".into()
            }
        );
        // Missing message is tolerated.
        let h = hello(&mut fake(reply(2))).unwrap();
        assert!(matches!(h, Hello::Refused { status: 2, .. }));
    }

    #[test]
    fn hello_rejects_bad_magic_and_short_reply() {
        let mut r = reply(0);
        r[0] = b'X';
        assert!(hello(&mut fake(r)).is_err());
        let e = hello(&mut fake(b"PBV1".to_vec())).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frames() {
        let mut s = Vec::new();
        s.extend(3u32.to_be_bytes());
        s.extend(1234u64.to_be_bytes());
        s.push(2);
        s.extend([0, 0, 1]);
        s.extend(0u32.to_be_bytes());
        s.extend(5u64.to_be_bytes());
        s.push(1);
        let mut c = io::Cursor::new(s);
        let mut buf = Vec::new();
        let h = next_frame(&mut c, &mut buf).unwrap();
        assert_eq!(
            h,
            FrameHeader {
                len: 3,
                pts_us: 1234,
                flags: 2
            }
        );
        assert_eq!(buf, [0, 0, 1]);
        let h = next_frame(&mut c, &mut buf).unwrap();
        assert_eq!((h.len, h.flags), (0, 1));
        assert!(next_frame(&mut c, &mut buf).is_err());
    }

    #[test]
    fn oversized_frame_is_an_error() {
        let mut h = [0u8; FRAME_HEADER];
        h[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_frame_header(&h).is_err());
    }

    fn info(width: u16, height: u16, rotation: u16) -> Info {
        Info {
            codec: 1,
            width,
            height,
            rotation,
            fps: 0,
        }
    }

    #[test]
    fn decoder_rotates_and_fits() {
        let a = decoder_args(&info(1280, 720, 90), (1280, 720));
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        // Portrait into landscape: blurred fill behind the fitted picture.
        assert!(vf.starts_with("transpose=1,split[a][b];"), "{vf}");
        assert!(vf.contains("boxblur") && vf.contains("overlay"), "{vf}");
        assert!(vf.ends_with(",setsar=1,format=yuyv422"), "{vf}");
        let fr = a.iter().position(|x| x == "-framerate").unwrap();
        assert_eq!(a[fr + 1], "1");
        assert_eq!(a[a.len() - 3..], ["-f", "rawvideo", "-"]);

        // Same aspect: scale (and pad, should the phone's numbers be off).
        let vf = video_filter(&info(1920, 1080, 0), (1280, 720));
        assert!(vf.starts_with("scale=1280:720:force_original"), "{vf}");
        assert!(
            vf.contains("pad=1280:720") && !vf.contains("boxblur"),
            "{vf}"
        );
        let vf = video_filter(&info(720, 1280, 270), (1280, 720));
        assert!(vf.starts_with("transpose=2,scale="), "{vf}");
        assert_eq!(rotate_filter(180), Some("hflip,vflip"));
        assert_eq!(rotate_filter(0), None);
    }

    #[test]
    fn writer_has_a_fixed_format() {
        let a = writer_args((640, 480), Path::new("/dev/video10"));
        let at = |k: &str| &a[a.iter().position(|x| x == k).unwrap() + 1];
        assert_eq!(at("-video_size"), "640x480");
        assert_eq!(at("-pix_fmt"), "yuyv422");
        assert_eq!(a.last().unwrap(), "/dev/video10");
        assert_eq!(frame_size((640, 480)), 640 * 480 * 2);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert_eq!(parse_size("720X1280"), Ok((720, 1280)));
        for bad in ["1280", "1281x720", "x720", "64x64", "8192x4320", "axb"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn feed_frames_repeat_then_placeholder() {
        let feed = Feed::default();
        let mut out = Vec::new();
        let tick = Duration::from_millis(1);
        // Nothing streamed yet.
        assert_eq!(feed.next(&mut out, tick, GRACE), Next::Placeholder);
        let mut buf = vec![7u8; 4];
        feed.push(&mut buf);
        assert_eq!(feed.next(&mut out, tick, GRACE), Next::Frame);
        assert_eq!(out, [7; 4]);
        // Stream paused: the last frame again, then the placeholder.
        assert_eq!(feed.next(&mut out, tick, GRACE), Next::Repeat);
        assert_eq!(feed.next(&mut out, tick, Duration::ZERO), Next::Placeholder);
        // Newest frame wins.
        feed.push(&mut vec![1u8; 4]);
        feed.push(&mut vec![2u8; 4]);
        assert_eq!(feed.next(&mut out, tick, GRACE), Next::Frame);
        assert_eq!(out, [2; 4]);
        feed.stop();
        assert_eq!(feed.next(&mut out, GRACE, GRACE), Next::Stop);
    }

    fn sysfs(tag: &str, devs: &[(&str, &str, bool)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("phonebridge-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (node, name, lo) in devs {
            let d = root.join(node);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("name"), format!("{name}\n")).unwrap();
            if *lo {
                fs::write(d.join("max_openers"), "10\n").unwrap();
            }
        }
        root
    }

    #[test]
    fn device_prefers_labelled_loopback() {
        let root = sysfs(
            "label",
            &[
                ("video0", "Integrated Camera", false),
                ("video2", "OBS Virtual Camera", true),
                ("video10", "Phonebridge", true),
            ],
        );
        assert_eq!(find_device(&root).unwrap(), Path::new("/dev/video10"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn device_falls_back_to_any_loopback() {
        let root = sysfs(
            "any",
            &[
                ("video0", "Integrated Camera", false),
                ("video11", "Dummy video device (0x0000)", false),
                ("video3", "Some Loopback", true),
            ],
        );
        assert_eq!(find_device(&root).unwrap(), Path::new("/dev/video3"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn device_missing() {
        let root = sysfs("none", &[("video0", "Phonebridge", false)]);
        assert!(
            find_device(&root)
                .unwrap_err()
                .contains("modprobe v4l2loopback")
        );
        fs::remove_dir_all(root).unwrap();
        assert!(find_device(Path::new("/nonexistent")).is_err());
    }
}
