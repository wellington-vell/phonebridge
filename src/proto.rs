//! WO Mic control/media protocol (USB transport, media endpoint V4).

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::log;

fn err(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn recv_exact(sock: &mut TcpStream, buf: &mut [u8]) -> io::Result<()> {
    sock.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => io::Error::new(e.kind(), "connection closed by phone"),
        _ => e,
    })
}

fn send_msg(sock: &mut TcpStream, mtype: u8, payload: &[u8]) -> io::Result<()> {
    let mut msg = Vec::with_capacity(5 + payload.len());
    msg.push(mtype);
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    sock.write_all(&msg)
}

fn recv_reply(sock: &mut TcpStream, expected: u8) -> io::Result<Vec<u8>> {
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

/// hello -> set codec -> start. Returns the media endpoint version.
pub fn handshake(sock: &mut TcpStream) -> io::Result<u8> {
    send_msg(sock, 0x65, &[4, 6, 3, 0, 0, 0])?;
    let reply = recv_reply(sock, 0x65)?;
    let version = reply.get(1).copied().unwrap_or(4).min(4);
    log!("hello ok, media endpoint version {version}");

    let mut codec = vec![2, 2];
    codec.extend_from_slice(&0u32.to_be_bytes()); // UDP media port, unused over USB
    send_msg(sock, 0x66, &codec)?;
    recv_reply(sock, 0x66)?;
    log!("codec parameters set");

    send_msg(sock, 0x67, &[])?;
    recv_reply(sock, 0x67)?;
    log!("streaming started");
    Ok(version)
}

pub struct Frame {
    pub seq: u16,
    #[allow(dead_code)]
    pub timestamp: u32,
    pub opus: Vec<u8>,
}

/// Read the next V4 media frame, skipping anything that isn't one:
/// `[0x04][0x00][len u16][seq u16][timestamp u32][flags u8][opus: len-7 bytes]`
pub fn next_frame(sock: &mut TcpStream) -> io::Result<Frame> {
    loop {
        let mut hdr = [0u8; 11];
        recv_exact(sock, &mut hdr)?;
        let (mtype, zero) = (hdr[0], hdr[1]);
        let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        let seq = u16::from_be_bytes([hdr[4], hdr[5]]);
        let timestamp = u32::from_be_bytes([hdr[6], hdr[7], hdr[8], hdr[9]]);
        let mut opus = vec![0u8; len.saturating_sub(7)];
        recv_exact(sock, &mut opus)?;
        if mtype != 4 || zero != 0 {
            log!("skipping non-media frame type=0x{mtype:02x} len={len}");
            continue;
        }
        return Ok(Frame {
            seq,
            timestamp,
            opus,
        });
    }
}
