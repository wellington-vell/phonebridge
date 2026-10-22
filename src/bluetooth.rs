//! Bluetooth transport: an RFCOMM socket (BlueZ) to the phone.
//!
//! The RFCOMM channel is looked up with a minimal SDP client: one
//! ServiceSearchAttribute request for the service UUID over L2CAP PSM 1,
//! asking only for the ProtocolDescriptorList. The phone must already be
//! paired (bluetoothctl / your desktop's Bluetooth settings).

use std::ffi::c_int;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::log;

const AF_BLUETOOTH: c_int = 31;
const BTPROTO_L2CAP: c_int = 0;
const BTPROTO_RFCOMM: c_int = 3;
const SDP_PSM: u16 = 1;

/// Service registered by the WeMic Android app.
pub const SERVICE_UUID: [u8; 16] = [
    0xaa, 0xbe, 0x37, 0x90, 0xb3, 0xb9, 0x40, 0x0f, 0x8a, 0x50, 0xe9, 0xb0, 0xf3, 0x8a, 0xc6, 0xb0,
];
/// Serial Port Profile, tried when the service above isn't registered.
const SPP_UUID: [u8; 16] = [
    0x00, 0x00, 0x11, 0x01, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb,
];

/// `bdaddr_t`: the address bytes in reverse (little-endian) order.
pub type BdAddr = [u8; 6];

#[repr(C)]
struct SockaddrRc {
    family: u16,
    bdaddr: BdAddr,
    channel: u8,
}

#[repr(C)]
struct SockaddrL2 {
    family: u16,
    psm: u16,
    bdaddr: BdAddr,
    cid: u16,
    bdaddr_type: u8,
}

/// Parse `AA:BB:CC:DD:EE:FF`.
pub fn parse_addr(s: &str) -> Result<BdAddr, String> {
    let bad = || format!("invalid Bluetooth address: {s} (expected AA:BB:CC:DD:EE:FF)");
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(bad());
    }
    let mut addr = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        addr[5 - i] = u8::from_str_radix(p, 16).map_err(|_| bad())?;
    }
    Ok(addr)
}

fn socket(ty: c_int, proto: c_int) -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the fd is owned right away.
    let fd = unsafe { libc::socket(AF_BLUETOOTH, ty | libc::SOCK_CLOEXEC, proto) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EAFNOSUPPORT) => io::Error::other("Bluetooth is not available (no BlueZ?)"),
            _ => e,
        });
    }
    // SAFETY: fd is a fresh, valid descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn connect<T>(fd: &OwnedFd, addr: &T) -> io::Result<()> {
    // SAFETY: `addr` is a properly initialised sockaddr of the right size.
    let r = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (addr as *const T).cast(),
            mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read and write timeouts for a Bluetooth socket.
pub fn set_timeout(fd: &impl AsRawFd, timeout: Duration) -> io::Result<()> {
    let tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: setsockopt(2) with a valid timeval of the declared size.
        let r = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&tv as *const libc::timeval).cast(),
                mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// ServiceSearchAttributeRequest for `uuid`, attribute 0x0004 (ProtocolDescriptorList).
fn sdp_request(uuid: &[u8; 16], tid: u16, cont: &[u8]) -> Vec<u8> {
    let mut params = vec![0x35, 17, 0x1c];
    params.extend_from_slice(uuid);
    params.extend_from_slice(&0xffffu16.to_be_bytes()); // max attribute bytes
    params.extend_from_slice(&[0x35, 3, 0x09, 0x00, 0x04]);
    params.extend_from_slice(cont);
    let mut pdu = vec![0x06];
    pdu.extend_from_slice(&tid.to_be_bytes());
    pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
    pdu.extend_from_slice(&params);
    pdu
}

/// Ask the phone's SDP server which RFCOMM channel serves `uuid`.
fn sdp_channel(addr: &BdAddr, uuid: &[u8; 16]) -> io::Result<Option<u8>> {
    let fd = socket(libc::SOCK_SEQPACKET, BTPROTO_L2CAP)?;
    set_timeout(&fd, Duration::from_secs(5))?;
    connect(
        &fd,
        &SockaddrL2 {
            family: AF_BLUETOOTH as u16,
            psm: SDP_PSM.to_le(),
            bdaddr: *addr,
            cid: 0,
            bdaddr_type: 0,
        },
    )?;
    let mut sock = File::from(fd);
    let mut lists = Vec::new();
    let mut cont = vec![0u8];
    for tid in 1..=16u16 {
        sock.write_all(&sdp_request(uuid, tid, &cont))?;
        let mut buf = [0u8; 1024];
        let n = sock.read(&mut buf)?;
        let pdu = &buf[..n];
        // [0x07][tid u16][len u16][byte count u16][attribute lists][continuation]
        if n < 7 || pdu[0] != 0x07 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unexpected SDP response 0x{:02x}",
                    pdu.first().unwrap_or(&0)
                ),
            ));
        }
        let count = u16::from_be_bytes([pdu[5], pdu[6]]) as usize;
        let end = (7 + count).min(n);
        lists.extend_from_slice(&pdu[7..end]);
        cont = pdu.get(end..).unwrap_or(&[0]).to_vec();
        if cont.first().copied().unwrap_or(0) == 0 {
            break;
        }
    }
    // RFCOMM protocol descriptor: UUID16 0x0003 followed by uint8 channel.
    Ok(lists
        .windows(5)
        .find(|w| w[..4] == [0x19, 0x00, 0x03, 0x08])
        .map(|w| w[4]))
}

pub fn display(addr: &BdAddr) -> String {
    addr.iter()
        .rev()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Connect to the phone's RFCOMM service. `channel` skips the SDP lookup.
pub fn open(addr: &BdAddr, channel: Option<u8>) -> io::Result<File> {
    let channel = match channel {
        Some(c) => c,
        None => match sdp_channel(addr, &SERVICE_UUID)? {
            Some(c) => c,
            None => sdp_channel(addr, &SPP_UUID)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "WeMic service not found on the phone (is the app started with transport=Bluetooth?)",
                )
            })?,
        },
    };
    log!("connecting to {} RFCOMM channel {channel}", display(addr));
    let fd = socket(libc::SOCK_STREAM, BTPROTO_RFCOMM)?;
    connect(
        &fd,
        &SockaddrRc {
            family: AF_BLUETOOTH as u16,
            bdaddr: *addr,
            channel,
        },
    )?;
    Ok(File::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_roundtrip() {
        let a = parse_addr("00:1A:7d:DA:71:13").unwrap();
        assert_eq!(a, [0x13, 0x71, 0xda, 0x7d, 0x1a, 0x00]);
        assert_eq!(display(&a), "00:1A:7D:DA:71:13");
        assert!(parse_addr("00:11:22").is_err());
    }

    #[test]
    fn request_layout() {
        let pdu = sdp_request(&SERVICE_UUID, 1, &[0]);
        assert_eq!(pdu[0], 0x06);
        assert_eq!(u16::from_be_bytes([pdu[3], pdu[4]]) as usize, pdu.len() - 5);
        assert_eq!(&pdu[5..8], &[0x35, 17, 0x1c]);
    }
}
