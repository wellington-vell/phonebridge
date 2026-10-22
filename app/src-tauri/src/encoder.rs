//! Opus encoder (vendored libopus): 48 kHz mono, 20 ms frames.

use opus_head_sys as ffi;
use std::ffi::{CStr, c_int};

pub const RATE: i32 = 48000;
/// 20 ms @ 48 kHz, what the Phonebridge client expects.
pub const FRAME: usize = 960;
const BITRATE: i32 = 64000;

fn strerror(code: c_int) -> String {
    // SAFETY: opus_strerror always returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(ffi::opus_strerror(code)) }
        .to_string_lossy()
        .into_owned()
}

pub struct Encoder {
    enc: *mut ffi::OpusEncoder,
    out: Vec<u8>,
}

// SAFETY: the encoder state is only touched through &mut self.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self, String> {
        let mut err: c_int = 0;
        // SAFETY: valid rate/channels/application and a valid out-pointer.
        let enc = unsafe {
            ffi::opus_encoder_create(RATE, 1, ffi::OPUS_APPLICATION_AUDIO as c_int, &mut err)
        };
        if err != 0 || enc.is_null() {
            return Err(strerror(err));
        }
        // SAFETY: OPUS_SET_BITRATE takes one opus_int32 argument.
        unsafe { ffi::opus_encoder_ctl(enc, ffi::OPUS_SET_BITRATE_REQUEST as c_int, BITRATE) };
        Ok(Self {
            enc,
            out: vec![0; 1500],
        })
    }

    /// Encode one 20 ms frame.
    pub fn encode(&mut self, pcm: &[i16; FRAME]) -> Result<&[u8], String> {
        // SAFETY: `pcm` holds FRAME samples and `out` is writable for its length.
        let n = unsafe {
            ffi::opus_encode(
                self.enc,
                pcm.as_ptr(),
                FRAME as c_int,
                self.out.as_mut_ptr(),
                self.out.len() as i32,
            )
        };
        if n < 0 {
            return Err(strerror(n));
        }
        Ok(&self.out[..n as usize])
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `enc` came from opus_encoder_create and is destroyed once.
        unsafe { ffi::opus_encoder_destroy(self.enc) };
    }
}
