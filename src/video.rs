//! Webcam: the phone's camera as H.264, decoded by ffmpeg into a v4l2loopback device.
//!
//! A separate TCP stream from the audio one, phone = server, so either can drop
//! without taking the other down. Over USB it goes through its own `adb
//! forward`; Bluetooth has no video (too little bandwidth).
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
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

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

pub struct Config {
    pub transport: Transport,
    pub port: u16,
    pub device: PathBuf,
    /// Return after the first session (or failed attempt) instead of retrying.
    pub once: bool,
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

/// ffmpeg filter that turns the decoded image upright.
fn rotate_filter(rotation: u16) -> Option<&'static str> {
    match rotation {
        90 => Some("transpose=1"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=2"),
        _ => None,
    }
}

/// Raw Annex-B on stdin, decoded with as little buffering as ffmpeg allows.
/// low_delay disables frame threading, which would hold frames back; with
/// fpsprobesize 0 probing ends at the first decoded frame instead of waiting
/// for 20 to estimate a rate we already know. (A tiny -probesize stops before
/// the first full access unit and fails; -fflags nobuffer was seen dropping
/// frames.)
fn ffmpeg_args(info: &Info, device: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
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
    ]
    .map(String::from)
    .into();
    a.push(info.fps.max(1).to_string());
    a.extend(["-i", "-"].map(String::from));
    if let Some(vf) = rotate_filter(info.rotation) {
        a.extend(["-vf".into(), vf.into()]);
    }
    a.extend(
        [
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "v4l2",
        ]
        .map(String::from),
    );
    a.push(device.display().to_string());
    a
}

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

/// Connect and stream until the phone goes away. `Err` is fatal (ffmpeg missing).
fn session(cfg: &Config, slot: &Mutex<Option<Child>>) -> Result<Outcome, String> {
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
    log!(
        "camera: streaming {}x{} at {} fps, rotation {}°, to {}",
        info.width,
        info.height,
        info.fps,
        info.rotation,
        cfg.device.display()
    );

    let mut ff = Command::new("ffmpeg")
        .args(ffmpeg_args(&info, &cfg.device))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e} (needed for --camera)"))?;
    let mut stdin = ff.stdin.take().expect("piped stdin");
    *slot.lock().unwrap() = Some(ff);

    let mut pump = || -> io::Result<()> {
        conn.set_timeout(Duration::from_secs(10))?;
        let mut buf = Vec::new();
        loop {
            next_frame(&mut conn, &mut buf)?;
            stdin
                .write_all(&buf)
                .map_err(|e| io::Error::new(e.kind(), format!("ffmpeg stopped: {e}")))?;
        }
    };
    let msg = pump().unwrap_err().to_string();
    // EOF lets ffmpeg flush and exit by itself; only nudge it if it lingers.
    drop(stdin);
    let ff = slot.lock().unwrap().take();
    if let Some(mut ff) = ff
        && !exited(&mut ff, 10)
    {
        stop_child(ff);
    }
    Ok(Outcome {
        streamed: true,
        msg,
    })
}

/// The camera's reconnect loop, independent of the audio one. Returns after
/// one attempt with `once`; `Err` only for fatal problems.
pub fn run(cfg: &Config, slot: &Mutex<Option<Child>>) -> Result<(), String> {
    let mut last = String::new();
    loop {
        let o = session(cfg, slot)?;
        if o.streamed {
            log!("camera: stopped: {}", o.msg);
            last.clear();
        } else if o.msg != last {
            log!(
                "camera: {} (retrying quietly every {}s)",
                o.msg,
                RETRY.as_secs()
            );
            last = o.msg;
        }
        if cfg.once {
            return Ok(());
        }
        thread::sleep(RETRY);
    }
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

    #[test]
    fn ffmpeg_rotation() {
        let info = |rotation| Info {
            codec: 1,
            width: 640,
            height: 480,
            rotation,
            fps: 0,
        };
        let a = ffmpeg_args(&info(90), Path::new("/dev/video10"));
        let vf = a.iter().position(|x| x == "-vf").unwrap();
        assert_eq!(a[vf + 1], "transpose=1");
        assert_eq!(a.last().unwrap(), "/dev/video10");
        let fr = a.iter().position(|x| x == "-framerate").unwrap();
        assert_eq!(a[fr + 1], "1");
        assert!(!ffmpeg_args(&info(0), Path::new("x")).contains(&"-vf".into()));
        assert_eq!(rotate_filter(180), Some("hflip,vflip"));
        assert_eq!(rotate_filter(270), Some("transpose=2"));
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
