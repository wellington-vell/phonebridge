//! Minimal libopus decoder bindings (linked against the system libopus).

use std::ffi::{CStr, c_char, c_int};
use std::ptr;

pub const RATE: i32 = 48000;
pub const CHANNELS: usize = 1;
/// 120 ms @ 48 kHz, the largest Opus frame.
const MAX_FRAME: usize = 5760;
/// WO Mic sends 20 ms frames; used for concealment before the first packet.
const DEFAULT_FRAME: usize = 960;

#[repr(C)]
struct RawDecoder {
    _private: [u8; 0],
}

#[link(name = "opus")]
unsafe extern "C" {
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut RawDecoder;
    fn opus_decode(
        st: *mut RawDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decoder_destroy(st: *mut RawDecoder);
    fn opus_strerror(error: c_int) -> *const c_char;
}

fn strerror(code: c_int) -> String {
    // SAFETY: opus_strerror always returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(opus_strerror(code)) }
        .to_string_lossy()
        .into_owned()
}

pub struct Decoder {
    dec: *mut RawDecoder,
    pcm: Vec<i16>,
    last_frame: usize,
}

impl Decoder {
    pub fn new() -> Result<Self, String> {
        let mut err: c_int = 0;
        // SAFETY: valid rate/channel count and a valid out-pointer for the error.
        let dec = unsafe { opus_decoder_create(RATE, CHANNELS as c_int, &mut err) };
        if err != 0 || dec.is_null() {
            return Err(strerror(err));
        }
        Ok(Self {
            dec,
            pcm: vec![0; MAX_FRAME * CHANNELS],
            last_frame: DEFAULT_FRAME,
        })
    }

    /// Decode one packet into interleaved s16 samples. `None` runs packet loss
    /// concealment for one frame of the last seen duration.
    pub fn decode(&mut self, data: Option<&[u8]>) -> Result<&[i16], String> {
        let (ptr, len, frame) = match data {
            Some(d) => (d.as_ptr(), d.len() as i32, MAX_FRAME),
            None => (ptr::null(), 0, self.last_frame),
        };
        // SAFETY: `pcm` holds MAX_FRAME * CHANNELS samples and `frame <= MAX_FRAME`.
        let n =
            unsafe { opus_decode(self.dec, ptr, len, self.pcm.as_mut_ptr(), frame as c_int, 0) };
        if n < 0 {
            return Err(strerror(n));
        }
        let n = n as usize;
        if data.is_some() {
            self.last_frame = n;
        }
        Ok(&self.pcm[..n * CHANNELS])
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `dec` came from opus_decoder_create and is destroyed once.
        unsafe { opus_decoder_destroy(self.dec) };
    }
}
