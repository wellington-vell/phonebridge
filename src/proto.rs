//! WO Mic control/media protocol (media endpoint V4).
//!
//! The same messages are used over every transport: USB (TCP through `adb
//! forward`), Wi-Fi (TCP straight to the phone) and Bluetooth (RFCOMM). Media
//! normally follows on the control stream; over Wi-Fi it can be sent to a UDP
//! port instead, with the same frame layout, one frame per datagram.

use std::io::{self, Read, Write};

use crate::log;

fn err(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn recv_exact(sock: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    sock.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => io::Error::new(e.kind(), "connection closed by phone"),
        _ => e,
    })
}

fn send_msg(sock: &mut impl Write, mtype: u8, payload: &[u8]) -> io::Result<()> {
    let mut msg = Vec::with_capacity(5 + payload.len());
    msg.push(mtype);
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    sock.write_all(&msg)
}

fn recv_reply(sock: &mut impl Read, expected: u8) -> io::Result<Vec<u8>> {
    let mut hdr = [0u8; 5];
    recv_exact(sock, &mut hdr)?;
    let mtype = hdr[0];
    let len = u32::from_be_bytes(hdr[1..5].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len];
    recv_exact(sock, &mut payload)?;
    if mtype != expected {
        return Err(err(format!(
            "unexpected reply type 0x{mtype:02x} (wanted 0x{expected:02x})"
        )));
    }
    match payload.first() {
        Some(&status) if status != 0 => Err(err(format!(
            "phone rejected 0x{expected:02x} with status {status}"
        ))),
        _ => Ok(payload),
    }
}

/// Extension of the WeMic Android app: a hello reply ending in this marker
/// asks for a `KEEPALIVE` message every second after start, so the phone
/// notices a PC that vanished without closing the connection.
const KEEPALIVE_MARKER: &[u8] = b"wmka";
const KEEPALIVE: u8 = 0x70;

pub struct Session {
    /// The phone wants keepalives (see `keepalive`).
    pub keepalive: bool,
}

/// hello -> set codec -> start.
///
/// `udp_port` asks the phone to send media to that UDP port; 0 keeps it on
/// this stream.
pub fn handshake(sock: &mut (impl Read + Write), udp_port: u16) -> io::Result<Session> {
    send_msg(sock, 0x65, &[4, 6, 3, 0, 0, 0])?;
    let reply = recv_reply(sock, 0x65)?;
    let version = reply.get(1).copied().unwrap_or(4).min(4);
    let keepalive = reply.ends_with(KEEPALIVE_MARKER);
    log!("hello ok, media endpoint version {version}");

    let mut codec = vec![2, 2];
    codec.extend_from_slice(&u32::from(udp_port).to_be_bytes());
    send_msg(sock, 0x66, &codec)?;
    recv_reply(sock, 0x66)?;
    log!("codec parameters set");

    send_msg(sock, 0x67, &[])?;
    recv_reply(sock, 0x67)?;
    log!("streaming started");
    Ok(Session { keepalive })
}

pub fn keepalive(sock: &mut impl Write) -> io::Result<()> {
    send_msg(sock, KEEPALIVE, &[])
}

pub struct Frame {
    pub seq: u16,
    #[allow(dead_code)]
    pub timestamp: u32,
    pub opus: Vec<u8>,
}

/// V4 media frame header:
/// `[0x04][0x00][len u16][seq u16][timestamp u32][flags u8]`, then `len-7` bytes of Opus.
const HEADER: usize = 11;

struct Header {
    media: bool,
    len: usize,
    seq: u16,
    timestamp: u32,
}

fn parse_header(hdr: &[u8]) -> Header {
    Header {
        media: hdr[0] == 4 && hdr[1] == 0,
        len: u16::from_be_bytes([hdr[2], hdr[3]]) as usize,
        seq: u16::from_be_bytes([hdr[4], hdr[5]]),
        timestamp: u32::from_be_bytes([hdr[6], hdr[7], hdr[8], hdr[9]]),
    }
}

/// Read the next V4 media frame from a stream, skipping anything that isn't one.
pub fn next_frame(sock: &mut impl Read) -> io::Result<Frame> {
    loop {
        let mut hdr = [0u8; HEADER];
        recv_exact(sock, &mut hdr)?;
        let h = parse_header(&hdr);
        let mut opus = vec![0u8; h.len.saturating_sub(7)];
        recv_exact(sock, &mut opus)?;
        if !h.media {
            log!(
                "skipping non-media frame type=0x{:02x} len={}",
                hdr[0],
                h.len
            );
            continue;
        }
        return Ok(Frame {
            seq: h.seq,
            timestamp: h.timestamp,
            opus,
        });
    }
}

/// Parse a media frame received as a single UDP datagram.
pub fn parse_datagram(buf: &[u8]) -> Option<Frame> {
    if buf.len() < HEADER {
        return None;
    }
    let h = parse_header(&buf[..HEADER]);
    let end = HEADER + h.len.saturating_sub(7);
    if !h.media || buf.len() < end {
        return None;
    }
    Some(Frame {
        seq: h.seq,
        timestamp: h.timestamp,
        opus: buf[HEADER..end].to_vec(),
    })
}
