//! Phone side of the WO Mic protocol: accept the PC client on the control
//! port, answer the handshake and stream Opus frames.
//!
//!   Control messages: [type u8][len u32 BE][payload]
//!     0x65 hello   <- 04 06 03 00 00 00     -> [status 0][media version 4]
//!     0x66 codec   <- 02 02 [UDP port u32]  -> [status 0]
//!     0x67 start   <-                       -> [status 0]
//!   Extension: the hello reply ends with "wmka", telling the Phonebridge client to
//!   send 0x70 keepalives (empty payload) every second after start. A client
//!   that sent one and then goes quiet for KEEPALIVE_TIMEOUT is dropped, so a
//!   PC that vanished without closing the connection (suspend, Wi-Fi drop,
//!   Bluetooth out of range) doesn't keep the session and screen alive.
//!   Media (inline, or one per datagram to the UDP port when it isn't 0):
//!     [0x04][0x00][len u16][seq u16][timestamp u32][flags u8][opus: len-7 bytes]
//!
//! Every transport ends up here as TCP: USB through `adb forward`, Wi-Fi
//! directly, Bluetooth through the RFCOMM bridge in the Android plugin.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::capture::{Capture, Source};
use crate::encoder::{Encoder, FRAME};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Usb,
    Wifi,
    Bluetooth,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Idle,
    Listening,
    Connected,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub state: State,
    /// Who is connected.
    pub peer: Option<String>,
    /// Last error, cleared when a client connects.
    pub error: Option<String>,
}

/// State shared between the UI commands and the streaming thread.
#[derive(Default)]
pub struct Shared {
    gain: AtomicU32,
    pub muted: AtomicBool,
    level: AtomicU32,
    pub source: Mutex<Source>,
    pub status: Mutex<Status>,
    pub stop: AtomicBool,
}

impl Shared {
    pub fn new() -> Self {
        let s = Self::default();
        s.set_gain(1.0);
        s
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    /// Peak of the last frame sent, 0..1 (after gain, 0 when muted).
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }

    fn fail(&self, e: impl std::fmt::Display) {
        let msg = e.to_string();
        eprintln!("[phonebridge] {msg}");
        self.update(|s| s.error = Some(msg));
    }
}

const KEEPALIVE: u8 = 0x70;
/// Hello reply marker announcing keepalive support.
const KEEPALIVE_MARKER: &[u8; 4] = b"wmka";
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(4);

/// Called with true when a client starts streaming and false when it stops.
pub type OnConnected = Arc<dyn Fn(bool) + Send + Sync>;

/// A client being served on its own thread.
struct Active {
    sock: TcpStream,
    cancel: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl Active {
    fn end(self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.sock.shutdown(Shutdown::Both);
        let _ = self.thread.join();
    }
}

/// Listen until `shared.stop` is set. One client streams at a time; a new
/// connection replaces the current one, so a PC that vanished without
/// closing its connection can't block it from reconnecting.
pub fn serve(transport: Transport, port: u16, shared: Arc<Shared>, on_connected: OnConnected) {
    // USB arrives through adb forward and Bluetooth through the local bridge,
    // so only Wi-Fi needs to be reachable from the network.
    let ip = match transport {
        Transport::Wifi => Ipv4Addr::UNSPECIFIED,
        _ => Ipv4Addr::LOCALHOST,
    };
    let listener = match TcpListener::bind((ip, port)).and_then(|l| {
        l.set_nonblocking(true)?;
        Ok(l)
    }) {
        Ok(l) => l,
        Err(e) => {
            shared.fail(format!("cannot listen on port {port}: {e}"));
            shared.update(|s| s.state = State::Idle);
            return;
        }
    };
    shared.update(|s| s.state = State::Listening);

    let mut active: Option<Active> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, addr)) => {
                if let Some(old) = active.take() {
                    old.end();
                }
                let peer = match transport {
                    Transport::Bluetooth => "Bluetooth".to_string(),
                    Transport::Usb => "USB".to_string(),
                    Transport::Wifi => addr.ip().to_string(),
                };
                match start_session(sock, peer, &shared, &on_connected) {
                    Ok(a) => active = Some(a),
                    Err(e) => shared.fail(e),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if active.as_ref().is_some_and(|a| a.thread.is_finished()) {
                    active.take().unwrap().end();
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                shared.fail(format!("accept: {e}"));
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
    if let Some(a) = active.take() {
        a.end();
    }
    shared.update(|s| {
        s.state = State::Idle;
        s.peer = None;
    });
}

fn start_session(
    sock: TcpStream,
    peer: String,
    shared: &Arc<Shared>,
    on_connected: &OnConnected,
) -> Result<Active, String> {
    let handle = sock.try_clone().map_err(|e| e.to_string())?;
    let cancel = Arc::new(AtomicBool::new(false));
    let thread = {
        let (shared, on_connected, cancel) = (
            Arc::clone(shared),
            Arc::clone(on_connected),
            Arc::clone(&cancel),
        );
        thread::Builder::new()
            .name("phonebridge-session".into())
            .spawn(move || {
                if let Err(e) = session(sock, &peer, &shared, &*on_connected, &cancel)
                    && !cancel.load(Ordering::Relaxed)
                {
                    shared.fail(e);
                }
                shared.set_level(0.0);
                shared.update(|s| {
                    s.state = State::Listening;
                    s.peer = None;
                });
            })
            .map_err(|e| e.to_string())?
    };
    Ok(Active {
        sock: handle,
        cancel,
        thread,
    })
}

fn read_msg(sock: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    sock.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes(hdr[1..5].try_into().unwrap()) as usize;
    if len > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized control message",
        ));
    }
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload)?;
    Ok((hdr[0], payload))
}

fn send_msg(sock: &mut impl Write, mtype: u8, payload: &[u8]) -> io::Result<()> {
    let mut msg = vec![mtype];
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    sock.write_all(&msg)
}

/// Answer hello / codec / start. Returns the UDP media port the client asked
/// for (0: send media on the control stream).
fn handshake(sock: &mut (impl Read + Write)) -> io::Result<u16> {
    let mut udp_port = 0;
    loop {
        let (mtype, payload) = read_msg(sock)?;
        match mtype {
            0x65 => {
                let mut reply = vec![0, 4];
                reply.extend_from_slice(KEEPALIVE_MARKER);
                send_msg(sock, 0x65, &reply)?;
            }
            0x66 => {
                if let Some(p) = payload.get(2..6) {
                    udp_port = u32::from_be_bytes(p.try_into().unwrap()) as u16;
                }
                send_msg(sock, 0x66, &[0])?;
            }
            0x67 => {
                send_msg(sock, 0x67, &[0])?;
                return Ok(udp_port);
            }
            other => send_msg(sock, other, &[1])?,
        }
    }
}

fn session(
    mut sock: TcpStream,
    peer: &str,
    shared: &Shared,
    on_connected: &(dyn Fn(bool) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let io_err = |e: io::Error| format!("{peer}: {e}");
    sock.set_nonblocking(false).map_err(io_err)?;
    sock.set_nodelay(true).map_err(io_err)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(io_err)?;
    let udp_port = handshake(&mut sock).map_err(io_err)?;
    // A PC that stops reading must not block the stream forever.
    sock.set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(io_err)?;
    let udp = match udp_port {
        0 => None,
        port => {
            let ip = sock.peer_addr().map_err(io_err)?.ip();
            let u = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(io_err)?;
            u.connect((ip, port)).map_err(io_err)?;
            Some(u)
        }
    };

    // After start the client only sends keepalives (if it supports them).
    // EOF, an error or missing keepalives mean it went away; needed with UDP
    // media, where sends don't fail.
    let closed = Arc::new(AtomicBool::new(false));
    let mut watch = sock.try_clone().map_err(io_err)?;
    watch
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(io_err)?;
    let watch_closed = Arc::clone(&closed);
    thread::spawn(move || {
        watch_control(&mut watch);
        watch_closed.store(true, Ordering::Relaxed);
    });

    shared.update(|s| {
        s.state = State::Connected;
        s.peer = Some(peer.to_string());
        s.error = None;
    });
    on_connected(true);
    let result = stream(&mut sock, udp.as_ref(), shared, &closed, cancel);
    on_connected(false);
    let _ = sock.shutdown(Shutdown::Both);
    result
}

/// Return when the client is gone: EOF, error or keepalive timeout.
fn watch_control(sock: &mut TcpStream) {
    let mut last_keepalive: Option<Instant> = None;
    let mut pending = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
        // Consume complete messages: [type u8][len u32 BE][payload].
        while pending.len() >= 5 {
            let len = u32::from_be_bytes(pending[1..5].try_into().unwrap()) as usize;
            if len > 64 * 1024 {
                return;
            }
            if pending.len() < 5 + len {
                break;
            }
            if pending[0] == KEEPALIVE {
                last_keepalive = Some(Instant::now());
            }
            pending.drain(..5 + len);
        }
        if last_keepalive.is_some_and(|t| t.elapsed() > KEEPALIVE_TIMEOUT) {
            return;
        }
    }
}

fn stream(
    sock: &mut TcpStream,
    udp: Option<&UdpSocket>,
    shared: &Shared,
    closed: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut enc = Encoder::new()?;
    let mut source = *shared.source.lock().unwrap();
    let mut capture = Some(Capture::open(source)?);
    let mut pcm = [0i16; FRAME];
    let mut packet = Vec::with_capacity(1500);
    let (mut seq, mut timestamp) = (0u16, 0u32);

    let running = || {
        !shared.stop.load(Ordering::Relaxed)
            && !closed.load(Ordering::Relaxed)
            && !cancel.load(Ordering::Relaxed)
    };
    while running() {
        let wanted = *shared.source.lock().unwrap();
        if wanted != source {
            capture.take(); // release the microphone before reopening it
            capture = Some(Capture::open(wanted)?);
            source = wanted;
        }
        capture.as_mut().unwrap().read(&mut pcm)?;

        let gain = if shared.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            shared.gain()
        };
        let mut peak = 0i32;
        for s in pcm.iter_mut() {
            let v = (*s as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            peak = peak.max((v as i32).abs());
            *s = v;
        }
        shared.set_level(peak as f32 / 32768.0);

        let opus = enc.encode(&pcm)?;
        packet.clear();
        packet.extend_from_slice(&[4, 0]);
        packet.extend_from_slice(&((opus.len() + 7) as u16).to_be_bytes());
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.push(0);
        packet.extend_from_slice(opus);
        match udp {
            // A refused datagram is not fatal; the control stream decides.
            Some(u) => {
                let _ = u.send(&packet);
            }
            None => {
                if sock.write_all(&packet).is_err() {
                    return Ok(()); // client went away
                }
            }
        }
        seq = seq.wrapping_add(1);
        timestamp = timestamp.wrapping_add(FRAME as u32);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_replies() {
        // hello, codec with UDP port 5000, start
        let mut input = vec![0x65, 0, 0, 0, 6, 4, 6, 3, 0, 0, 0];
        input.extend_from_slice(&[0x66, 0, 0, 0, 6, 2, 2, 0, 0, 0x13, 0x88]);
        input.extend_from_slice(&[0x67, 0, 0, 0, 0]);
        let mut rw = io::Cursor::new(input);
        let mut out = Vec::new();
        struct Duplex<'a>(&'a mut io::Cursor<Vec<u8>>, &'a mut Vec<u8>);
        impl Read for Duplex<'_> {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Write for Duplex<'_> {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.1.write(b)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let port = handshake(&mut Duplex(&mut rw, &mut out)).unwrap();
        assert_eq!(port, 5000);
        assert_eq!(
            out,
            [
                0x65, 0, 0, 0, 6, 0, 4, b'w', b'm', b'k', b'a', //
                0x66, 0, 0, 0, 1, 0, //
                0x67, 0, 0, 0, 1, 0
            ]
        );
    }
}
