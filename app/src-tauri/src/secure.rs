//! PBA1 secure channel, phone (server) side; the spec, with test vectors, is
//! docs/secure-channel.md.
//!
//!   C -> S  "PBA1" | Nc (16 bytes)
//!   S -> C  Ns (16 bytes)
//!   C -> S  auth c2s (32 bytes)    checked in constant time
//!   S -> C  auth s2c (32 bytes)
//!
//! Keys come from HKDF-SHA256 over the pairing code with salt Nc | Ns. After
//! the handshake each TCP direction is a stream of AES-256-GCM records
//! ([len u32 BE][ciphertext | tag], nonce = 0u32 | counter u64 BE), and UDP
//! media datagrams are [counter u64 BE][ciphertext | tag].
//!
//! Each direction is one object ([`RecordReader`] / [`RecordWriter`] /
//! [`UdpSealer`]) owning its key and counter, so a nonce can't be reused as
//! long as each is used from one place.

use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// First 4 bytes a secure client sends.
pub const MAGIC: &[u8; 4] = b"PBA1";
/// Crockford base32.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Pairing code length, canonical form.
const CODE_LEN: usize = 20;
/// How long the client may take for each handshake step.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Delay before closing after a wrong proof, to slow down online guessing.
const FAIL_DELAY: Duration = Duration::from_secs(1);
/// Largest plaintext in one record.
const MAX_RECORD: usize = 65536;
const TAG_LEN: usize = 16;

pub type Key = [u8; 32];

fn random(buf: &mut [u8]) -> io::Result<()> {
    getrandom::fill(buf).map_err(|e| io::Error::other(format!("random: {e}")))
}

/// A new random pairing code, display form (`ABCDE-FGHJK-MNPQR-STVWX`).
/// Only used on desktop; the Android app keeps its own.
pub fn generate_code() -> io::Result<String> {
    let mut bytes = [0u8; CODE_LEN];
    random(&mut bytes)?;
    // 256 is a multiple of 32: no bias.
    let code: String = bytes
        .iter()
        .map(|b| ALPHABET[(b & 31) as usize] as char)
        .collect();
    Ok(display(&code))
}

/// Canonical form of a pairing code typed or stored anywhere: separators
/// and whitespace dropped, uppercase, O -> 0, I/L -> 1. None unless that
/// leaves exactly 20 alphabet characters.
pub fn canonical(code: &str) -> Option<String> {
    let code: String = code
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

/// Canonical code as 4 dash-separated groups of 5.
pub fn display(canonical: &str) -> String {
    canonical
        .as_bytes()
        .chunks(5)
        .map(|g| std::str::from_utf8(g).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// Everything derived from the code and both nonces.
pub struct Keys {
    pub auth_c2s: Key,
    pub auth_s2c: Key,
    pub key_c2s: Key,
    pub key_s2c: Key,
    pub key_udp: Key,
}

/// HKDF-SHA256: IKM = canonical code, salt = Nc | Ns, info = "phonebridge v1 " + label.
pub fn derive(code: &str, nc: &[u8; 16], ns: &[u8; 16]) -> Keys {
    let mut salt = [0u8; 32];
    salt[..16].copy_from_slice(nc);
    salt[16..].copy_from_slice(ns);
    let hk = Hkdf::<Sha256>::new(Some(&salt), code.as_bytes());
    let expand = |label: &str| {
        let mut out = [0u8; 32];
        hk.expand(format!("phonebridge v1 {label}").as_bytes(), &mut out)
            .expect("32 bytes is a valid HKDF-SHA256 length");
        out
    };
    Keys {
        auth_c2s: expand("auth c2s"),
        auth_s2c: expand("auth s2c"),
        key_c2s: expand("key c2s"),
        key_s2c: expand("key s2c"),
        key_udp: expand("key udp"),
    }
}

/// Server side of the handshake, after the 4 magic bytes were read. The
/// caller sets the socket timeouts ([`STEP_TIMEOUT`]). A wrong proof fails
/// with `PermissionDenied` after [`FAIL_DELAY`].
pub fn accept(sock: &mut (impl Read + Write), code: &str) -> io::Result<Keys> {
    let mut nc = [0u8; 16];
    sock.read_exact(&mut nc)?;
    let mut ns = [0u8; 16];
    random(&mut ns)?;
    sock.write_all(&ns)?;
    let keys = derive(code, &nc, &ns);
    let mut proof = [0u8; 32];
    sock.read_exact(&mut proof)?;
    if !bool::from(proof.ct_eq(&keys.auth_c2s)) {
        thread::sleep(FAIL_DELAY);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wrong pairing code",
        ));
    }
    sock.write_all(&keys.auth_s2c)?;
    sock.flush()?;
    Ok(keys)
}

/// One direction's AES-256-GCM key and nonce counter.
struct Cipher {
    aead: Aes256Gcm,
    counter: u64,
}

impl Cipher {
    fn new(key: &Key) -> Self {
        Self {
            aead: Aes256Gcm::new(key.into()),
            counter: 0,
        }
    }

    /// The counter for the next message, then advanced: never reused.
    fn next(&mut self) -> io::Result<(u64, [u8; 12])> {
        let counter = self.counter;
        self.counter = counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("nonce counter exhausted"))?;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        Ok((counter, nonce))
    }

    fn seal(&mut self, buf: &mut [u8]) -> io::Result<(u64, Tag)> {
        let (counter, nonce) = self.next()?;
        let tag = self
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), b"", buf)
            .map_err(|_| io::Error::other("encryption failed"))?;
        Ok((counter, tag))
    }

    fn open(&mut self, buf: &mut [u8], tag: &[u8]) -> io::Result<()> {
        let (_, nonce) = self.next()?;
        self.aead
            .decrypt_in_place_detached(Nonce::from_slice(&nonce), b"", buf, Tag::from_slice(tag))
            .map_err(|_| invalid("record authentication failed"))
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Decrypting side of a record stream. Keeps partial records across read
/// timeouts (`WouldBlock` / `TimedOut` from the socket), so it can be read
/// with a short timeout; any malformed or forged record ends the stream.
pub struct RecordReader<R> {
    inner: R,
    cipher: Cipher,
    /// Received bytes not yet forming a whole record.
    raw: Vec<u8>,
    /// Decrypted bytes, delivered from `pos`.
    plain: Vec<u8>,
    pos: usize,
    broken: bool,
}

impl<R: Read> RecordReader<R> {
    pub fn new(inner: R, key: &Key) -> Self {
        Self {
            inner,
            cipher: Cipher::new(key),
            raw: Vec::new(),
            plain: Vec::new(),
            pos: 0,
            broken: false,
        }
    }

    /// Decrypt the first record in `raw` if it is complete.
    fn open_record(&mut self) -> io::Result<bool> {
        let Some(hdr) = self.raw.get(..4) else {
            return Ok(false);
        };
        let len = u32::from_be_bytes(hdr.try_into().unwrap()) as usize;
        if !(TAG_LEN + 1..=MAX_RECORD + TAG_LEN).contains(&len) {
            return Err(invalid("bad record length"));
        }
        if self.raw.len() < 4 + len {
            return Ok(false);
        }
        let (body, tag) = self.raw[4..4 + len].split_at_mut(len - TAG_LEN);
        self.cipher.open(body, tag)?;
        self.plain.clear();
        self.plain.extend_from_slice(body);
        self.pos = 0;
        self.raw.drain(..4 + len);
        Ok(true)
    }
}

impl<R: Read> Read for RecordReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.plain.len() {
                let n = out.len().min(self.plain.len() - self.pos);
                out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.broken {
                return Err(invalid("broken record stream"));
            }
            match self.open_record() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    self.broken = true;
                    return Err(e);
                }
            }
            let mut buf = [0u8; 8192];
            let n = self.inner.read(&mut buf)?;
            if n == 0 {
                if self.raw.is_empty() {
                    return Ok(0);
                }
                self.broken = true;
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.raw.extend_from_slice(&buf[..n]);
        }
    }
}

/// Encrypting side of a record stream: one record per write of up to 64 KiB.
/// After a failed write (timeout mid-record) the stream is out of sync, so
/// every later write fails too.
pub struct RecordWriter<W> {
    inner: W,
    cipher: Cipher,
    buf: Vec<u8>,
    broken: bool,
}

impl<W: Write> RecordWriter<W> {
    pub fn new(inner: W, key: &Key) -> Self {
        Self {
            inner,
            cipher: Cipher::new(key),
            buf: Vec::new(),
            broken: false,
        }
    }
}

impl<W: Write> Write for RecordWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.broken {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = data.len().min(MAX_RECORD);
        if n == 0 {
            return Ok(0);
        }
        self.buf.clear();
        self.buf
            .extend_from_slice(&((n + TAG_LEN) as u32).to_be_bytes());
        self.buf.extend_from_slice(&data[..n]);
        let (_, tag) = self.cipher.seal(&mut self.buf[4..])?;
        self.buf.extend_from_slice(&tag);
        if let Err(e) = self.inner.write_all(&self.buf) {
            self.broken = true;
            return Err(e);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Encrypts UDP media datagrams (phone to PC).
pub struct UdpSealer(Cipher);

impl UdpSealer {
    pub fn new(key: &Key) -> Self {
        Self(Cipher::new(key))
    }

    /// `out` = [counter u64 BE][ciphertext | tag] of `frame`.
    pub fn seal(&mut self, frame: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        out.clear();
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(frame);
        let (counter, tag) = self.0.seal(&mut out[8..])?;
        out[..8].copy_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&tag);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const CODE: &str = "ABCDEFGHJKMNPQRSTVWX";

    fn vector_keys() -> Keys {
        let nc: [u8; 16] = std::array::from_fn(|i| i as u8);
        let ns: [u8; 16] = std::array::from_fn(|i| 0x10 + i as u8);
        derive(CODE, &nc, &ns)
    }

    #[test]
    fn spec_vectors() {
        let k = vector_keys();
        let expect = |key: Key, h: &str| assert_eq!(key.to_vec(), hex(h));
        expect(
            k.auth_c2s,
            "ecf73566ebfebeb67995440cc96aadb1f1037f49fccf4418698766c1f6eeef3c",
        );
        expect(
            k.auth_s2c,
            "eb5fc90f3f40d323dda73b7c6f0176fc54acdaf5e3c8557a9238489a8f8d50cc",
        );
        expect(
            k.key_c2s,
            "74432953986f9737db929a8314ebbec6127249a19c23d5d2727f20f9e7bde640",
        );
        expect(
            k.key_s2c,
            "69870d929048685f2d672c69d2f2b279f6d2ddc2a4474a5e4dbbac3e89a302ab",
        );
        expect(
            k.key_udp,
            "d2ebe84b600deec4f80d13790ef361825a58e9dff0811197dd54584753205d0b",
        );

        // Records: counter 0 "hello", counter 1 "world".
        let mut out = Vec::new();
        let mut w = RecordWriter::new(&mut out, &k.key_s2c);
        w.write_all(b"hello").unwrap();
        w.write_all(b"world").unwrap();
        let mut expected = vec![0, 0, 0, 21];
        expected.extend(hex("0af18ea846ffd0e5f9acc99c29b16023ffc03321a6"));
        expected.extend([0, 0, 0, 21]);
        expected.extend(hex("9e047e085752a8473d522172a55639a7fd28c505f3"));
        assert_eq!(out, expected);

        let mut r = RecordReader::new(&out[..], &k.key_s2c);
        let mut text = String::new();
        r.read_to_string(&mut text).unwrap();
        assert_eq!(text, "helloworld");
    }

    #[test]
    fn udp_datagrams() {
        let k = vector_keys();
        let mut sealer = UdpSealer::new(&k.key_udp);
        let mut out = Vec::new();
        for counter in 0..2u64 {
            sealer.seal(b"frame", &mut out).unwrap();
            assert_eq!(out[..8], counter.to_be_bytes());
            assert_eq!(out.len(), 8 + 5 + TAG_LEN);
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&counter.to_be_bytes());
            let (body, tag) = out[8..].split_at_mut(5);
            Aes256Gcm::new((&k.key_udp).into())
                .decrypt_in_place_detached(
                    Nonce::from_slice(&nonce),
                    b"",
                    body,
                    Tag::from_slice(tag),
                )
                .unwrap();
            assert_eq!(body, b"frame");
        }
    }

    #[test]
    fn codes() {
        assert_eq!(canonical("abcde-fghjk mnpqr-stvwx").as_deref(), Some(CODE));
        assert_eq!(
            canonical("OIL00-00000-00000-00000").as_deref(),
            Some("01100000000000000000")
        );
        assert_eq!(canonical("ABCDE-FGHJK-MNPQR-STVWU"), None); // U is not in the alphabet
        assert_eq!(canonical("ABCDE"), None);
        assert_eq!(display(CODE), "ABCDE-FGHJK-MNPQR-STVWX");
        let code = generate_code().unwrap();
        assert_eq!(display(&canonical(&code).unwrap()), code);
        assert_ne!(generate_code().unwrap(), code);
    }

    #[test]
    fn rejects_tampering() {
        let k = vector_keys();
        let mut out = Vec::new();
        RecordWriter::new(&mut out, &k.key_s2c)
            .write_all(b"hello")
            .unwrap();
        out[6] ^= 1;
        let mut r = RecordReader::new(&out[..], &k.key_s2c);
        assert!(r.read(&mut [0u8; 16]).is_err());
        assert!(r.read(&mut [0u8; 16]).is_err());
        // Too short to hold a tag and a byte.
        let short = [0u8, 0, 0, 16];
        assert!(
            RecordReader::new(&short[..], &k.key_s2c)
                .read(&mut [0u8; 16])
                .is_err()
        );
    }

    /// Client side, as in the PC client: returns the stream keys or an error.
    fn client(sock: &mut TcpStream, code: &str) -> io::Result<Keys> {
        let mut hello = MAGIC.to_vec();
        let nc = [7u8; 16];
        hello.extend_from_slice(&nc);
        sock.write_all(&hello)?;
        let mut ns = [0u8; 16];
        sock.read_exact(&mut ns)?;
        let keys = derive(code, &nc, &ns);
        sock.write_all(&keys.auth_c2s)?;
        let mut proof = [0u8; 32];
        sock.read_exact(&mut proof)?;
        if proof != keys.auth_s2c {
            return Err(io::Error::other("server proof mismatch"));
        }
        Ok(keys)
    }

    #[test]
    fn handshake_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(STEP_TIMEOUT)).unwrap();
            let mut magic = [0u8; 4];
            sock.read_exact(&mut magic).unwrap();
            assert_eq!(&magic, MAGIC);
            let keys = accept(&mut sock, CODE).unwrap();
            let mut r = RecordReader::new(sock.try_clone().unwrap(), &keys.key_c2s);
            let mut w = RecordWriter::new(sock, &keys.key_s2c);
            let mut msg = [0u8; 4];
            r.read_exact(&mut msg).unwrap();
            assert_eq!(&msg, b"ping");
            // Larger than one record.
            w.write_all(&vec![0x5a; MAX_RECORD + 10]).unwrap();
            let wrong = listener.accept().unwrap().0;
            (wrong, keys.key_udp)
        });

        let mut sock = TcpStream::connect(addr).unwrap();
        let keys = client(&mut sock, CODE).unwrap();
        let mut w = RecordWriter::new(sock.try_clone().unwrap(), &keys.key_c2s);
        let mut r = RecordReader::new(sock, &keys.key_s2c);
        w.write_all(b"ping").unwrap();
        let mut big = vec![0u8; MAX_RECORD + 10];
        r.read_exact(&mut big).unwrap();
        assert!(big.iter().all(|&b| b == 0x5a));

        // A wrong code is refused.
        let mut other = TcpStream::connect(addr).unwrap();
        let (mut wrong, key_udp) = server.join().unwrap();
        assert_eq!(key_udp, keys.key_udp);
        let refused = thread::spawn(move || {
            let mut magic = [0u8; 4];
            wrong.read_exact(&mut magic).unwrap();
            accept(&mut wrong, CODE).err().map(|e| e.kind())
        });
        assert!(client(&mut other, "ABCDEFGHJKMNPQRSTVWY").is_err());
        assert_eq!(
            refused.join().unwrap(),
            Some(io::ErrorKind::PermissionDenied)
        );
    }
}
