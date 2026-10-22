//! Microphone capture: AAudio on Android, a test tone elsewhere (desktop dev).

use serde::{Deserialize, Serialize};

use crate::encoder::{FRAME, RATE};

/// Android audio source (AAudio input preset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Main microphone, standard processing.
    #[default]
    Mic,
    /// Microphone aimed like the camera, tuned for video recording.
    Camcorder,
    /// Tuned for speech recognition: no AGC or noise suppression.
    VoiceRecognition,
    /// Tuned for calls: echo cancellation, noise suppression, AGC.
    VoiceCommunication,
    /// Raw signal, no processing (where supported).
    Unprocessed,
    /// Low latency, for live performance (Android 10+).
    VoicePerformance,
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    use ndk_sys as ffi;
    use std::ffi::CStr;
    use std::ptr;

    /// Blocking-read AAudio input stream. (Raw ndk-sys: the `ndk` 0.9 wrapper
    /// reports successful reads as errors.)
    pub struct Capture {
        stream: *mut ffi::AAudioStream,
    }

    fn preset(source: Source) -> i32 {
        (match source {
            Source::Mic => ffi::AAUDIO_INPUT_PRESET_GENERIC,
            Source::Camcorder => ffi::AAUDIO_INPUT_PRESET_CAMCORDER,
            Source::VoiceRecognition => ffi::AAUDIO_INPUT_PRESET_VOICE_RECOGNITION,
            Source::VoiceCommunication => ffi::AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION,
            Source::Unprocessed => ffi::AAUDIO_INPUT_PRESET_UNPROCESSED,
            // Android 10+; older releases fall back to their default preset.
            Source::VoicePerformance => ffi::AAUDIO_INPUT_PRESET_VOICE_PERFORMANCE,
        }) as i32
    }

    fn check(what: &str, result: ffi::aaudio_result_t) -> Result<i32, String> {
        if result < 0 {
            // SAFETY: AAudio_convertResultToText returns a static C string.
            let text = unsafe { CStr::from_ptr(ffi::AAudio_convertResultToText(result)) };
            return Err(format!("microphone {what}: {}", text.to_string_lossy()));
        }
        Ok(result)
    }

    impl Capture {
        pub fn open(source: Source) -> Result<Self, String> {
            let mut builder = ptr::null_mut();
            let mut stream = ptr::null_mut();
            // SAFETY: the builder is created, configured and deleted here; the
            // stream it opens is owned by the returned Capture.
            unsafe {
                check("builder", ffi::AAudio_createStreamBuilder(&mut builder))?;
                ffi::AAudioStreamBuilder_setDirection(builder, ffi::AAUDIO_DIRECTION_INPUT as i32);
                ffi::AAudioStreamBuilder_setSharingMode(
                    builder,
                    ffi::AAUDIO_SHARING_MODE_SHARED as i32,
                );
                ffi::AAudioStreamBuilder_setPerformanceMode(
                    builder,
                    ffi::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY as i32,
                );
                ffi::AAudioStreamBuilder_setSampleRate(builder, RATE);
                ffi::AAudioStreamBuilder_setChannelCount(builder, 1);
                ffi::AAudioStreamBuilder_setFormat(builder, ffi::AAUDIO_FORMAT_PCM_I16 as i32);
                ffi::AAudioStreamBuilder_setInputPreset(builder, preset(source));
                let opened = ffi::AAudioStreamBuilder_openStream(builder, &mut stream);
                ffi::AAudioStreamBuilder_delete(builder);
                check("open", opened)?;
            }
            let capture = Self { stream };
            // SAFETY: `stream` is open.
            check("start", unsafe { ffi::AAudioStream_requestStart(stream) })?;
            Ok(capture)
        }

        /// Block until one full frame has been captured.
        pub fn read(&mut self, buf: &mut [i16; FRAME]) -> Result<(), String> {
            let mut filled = 0;
            let mut idle = 0;
            while filled < FRAME {
                // SAFETY: the pointer covers FRAME - filled writable mono i16 frames.
                let n = check("read", unsafe {
                    ffi::AAudioStream_read(
                        self.stream,
                        buf[filled..].as_mut_ptr().cast(),
                        (FRAME - filled) as i32,
                        200_000_000,
                    )
                })? as usize;
                if n == 0 {
                    idle += 1;
                    if idle > 10 {
                        return Err("microphone stopped delivering audio".into());
                    }
                }
                filled += n;
            }
            Ok(())
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            // SAFETY: `stream` is open and closed exactly once.
            unsafe {
                ffi::AAudioStream_requestStop(self.stream);
                ffi::AAudioStream_close(self.stream);
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
mod imp {
    use super::*;
    use std::time::{Duration, Instant};

    /// 440 Hz tone paced in real time, so the whole pipeline can be tried on a PC.
    pub struct Capture {
        next: Instant,
        phase: f32,
    }

    impl Capture {
        pub fn open(_source: Source) -> Result<Self, String> {
            Ok(Self {
                next: Instant::now(),
                phase: 0.0,
            })
        }

        pub fn read(&mut self, buf: &mut [i16; FRAME]) -> Result<(), String> {
            let step = 440.0 * std::f32::consts::TAU / RATE as f32;
            for s in buf.iter_mut() {
                *s = (self.phase.sin() * 8000.0) as i16;
                self.phase = (self.phase + step) % std::f32::consts::TAU;
            }
            self.next += Duration::from_millis(20);
            if let Some(wait) = self.next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            Ok(())
        }
    }
}

pub use imp::Capture;
