//! Noise gate: silence below a threshold, so quieter sound (other people in
//! the room, further from the phone than you) doesn't reach the PC and trip
//! voice activity there.
//!
//! Opens on a frame whose peak reaches the threshold, stays open while the
//! sound stays within HYSTERESIS of it, then holds for HOLD before fading out
//! over RELEASE (so word endings and short pauses aren't cut). Opening fades in
//! over ATTACK, short enough to keep the start of a word, long enough not to
//! click.

use crate::encoder::RATE;

/// Thresholds the UI offers, dBFS (peak).
pub const MIN_DB: f32 = -70.0;
pub const MAX_DB: f32 = -5.0;
const HYSTERESIS_DB: f32 = 6.0;
const HOLD_MS: u32 = 300;
const ATTACK_MS: u32 = 2;
const RELEASE_MS: u32 = 120;

const fn samples(ms: u32) -> u32 {
    RATE as u32 / 1000 * ms
}

pub struct Gate {
    /// Linear peak that opens the gate, 0..1 (of i16 full scale).
    open_at: f32,
    /// Linear peak that keeps it open.
    stay_at: f32,
    open: bool,
    /// Samples left before the gate starts closing.
    hold: u32,
    /// Current gain, 0..1, moving towards 1 (open) or 0 (closed).
    gain: f32,
}

impl Gate {
    pub fn new(threshold_db: f32) -> Self {
        let mut g = Gate {
            open_at: 0.0,
            stay_at: 0.0,
            open: false,
            hold: 0,
            gain: 0.0,
        };
        g.set_threshold(threshold_db);
        g
    }

    pub fn set_threshold(&mut self, db: f32) {
        let db = db.clamp(MIN_DB, MAX_DB);
        self.open_at = 10f32.powf(db / 20.0);
        self.stay_at = 10f32.powf((db - HYSTERESIS_DB) / 20.0);
    }

    /// Whether the last frame was let through (even partly, while fading).
    pub fn is_open(&self) -> bool {
        self.gain > 0.0
    }

    /// Gate one frame in place; `peak` is its peak, 0..1.
    pub fn process(&mut self, pcm: &mut [i16], peak: f32) {
        let len = pcm.len() as u32;
        let at = if self.open {
            self.stay_at
        } else {
            self.open_at
        };
        if peak >= at {
            self.open = true;
            self.hold = samples(HOLD_MS);
        } else if self.hold > len {
            self.hold -= len;
        } else {
            self.hold = 0;
            self.open = false;
        }
        let (target, step) = if self.open {
            (1.0, 1.0 / samples(ATTACK_MS) as f32)
        } else {
            (0.0, 1.0 / samples(RELEASE_MS) as f32)
        };
        if self.gain == target {
            if target == 0.0 {
                pcm.fill(0);
            }
            return;
        }
        for s in pcm.iter_mut() {
            self.gain = if target > self.gain {
                (self.gain + step).min(target)
            } else {
                (self.gain - step).max(target)
            };
            *s = (*s as f32 * self.gain) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::FRAME;

    /// A frame of a square wave with this peak.
    fn frame(peak: f32) -> [i16; FRAME] {
        let a = (peak * 32767.0) as i16;
        std::array::from_fn(|i| if i % 2 == 0 { a } else { -a })
    }

    fn run(g: &mut Gate, peak: f32) -> [i16; FRAME] {
        let mut f = frame(peak);
        g.process(&mut f, peak);
        f
    }

    const LOUD: f32 = 0.3; // about -10 dB
    const QUIET: f32 = 0.003; // about -50 dB

    #[test]
    fn quiet_sound_stays_out() {
        let mut g = Gate::new(-30.0);
        for _ in 0..50 {
            assert!(run(&mut g, QUIET).iter().all(|&s| s == 0));
        }
        assert!(!g.is_open());
    }

    #[test]
    fn loud_sound_passes_with_a_short_fade_in() {
        let mut g = Gate::new(-30.0);
        let f = run(&mut g, LOUD);
        assert!(f[0].abs() < f[200].abs(), "fades in");
        assert_eq!(f[200..], frame(LOUD)[200..], "then passes untouched");
        assert_eq!(run(&mut g, LOUD), frame(LOUD));
    }

    #[test]
    fn holds_through_a_pause_then_fades_out() {
        let mut g = Gate::new(-30.0);
        run(&mut g, LOUD);
        // A 280 ms pause (14 frames of 20 ms) is still within the hold...
        for _ in 0..14 {
            assert_eq!(run(&mut g, QUIET), frame(QUIET));
        }
        // ...then it fades out over the release and stays shut.
        for _ in 0..10 {
            run(&mut g, QUIET);
        }
        assert!(!g.is_open());
        assert!(run(&mut g, QUIET).iter().all(|&s| s == 0));
    }

    #[test]
    fn hysteresis_keeps_a_fading_voice_open() {
        let mut g = Gate::new(-30.0);
        run(&mut g, LOUD);
        // -33 dB: below the threshold, within the hysteresis.
        let tail = 10f32.powf(-33.0 / 20.0);
        for _ in 0..100 {
            run(&mut g, tail);
        }
        assert!(g.is_open());
        // From closed, the same level doesn't open it.
        let mut g = Gate::new(-30.0);
        run(&mut g, tail);
        assert!(!g.is_open());
    }
}
