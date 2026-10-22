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
//!
//! All of this runs inside the PBA1 secure channel (secure.rs): a client
//! must prove it knows the pairing code before anything else, then every
//! byte (and every UDP datagram) is encrypted. Plaintext clients (the
//! official WO Mic app) are only accepted when the user allowed them.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::capture::{Capture, Source};
use crate::encoder::{Encoder, FRAME};
use crate::secure::{self, Key, RecordReader, RecordWriter, STEP_TIMEOUT, UdpSealer};

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

/// Who may connect.
pub struct Auth {
    /// Pairing code, canonical form (see secure.rs).
    pub code: String,
    /// Also accept plaintext clients without the code (official WO Mic
    /// client). Off by default: anyone who can reach the port could listen.
    pub allow_unauthenticated: bool,
}

/// Connections doing their handshake at once; more are closed right away,
/// so a connection flood can't exhaust threads.
const MAX_PENDING: usize = 4;
/// A handshake (PBA1 and the WO Mic hello) that takes longer is cut off, so
/// slow clients can't hold the pending slots.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);

/// Reading side of a client connection.
enum Reader {
    /// Plaintext, starting with the 4 bytes read to tell it from PBA1.
    Plain(io::Chain<io::Cursor<[u8; 4]>, TcpStream>),
    Secure(Box<RecordReader<TcpStream>>),
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Reader::Plain(r) => r.read(buf),
            Reader::Secure(r) => r.read(buf),
        }
    }
}

/// Writing side of a client connection.
enum Writer {
    Plain(TcpStream),
    Secure(Box<RecordWriter<TcpStream>>),
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Writer::Plain(w) => w.write(buf),
            Writer::Secure(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Writer::Plain(w) => w.flush(),
            Writer::Secure(w) => w.flush(),
        }
    }
}

/// Both sides as one stream, for the handshake.
struct Duplex<'a, R, W>(&'a mut R, &'a mut W);

impl<R: Read, W> Read for Duplex<'_, R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R, W: Write> Write for Duplex<'_, R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.1.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.1.flush()
    }
}

/// A client that completed its handshake, ready to stream.
struct Ready {
    sock: TcpStream,
    reader: Reader,
    writer: Writer,
    peer: String,
    /// UDP media port the client asked for (0: inline media).
    udp_port: u16,
    /// UDP media key, for an authenticated client.
    udp_key: Option<Key>,
}

/// A connection doing its handshake on its own thread.
struct Pending {
    sock: TcpStream,
    started: Instant,
    thread: thread::JoinHandle<Option<Ready>>,
}

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

/// Listen until `shared.stop` is set. One client streams at a time. A new
/// connection replaces the current one, so a PC that vanished without
/// closing its connection can't block it from reconnecting, but only once it
/// completed its handshake (with the pairing code unless `auth` allows
/// clients without it): anything else can't cut off the current client.
pub fn serve(
    transport: Transport,
    port: u16,
    auth: Auth,
    shared: Arc<Shared>,
    on_connected: OnConnected,
) {
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

    let auth = Arc::new(auth);
    let mut active: Option<Active> = None;
    let mut pending: Vec<Pending> = Vec::new();
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, addr)) => {
                let peer = match transport {
                    Transport::Bluetooth => "Bluetooth".to_string(),
                    Transport::Usb => "USB".to_string(),
                    Transport::Wifi => addr.ip().to_string(),
                };
                if pending.len() >= MAX_PENDING {
                    eprintln!("[phonebridge] {peer}: too many pending connections, closed");
                } else {
                    match start_pending(sock, peer, &auth, &shared) {
                        Ok(p) => pending.push(p),
                        Err(e) => shared.fail(e),
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                shared.fail(format!("accept: {e}"));
                thread::sleep(Duration::from_secs(1));
            }
        }

        // A client that completed its handshake takes over from the current one.
        let mut i = 0;
        while i < pending.len() {
            if !pending[i].thread.is_finished() {
                if pending[i].started.elapsed() > HANDSHAKE_DEADLINE {
                    let _ = pending[i].sock.shutdown(Shutdown::Both);
                }
                i += 1;
                continue;
            }
            let Ok(Some(ready)) = pending.swap_remove(i).thread.join() else {
                continue;
            };
            if let Some(old) = active.take() {
                old.end();
            }
            match start_session(ready, &shared, &on_connected) {
                Ok(a) => active = Some(a),
                Err(e) => shared.fail(e),
            }
        }
        if active.as_ref().is_some_and(|a| a.thread.is_finished()) {
            active.take().unwrap().end();
        }
    }
    for p in pending {
        let _ = p.sock.shutdown(Shutdown::Both);
        let _ = p.thread.join();
    }
    if let Some(a) = active.take() {
        a.end();
    }
    shared.update(|s| {
        s.state = State::Idle;
        s.peer = None;
    });
}

fn start_pending(
    sock: TcpStream,
    peer: String,
    auth: &Arc<Auth>,
    shared: &Arc<Shared>,
) -> Result<Pending, String> {
    let handle = sock.try_clone().map_err(|e| e.to_string())?;
    let (auth, shared) = (Arc::clone(auth), Arc::clone(shared));
    let thread = thread::Builder::new()
        .name("phonebridge-handshake".into())
        .spawn(move || match establish(sock, &peer, &auth) {
            Ok(r) => Some(r),
            Err(e) => {
                // Refused clients are shown; anything else (port scans,
                // timeouts) is only logged, to not bury the current session.
                if matches!(e.kind(), io::ErrorKind::PermissionDenied) {
                    shared.fail(format!("{peer}: {e}"));
                } else {
                    eprintln!("[phonebridge] {peer}: handshake: {e}");
                }
                None
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Pending {
        sock: handle,
        started: Instant::now(),
        thread,
    })
}

/// PBA1 (or plaintext if allowed) and the WO Mic handshake, each step
/// within [`STEP_TIMEOUT`].
fn establish(mut sock: TcpStream, peer: &str, auth: &Auth) -> io::Result<Ready> {
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(STEP_TIMEOUT))?;
    sock.set_write_timeout(Some(STEP_TIMEOUT))?;
    let mut magic = [0u8; 4];
    sock.read_exact(&mut magic)?;
    let (mut reader, mut writer, udp_key) = if &magic == secure::MAGIC {
        let keys = secure::accept(&mut sock, &auth.code)?;
        (
            Reader::Secure(Box::new(RecordReader::new(
                sock.try_clone()?,
                &keys.key_c2s,
            ))),
            Writer::Secure(Box::new(RecordWriter::new(
                sock.try_clone()?,
                &keys.key_s2c,
            ))),
            Some(keys.key_udp),
        )
    } else if auth.allow_unauthenticated {
        (
            Reader::Plain(io::Cursor::new(magic).chain(sock.try_clone()?)),
            Writer::Plain(sock.try_clone()?),
            None,
        )
    } else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refused a client without the pairing code",
        ));
    };
    let udp_port = handshake(&mut Duplex(&mut reader, &mut writer))?;
    Ok(Ready {
        sock,
        reader,
        writer,
        peer: peer.to_string(),
        udp_port,
        udp_key,
    })
}

fn start_session(
    ready: Ready,
    shared: &Arc<Shared>,
    on_connected: &OnConnected,
) -> Result<Active, String> {
    let handle = ready.sock.try_clone().map_err(|e| e.to_string())?;
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
                if let Err(e) = session(ready, &shared, &*on_connected, &cancel)
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

/// UDP media socket, encrypting for an authenticated client.
struct Udp {
    sock: UdpSocket,
    sealer: Option<UdpSealer>,
    buf: Vec<u8>,
}

impl Udp {
    /// A refused or failed datagram is not fatal; the control stream decides.
    fn send(&mut self, frame: &[u8]) {
        let datagram = match &mut self.sealer {
            Some(s) => match s.seal(frame, &mut self.buf) {
                Ok(()) => &self.buf,
                Err(_) => return,
            },
            None => frame,
        };
        let _ = self.sock.send(datagram);
    }
}

fn session(
    ready: Ready,
    shared: &Shared,
    on_connected: &(dyn Fn(bool) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let Ready {
        sock,
        mut reader,
        mut writer,
        peer,
        udp_port,
        udp_key,
    } = ready;
    let io_err = |e: io::Error| format!("{peer}: {e}");
    // A PC that stops reading must not block the stream forever.
    sock.set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(io_err)?;
    let mut udp = match udp_port {
        0 => None,
        port => {
            let ip = sock.peer_addr().map_err(io_err)?.ip();
            let u = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(io_err)?;
            u.connect((ip, port)).map_err(io_err)?;
            Some(Udp {
                sock: u,
                sealer: udp_key.as_ref().map(UdpSealer::new),
                buf: Vec::with_capacity(1500),
            })
        }
    };

    // After start the client only sends keepalives (if it supports them).
    // EOF, an error or missing keepalives mean it went away; needed with UDP
    // media, where sends don't fail. The watcher is the only reader from
    // here on, the stream below the only writer.
    let closed = Arc::new(AtomicBool::new(false));
    sock.set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(io_err)?;
    let watch_closed = Arc::clone(&closed);
    thread::spawn(move || {
        watch_control(&mut reader);
        watch_closed.store(true, Ordering::Relaxed);
    });

    shared.update(|s| {
        s.state = State::Connected;
        s.peer = Some(peer.clone());
        s.error = None;
    });
    on_connected(true);
    let result = stream(&mut writer, udp.as_mut(), shared, &closed, cancel);
    on_connected(false);
    let _ = sock.shutdown(Shutdown::Both);
    result
}

/// Return when the client is gone: EOF, error or keepalive timeout.
fn watch_control(sock: &mut impl Read) {
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
    sock: &mut impl Write,
    mut udp: Option<&mut Udp>,
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
        match udp.as_mut() {
            Some(u) => u.send(&packet),
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

    const CODE: &str = "ABCDEFGHJKMNPQRSTVWX";

    fn hello() -> Vec<u8> {
        let mut m = vec![0x65, 0, 0, 0, 6, 4, 6, 3, 0, 0, 0];
        m.extend_from_slice(&[0x66, 0, 0, 0, 6, 2, 2, 0, 0, 0, 0]);
        m.extend_from_slice(&[0x67, 0, 0, 0, 0]);
        m
    }

    /// `establish` on a server socket while `client` talks to it.
    fn with_client<T: Send + 'static>(
        allow_unauthenticated: bool,
        client: impl FnOnce(TcpStream) -> T + Send + 'static,
    ) -> (io::Result<Ready>, T) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || client(TcpStream::connect(addr).unwrap()));
        let (sock, _) = listener.accept().unwrap();
        let auth = Auth {
            code: CODE.into(),
            allow_unauthenticated,
        };
        let ready = establish(sock, "test", &auth);
        (ready, client.join().unwrap())
    }

    #[test]
    fn plaintext_needs_opt_in() {
        let (ready, _) = with_client(false, |mut c| {
            let _ = c.write_all(&hello());
        });
        assert_eq!(ready.err().unwrap().kind(), io::ErrorKind::PermissionDenied);

        let (ready, reply) = with_client(true, |mut c| {
            c.write_all(&hello()).unwrap();
            let mut reply = [0u8; 23];
            c.read_exact(&mut reply).unwrap();
            reply
        });
        let ready = ready.unwrap();
        assert!(ready.udp_key.is_none());
        assert_eq!(reply[..5], [0x65, 0, 0, 0, 6]);
    }

    #[test]
    fn secure_session() {
        let (ready, (keys, reply, keepalive)) = with_client(false, |mut c| {
            let nc = [1u8; 16];
            let mut m = secure::MAGIC.to_vec();
            m.extend_from_slice(&nc);
            c.write_all(&m).unwrap();
            let mut ns = [0u8; 16];
            c.read_exact(&mut ns).unwrap();
            let keys = secure::derive(CODE, &nc, &ns);
            c.write_all(&keys.auth_c2s).unwrap();
            let mut proof = [0u8; 32];
            c.read_exact(&mut proof).unwrap();
            assert_eq!(proof, keys.auth_s2c);
            let mut w = RecordWriter::new(c.try_clone().unwrap(), &keys.key_c2s);
            // Records need not line up with messages.
            let msgs = hello();
            w.write_all(&msgs[..3]).unwrap();
            w.write_all(&msgs[3..]).unwrap();
            let mut r = RecordReader::new(c, &keys.key_s2c);
            let mut reply = [0u8; 23];
            r.read_exact(&mut reply).unwrap();
            w.write_all(&[KEEPALIVE, 0, 0, 0, 0]).unwrap();
            (keys, reply, w)
        });
        let mut ready = ready.unwrap();
        assert_eq!(ready.udp_key, Some(keys.key_udp));
        assert_eq!(
            reply[..11],
            [0x65, 0, 0, 0, 6, 0, 4, b'w', b'm', b'k', b'a']
        );
        let (mtype, _) = read_msg(&mut ready.reader).unwrap();
        assert_eq!(mtype, KEEPALIVE);
        drop(keepalive);
    }
}
