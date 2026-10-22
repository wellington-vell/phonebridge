//! Pairing code: authentication and encryption of the links to the phone (PBA1).
//!
//! The PC is the client; see docs/secure-channel.md for the protocol. After
//! the handshake every byte of a TCP (or RFCOMM) stream travels in
//! AES-256-GCM records, with one key and nonce counter per direction, and UDP
//! media datagrams use a third key. Without a pairing code the stream passes
//! through unchanged (plaintext WO Mic / PBV1, as the official WO Mic app speaks).

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, PoisonError};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use subtle::ConstantTimeEq;

const MAGIC: &[u8; 4] = b"PBA1";
/// Crockford base32.
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 20;
const NONCE: usize = 16;
const PROOF: usize = 32;
const TAG: usize = 16;
/// Largest plaintext per record.
const MAX_PLAIN: usize = 65536;

const BAD_CODE: &str = "invalid pairing code: expected the 20 characters shown on the phone, \
                        e.g. ABCDE-FGHJK-MNPQR-STVWX";

/// Canonical form of a pairing code as typed: drop `-` and whitespace,
/// uppercase, read O as 0 and I / L as 1; then exactly 20 base32 characters.
pub fn normalize(input: &str) -> Result<String, String> {
    let code: String = input
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    if code.len() != CODE_LEN || !code.bytes().all(|b| ALPHABET.contains(&b)) {
        return Err(BAD_CODE.into());
    }
    Ok(code)
}

struct Keys {
    auth_c2s: [u8; 32],
    auth_s2c: [u8; 32],
    key_c2s: [u8; 32],
    key_s2c: [u8; 32],
    key_udp: [u8; 32],
}

/// HKDF-SHA256 over the canonical code, salted with both handshake nonces.
fn derive(code: &str, nc: &[u8; NONCE], ns: &[u8; NONCE]) -> Keys {
    let mut salt = [0u8; 2 * NONCE];
    salt[..NONCE].copy_from_slice(nc);
    salt[NONCE..].copy_from_slice(ns);
    let hk = Hkdf::<Sha256>::new(Some(&salt), code.as_bytes());
    let x = |label: &str| {
        let mut k = [0u8; 32];
        hk.expand(format!("phonebridge v1 {label}").as_bytes(), &mut k)
            .expect("32 bytes is a valid HKDF-SHA256 length");
        k
    };
    Keys {
        auth_c2s: x("auth c2s"),
        auth_s2c: x("auth s2c"),
        key_c2s: x("key c2s"),
        key_s2c: x("key s2c"),
        key_udp: x("key udp"),
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn nonce(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// One direction: AES-256-GCM with a counter nonce that only goes up.
struct Cipher {
    aead: Aes256Gcm,
    counter: u64,
}

impl Cipher {
    fn new(key: &[u8; 32]) -> Self {
        Cipher {
            aead: Aes256Gcm::new(key.into()),
            counter: 0,
        }
    }

    /// The next nonce; the counter moves on before it is used, so a nonce is
    /// never used twice, whatever happens to the record.
    fn next(&mut self) -> io::Result<[u8; 12]> {
        let n = nonce(self.counter);
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| invalid("record counter exhausted"))?;
        Ok(n)
    }

    fn seal(&mut self, plain: &[u8]) -> io::Result<Vec<u8>> {
        let n = self.next()?;
        self.aead
            .encrypt(Nonce::from_slice(&n), plain)
            .map_err(|_| invalid("encryption failed"))
    }

    fn open(&mut self, sealed: &[u8]) -> io::Result<Vec<u8>> {
        let n = self.next()?;
        self.aead
            .decrypt(Nonce::from_slice(&n), sealed)
            .map_err(|_| {
                invalid("record failed authentication (tampered, or not the paired phone)")
            })
    }
}

/// A stream to the phone: encrypted records after a PBA1 handshake, or plain
/// bytes without a pairing code. Reads come from `R`; writes go through a
/// [`Writer`], which other threads can share (keepalives).
pub struct Channel<R, W> {
    reader: R,
    rx: Option<Cipher>,
    plain: Vec<u8>,
    pos: usize,
    /// A record failed or was cut short: the stream is out of step for good.
    broken: bool,
    tx: Writer<W>,
}

/// The sending half of a [`Channel`]. Clones share one stream and one nonce
/// counter behind a lock, so records never interleave and nonces never repeat.
pub struct Writer<W>(Arc<Mutex<Tx<W>>>);

impl<W> Clone for Writer<W> {
    fn clone(&self) -> Self {
        Writer(Arc::clone(&self.0))
    }
}

struct Tx<W> {
    w: W,
    seal: Option<Cipher>,
    broken: bool,
}

impl<W: Write> Tx<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(seal) = &mut self.seal else {
            return self.w.write(buf);
        };
        if self.broken {
            return Err(invalid("encrypted stream broken by an earlier error"));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(MAX_PLAIN);
        let sealed = seal.seal(&buf[..n])?;
        let mut rec = Vec::with_capacity(4 + sealed.len());
        rec.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
        rec.extend_from_slice(&sealed);
        self.broken = true;
        self.w.write_all(&rec)?;
        self.broken = false;
        Ok(n)
    }
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .w
            .flush()
    }
}

impl<R, W> Channel<R, W> {
    fn new(reader: R, writer: W, rx: Option<Cipher>, tx: Option<Cipher>) -> Self {
        Channel {
            reader,
            rx,
            plain: Vec::new(),
            pos: 0,
            broken: false,
            tx: Writer(Arc::new(Mutex::new(Tx {
                w: writer,
                seal: tx,
                broken: false,
            }))),
        }
    }

    /// The underlying reader (e.g. to change socket timeouts).
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// A sending handle for another thread.
    pub fn writer(&self) -> Writer<W> {
        self.tx.clone()
    }
}

impl<R: Read, W> Channel<R, W> {
    /// Read, check and decrypt the next record.
    fn fill(&mut self) -> io::Result<()> {
        let Some(rx) = &mut self.rx else {
            unreachable!("plain channels don't use records")
        };
        if self.broken {
            return Err(invalid("encrypted stream broken by an earlier error"));
        }
        self.broken = true;
        let mut len = [0u8; 4];
        self.reader.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if !(TAG + 1..=MAX_PLAIN + TAG).contains(&len) {
            return Err(invalid("bad encrypted record length"));
        }
        let mut sealed = vec![0u8; len];
        self.reader.read_exact(&mut sealed)?;
        self.plain = rx.open(&sealed)?;
        self.pos = 0;
        self.broken = false;
        Ok(())
    }
}

impl<R: Read, W> Read for Channel<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.rx.is_none() {
            return self.reader.read(buf);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos == self.plain.len() {
            self.fill()?;
        }
        let n = buf.len().min(self.plain.len() - self.pos);
        buf[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl<R, W: Write> Write for Channel<R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tx.flush()
    }
}

/// Decrypts UDP media datagrams; replays and anything older than the newest
/// accepted datagram are dropped.
pub struct Udp {
    /// Boxed: the key schedule is large and this sits in an enum.
    aead: Box<Aes256Gcm>,
    highest: Option<u64>,
}

impl Udp {
    /// The plaintext media frame, or None to drop the datagram.
    pub fn open(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() < 8 + TAG {
            return None;
        }
        let (counter, sealed) = datagram.split_at(8);
        let counter = u64::from_be_bytes(counter.try_into().unwrap());
        if self.highest.is_some_and(|h| counter <= h) {
            return None;
        }
        let plain = self
            .aead
            .decrypt(Nonce::from_slice(&nonce(counter)), sealed)
            .ok()?;
        self.highest = Some(counter);
        Some(plain)
    }
}

/// Hints for a phone that hangs up or goes quiet during pairing.
fn pairing_err(e: io::Error, msg: &str) -> io::Error {
    match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(e.kind(), msg)
        }
        _ => e,
    }
}

/// Open the channel: the PBA1 handshake as the client when there is a pairing
/// `code` (canonical, see [`normalize`]), else a plain pass-through. The
/// reader should have a read timeout (each handshake step gets 5 s). Returns
/// the UDP media decryptor along with an encrypted channel.
pub fn open<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    code: Option<&str>,
) -> io::Result<(Channel<R, W>, Option<Udp>)> {
    let Some(code) = code else {
        return Ok((Channel::new(reader, writer, None, None), None));
    };
    let mut nc = [0u8; NONCE];
    getrandom::fill(&mut nc).map_err(|e| io::Error::other(format!("random nonce: {e}")))?;
    let mut hello = MAGIC.to_vec();
    hello.extend_from_slice(&nc);
    writer.write_all(&hello)?;

    let mut ns = [0u8; NONCE];
    reader.read_exact(&mut ns).map_err(|e| {
        pairing_err(
            e,
            "no pairing reply from the phone: is it the Phonebridge app? \
             (the official WO Mic app has no pairing code: drop --token)",
        )
    })?;
    let k = derive(code, &nc, &ns);
    writer.write_all(&k.auth_c2s)?;
    let mut proof = [0u8; PROOF];
    reader.read_exact(&mut proof).map_err(|e| {
        pairing_err(
            e,
            "the phone refused the pairing code (wrong code, or reset on the phone?)",
        )
    })?;
    if !bool::from(proof.ct_eq(&k.auth_s2c)) {
        return Err(invalid(
            "the phone failed the pairing check: not the phone with this pairing code",
        ));
    }
    let udp = Udp {
        aead: Box::new(Aes256Gcm::new((&k.key_udp).into())),
        highest: None,
    };
    let rx = Cipher::new(&k.key_s2c);
    let tx = Cipher::new(&k.key_c2s);
    Ok((Channel::new(reader, writer, Some(rx), Some(tx)), Some(udp)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::thread;

    const CODE: &str = "ABCDEFGHJKMNPQRSTVWX";

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn vector_nonces() -> ([u8; NONCE], [u8; NONCE]) {
        let nc: [u8; NONCE] = std::array::from_fn(|i| i as u8);
        let ns: [u8; NONCE] = std::array::from_fn(|i| 0x10 + i as u8);
        (nc, ns)
    }

    #[test]
    fn spec_vectors() {
        let (nc, ns) = vector_nonces();
        let k = derive(CODE, &nc, &ns);
        assert_eq!(
            hex(&k.auth_c2s),
            "ecf73566ebfebeb67995440cc96aadb1f1037f49fccf4418698766c1f6eeef3c"
        );
        assert_eq!(
            hex(&k.auth_s2c),
            "eb5fc90f3f40d323dda73b7c6f0176fc54acdaf5e3c8557a9238489a8f8d50cc"
        );
        assert_eq!(
            hex(&k.key_c2s),
            "74432953986f9737db929a8314ebbec6127249a19c23d5d2727f20f9e7bde640"
        );
        assert_eq!(
            hex(&k.key_s2c),
            "69870d929048685f2d672c69d2f2b279f6d2ddc2a4474a5e4dbbac3e89a302ab"
        );
        assert_eq!(
            hex(&k.key_udp),
            "d2ebe84b600deec4f80d13790ef361825a58e9dff0811197dd54584753205d0b"
        );
        let mut c = Cipher::new(&k.key_s2c);
        assert_eq!(
            hex(&c.seal(b"hello").unwrap()),
            "0af18ea846ffd0e5f9acc99c29b16023ffc03321a6"
        );
        assert_eq!(
            hex(&c.seal(b"world").unwrap()),
            "9e047e085752a8473d522172a55639a7fd28c505f3"
        );
    }

    #[test]
    fn codes() {
        assert_eq!(normalize("abcde-fghjk mnpqr-stvwx").unwrap(), CODE);
        assert_eq!(
            normalize("OIL00-00000-00000-0000O").unwrap(),
            "01100000000000000000"
        );
        for bad in [
            "",
            "ABCDE-FGHJK-MNPQR-STVW",
            "ABCDE-FGHJK-MNPQR-STVWXY",
            "ABCDE-FGHJK-MNPQR-STVWU",
        ] {
            assert!(normalize(bad).is_err(), "{bad}");
        }
    }

    /// The phone's side of the handshake; then echoes everything back.
    fn fake_phone(sock: UnixStream, code: &'static str) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut r = sock.try_clone().unwrap();
            let mut w = sock;
            let mut hello = [0u8; 4 + NONCE];
            r.read_exact(&mut hello).unwrap();
            assert_eq!(&hello[..4], MAGIC);
            let nc: [u8; NONCE] = hello[4..].try_into().unwrap();
            let ns = [7u8; NONCE];
            w.write_all(&ns).unwrap();
            let k = derive(code, &nc, &ns);
            let mut proof = [0u8; PROOF];
            r.read_exact(&mut proof).unwrap();
            if proof != k.auth_c2s {
                return; // closes: wrong code
            }
            w.write_all(&k.auth_s2c).unwrap();
            let rx = Cipher::new(&k.key_c2s);
            let tx = Cipher::new(&k.key_s2c);
            let mut ch = Channel::new(r, w, Some(rx), Some(tx));
            let mut out = ch.writer();
            let _ = io::copy(&mut ch, &mut out);
        })
    }

    #[test]
    fn round_trip() {
        let (pc, phone) = UnixStream::pair().unwrap();
        let echo = fake_phone(phone, CODE);
        let (mut ch, udp) = open(pc.try_clone().unwrap(), pc, Some(CODE)).unwrap();
        assert!(udp.is_some());
        // Larger than one record, and written from two threads at once
        // (while this one reads the echo).
        let big: Vec<u8> = (0..150_000u32).map(|i| i as u8).collect();
        let (mut w1, mut w2) = (ch.writer(), ch.writer());
        let data = big.clone();
        let t1 = thread::spawn(move || w1.write_all(&data).unwrap());
        let t2 = thread::spawn(move || w2.write_all(&[0xaa; 1000]).unwrap());
        let mut back = vec![0u8; big.len() + 1000];
        ch.read_exact(&mut back).unwrap();
        t1.join().unwrap();
        t2.join().unwrap();
        // Records stay whole: the other thread's bytes come as one block.
        let at = back.windows(1000).position(|w| w == [0xaa; 1000]).unwrap();
        let mut rest = back[..at].to_vec();
        rest.extend_from_slice(&back[at + 1000..]);
        assert_eq!(rest, big);
        drop(ch);
        echo.join().unwrap();
    }

    #[test]
    fn wrong_code_is_refused() {
        let (pc, phone) = UnixStream::pair().unwrap();
        let echo = fake_phone(phone, "ZZZZZZZZZZZZZZZZZZZZ");
        let e = open(pc.try_clone().unwrap(), pc, Some(CODE)).err().unwrap();
        assert!(e.to_string().contains("refused the pairing code"), "{e}");
        echo.join().unwrap();
    }

    #[test]
    fn tampered_record_is_an_error() {
        let k = derive(CODE, &[0; NONCE], &[1; NONCE]);
        let mut sealed = Cipher::new(&k.key_s2c).seal(b"media").unwrap();
        let mut rec = (sealed.len() as u32).to_be_bytes().to_vec();
        sealed[0] ^= 1;
        rec.extend(sealed);
        let mut ch = Channel::new(
            io::Cursor::new(rec),
            io::sink(),
            Some(Cipher::new(&k.key_s2c)),
            None,
        );
        assert!(ch.read(&mut [0u8; 8]).is_err());
        // Too short and too long records.
        for len in [16u32, 65536 + 17] {
            let mut ch = Channel::new(
                io::Cursor::new(len.to_be_bytes().to_vec()),
                io::sink(),
                Some(Cipher::new(&k.key_s2c)),
                None,
            );
            assert!(ch.read(&mut [0u8; 8]).is_err());
        }
    }

    #[test]
    fn udp_drops_replays() {
        let k = derive(CODE, &[0; NONCE], &[1; NONCE]);
        let mut udp = Udp {
            aead: Box::new(Aes256Gcm::new((&k.key_udp).into())),
            highest: None,
        };
        let datagram = |counter: u64, plain: &[u8]| {
            let mut c = Cipher::new(&k.key_udp);
            c.counter = counter;
            let mut d = counter.to_be_bytes().to_vec();
            d.extend(c.seal(plain).unwrap());
            d
        };
        assert_eq!(udp.open(&datagram(0, b"a")).unwrap(), b"a");
        assert_eq!(udp.open(&datagram(5, b"b")).unwrap(), b"b");
        assert!(udp.open(&datagram(5, b"b")).is_none()); // replay
        assert!(udp.open(&datagram(3, b"c")).is_none()); // older
        let mut bad = datagram(6, b"d");
        bad[10] ^= 1;
        assert!(udp.open(&bad).is_none());
        assert_eq!(udp.open(&datagram(6, b"d")).unwrap(), b"d");
    }
}
