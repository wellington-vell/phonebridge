//! How the client reaches the phone: USB (adb forward), Wi-Fi or Bluetooth.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::time::Duration;

use crate::bluetooth::{self, BdAddr};
use crate::proto::{self, Frame};
use crate::secure;

#[derive(Clone)]
pub enum Transport {
    /// TCP to 127.0.0.1:port, forwarded to the phone by `adb forward`
    /// before every connection attempt when `adb` is set (forwards vanish
    /// when the phone is replugged or the adb server restarts).
    Usb { adb: bool },
    /// TCP straight to the phone's address on the local network.
    Wifi(String),
    /// RFCOMM; the channel comes from SDP unless given.
    Bluetooth(BdAddr, Option<u8>),
}

impl Transport {
    pub fn hint(&self) -> &'static str {
        match self {
            Transport::Usb { .. } => {
                "is Phonebridge (or WO Mic) started on the phone with transport=USB?"
            }
            Transport::Wifi(_) => {
                "is Phonebridge (or WO Mic) started on the phone with transport=Wi-Fi, on the same network?"
            }
            Transport::Bluetooth(..) => {
                "is the phone paired and Phonebridge started with transport=Bluetooth?"
            }
        }
    }
}

/// A connected control stream.
pub enum Conn {
    Tcp(TcpStream),
    Rfcomm(File),
}

impl Conn {
    pub fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => {
                s.set_read_timeout(Some(timeout))?;
                s.set_write_timeout(Some(timeout))
            }
            Conn::Rfcomm(f) => bluetooth::set_timeout(f, timeout),
        }
    }

    /// A second handle on the same stream (for writing from another thread).
    pub fn try_clone(&self) -> io::Result<Conn> {
        Ok(match self {
            Conn::Tcp(s) => Conn::Tcp(s.try_clone()?),
            Conn::Rfcomm(f) => Conn::Rfcomm(f.try_clone()?),
        })
    }

    /// The phone's IP address, for UDP media.
    pub fn peer_ip(&self) -> Option<std::net::IpAddr> {
        match self {
            Conn::Tcp(s) => s.peer_addr().ok().map(|a| a.ip()),
            Conn::Rfcomm(_) => None,
        }
    }

    /// Our end's IP address, to receive UDP media on that interface only.
    pub fn local_ip(&self) -> Option<std::net::IpAddr> {
        match self {
            Conn::Tcp(s) => s.local_addr().ok().map(|a| a.ip()),
            Conn::Rfcomm(_) => None,
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Rfcomm(f) => f.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Rfcomm(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => s.flush(),
            Conn::Rfcomm(f) => f.flush(),
        }
    }
}

// ---------------------------------------------------------------------- adb
/// Run adb; its stderr becomes the error message on failure.
pub fn adb(args: &[&str]) -> io::Result<String> {
    let out = Command::new("adb").args(args).output().map_err(|e| {
        let msg = match e.kind() {
            io::ErrorKind::NotFound => "adb not found (nix-shell -p android-tools)".into(),
            _ => format!("adb: {e}"),
        };
        io::Error::new(e.kind(), msg)
    })?;
    if !out.status.success() {
        // The last line, without "* daemon started" chatter or prefixes.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with('*'))
            .unwrap_or("failed")
            .trim_start_matches("adb: ")
            .trim_start_matches("error: ");
        return Err(io::Error::other(format!(
            "adb {}: {msg} (phone plugged in, USB debugging allowed?)",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `adb forward tcp:P tcp:P`. Idempotent and cheap, so it is simply redone
/// before each USB connection attempt.
pub fn forward(port: u16) -> io::Result<()> {
    let p = format!("tcp:{port}");
    adb(&["forward", &p, &p]).map(drop)
}

pub fn remove_forward(port: u16) {
    let _ = adb(&["forward", "--remove", &format!("tcp:{port}")]);
}

fn tcp(addr: SocketAddr) -> io::Result<Conn> {
    let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    sock.set_nodelay(true)?;
    Ok(Conn::Tcp(sock))
}

pub fn connect(t: &Transport, port: u16) -> io::Result<Conn> {
    match t {
        Transport::Usb { adb } => {
            if *adb {
                forward(port)?;
            }
            tcp(SocketAddr::from(([127, 0, 0, 1], port)))
        }
        Transport::Wifi(host) => {
            let addr = (host.as_str(), port)
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {host}"))
                })?;
            tcp(addr)
        }
        Transport::Bluetooth(addr, channel) => bluetooth::open(addr, *channel).map(Conn::Rfcomm),
    }
}

/// Where media frames arrive after the handshake.
pub enum Media {
    /// On the control stream.
    Inline,
    /// UDP datagrams from the phone; the control stream stays open alongside.
    /// Encrypted when the session is (see `secure`).
    Udp(UdpSocket, Option<std::net::IpAddr>, Option<secure::Udp>),
}

impl Media {
    pub fn next(&mut self, conn: &mut impl Read) -> io::Result<Frame> {
        match self {
            Media::Inline => proto::next_frame(conn),
            Media::Udp(sock, phone, key) => {
                let mut buf = [0u8; 2048];
                loop {
                    let (n, from) = sock.recv_from(&mut buf)?;
                    if phone.is_some_and(|ip| ip != from.ip()) {
                        continue;
                    }
                    let frame = match key {
                        Some(k) => k.open(&buf[..n]).and_then(|p| proto::parse_datagram(&p)),
                        None => proto::parse_datagram(&buf[..n]),
                    };
                    if let Some(frame) = frame {
                        return Ok(frame);
                    }
                }
            }
        }
    }
}
