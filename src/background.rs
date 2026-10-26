//! Virtual background: the person cut out of each webcam frame and put over a
//! picture chosen in the phone app.
//!
//! The phone sends the picture (JPEG) in its video hello; ffmpeg turns it into
//! one webcam-size YUYV frame. A segmentation thread runs MediaPipe Selfie
//! Segmentation (models/, Apache-2.0) with tract on a 256x144 copy of the
//! newest frame and publishes a person mask; every frame is blended with the
//! latest mask, so a slow inference never holds the stream back. Until the
//! first mask the webcam shows only the picture.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

use tract_onnx::prelude::*;

use crate::log;
use crate::video::{PIX_FMT, frame_size, strings};

const MODEL: &[u8] = include_bytes!("../models/selfie_segmentation_landscape.onnx");
/// Model input and mask size.
const MW: usize = 256;
const MH: usize = 144;
/// Weight of the new mask against the previous one (less flicker).
const EMA_NEW: f32 = 0.6;
/// Mask probabilities between these fade from background to person.
const EDGE: (f32, f32) = (0.3, 0.7);

// ------------------------------------------------------------------ picture
/// One-shot ffmpeg: the phone's JPEG on stdin, cropped to fill the webcam
/// size, one YUYV frame out. Only the MJPEG decoder, and allocations capped,
/// as the picture comes from the network.
fn picture_args((w, h): (u32, u32)) -> Vec<String> {
    let mut a = strings([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-max_alloc",
        "67108864",
        "-threads",
        "1",
        "-f",
        "image2pipe",
        "-c:v",
        "mjpeg",
        "-i",
        "-",
        "-vf",
    ]);
    a.push(format!(
        "scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},setsar=1,format={PIX_FMT}"
    ));
    a.extend(strings([
        "-frames:v",
        "1",
        "-pix_fmt",
        PIX_FMT,
        "-f",
        "rawvideo",
        "-",
    ]));
    a
}

/// The background picture as a webcam-size YUYV frame.
pub fn decode_picture(jpeg: &[u8], size: (u32, u32)) -> Result<Vec<u8>, String> {
    let mut child = Command::new("ffmpeg")
        .args(picture_args(size))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Fed from a thread: ffmpeg may fill stdout before reading all of stdin.
    let out = thread::scope(|s| {
        s.spawn(move || {
            let _ = stdin.write_all(jpeg);
        });
        child.wait_with_output()
    })
    .map_err(|e| format!("ffmpeg: {e}"))?;
    if out.status.success() && out.stdout.len() == frame_size(size) {
        Ok(out.stdout)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.lines().next().unwrap_or("no picture decoded");
        Err(format!("cannot decode the background picture: {err}"))
    }
}

// -------------------------------------------------------------------- model
type Model = Arc<TypedRunnableModel>;

/// Loaded on first use, then shared by every session.
fn model() -> Result<Model, String> {
    static MODEL_CELL: OnceLock<Result<Model, String>> = OnceLock::new();
    MODEL_CELL
        .get_or_init(|| {
            (|| -> TractResult<Model> {
                // The file's shape hints use a symbolic batch size; pin it to 1.
                tract_onnx::onnx()
                    .with_ignore_output_shapes(true)
                    .with_ignore_value_info(true)
                    .model_for_read(&mut io::Cursor::new(MODEL))?
                    .with_input_fact(0, f32::fact([1, 3, MH, MW]).into())?
                    .into_optimized()?
                    .into_runnable()
            })()
            .map_err(|e| format!("segmentation model: {e}"))
        })
        .clone()
}

/// Person probability per pixel of a 256x144 RGB input (NCHW, 0..1).
fn infer(model: &Model, input: Vec<f32>) -> TractResult<Vec<f32>> {
    let t = Tensor::from_shape(&[1, 3, MH, MW], &input)?;
    let out = model.run(tvec!(t.into()))?;
    let m = out[0].to_plain_array_view::<f32>()?;
    if m.len() != MW * MH {
        return Err(TractError::msg(format!(
            "unexpected mask shape {:?}",
            m.shape()
        )));
    }
    Ok(m.iter().copied().collect())
}

// ------------------------------------------------------------------ pixels
/// The model's input from a YUYV frame: nearest sample per cell, BT.601
/// limited range (what ffmpeg's scale writes) to RGB, NCHW.
fn model_input(yuyv: &[u8], (w, h): (u32, u32)) -> Vec<f32> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0f32; 3 * MW * MH];
    for my in 0..MH {
        let sy = (2 * my + 1) * h / (2 * MH);
        for mx in 0..MW {
            let sx = (2 * mx + 1) * w / (2 * MW);
            let pair = (sy * w + (sx & !1)) * 2;
            let y = f32::from(yuyv[(sy * w + sx) * 2]) - 16.0;
            let u = f32::from(yuyv[pair + 1]) - 128.0;
            let v = f32::from(yuyv[pair + 3]) - 128.0;
            let rgb = [
                1.164 * y + 1.596 * v,
                1.164 * y - 0.392 * u - 0.813 * v,
                1.164 * y + 2.017 * u,
            ];
            for (c, val) in rgb.into_iter().enumerate() {
                out[(c * MH + my) * MW + mx] = (val / 255.0).clamp(0.0, 1.0);
            }
        }
    }
    out
}

/// Fold a new inference into the running mask and turn it into 0..255
/// weights with a soft edge.
fn update_mask(ema: &mut Vec<f32>, probs: &[f32]) -> Vec<u8> {
    if ema.len() != probs.len() {
        ema.clear();
        ema.extend_from_slice(probs);
    } else {
        for (e, p) in ema.iter_mut().zip(probs) {
            *e += (p - *e) * EMA_NEW;
        }
    }
    ema.iter()
        .map(|&p| {
            let t = ((p - EDGE.0) / (EDGE.1 - EDGE.0)).clamp(0.0, 1.0);
            (t * t * (3.0 - 2.0 * t) * 255.0).round() as u8
        })
        .collect()
}

/// Bilinear sampling positions from `dst` cells onto `src` cells: for each
/// destination index, the two source indices and the weight (0..256) of the
/// second.
fn taps(dst: usize, src: usize) -> Vec<(usize, usize, u32)> {
    (0..dst)
        .map(|i| {
            let f = ((i as f32 + 0.5) * src as f32 / dst as f32 - 0.5).max(0.0);
            let a = (f as usize).min(src - 1);
            let b = (a + 1).min(src - 1);
            (a, b, ((f - a as f32) * 256.0) as u32)
        })
        .collect()
}

/// Upsamples the 256x144 mask to the webcam size and blends the frame over
/// the picture, in place.
struct Blender {
    cols: Vec<(usize, usize, u32)>,
    rows: Vec<(usize, usize, u32)>,
    /// One upsampled mask row.
    line: Vec<u32>,
}

impl Blender {
    fn new((w, h): (u32, u32)) -> Self {
        Blender {
            cols: taps(w as usize, MW),
            rows: taps(h as usize, MH),
            line: vec![0; w as usize],
        }
    }

    fn blend(&mut self, frame: &mut [u8], picture: &[u8], mask: &[u8]) {
        let w = self.cols.len();
        for (y, &(r0, r1, wy)) in self.rows.iter().enumerate() {
            let (top, bottom) = (&mask[r0 * MW..][..MW], &mask[r1 * MW..][..MW]);
            for (m, &(c0, c1, wx)) in self.line.iter_mut().zip(&self.cols) {
                let lerp = |row: &[u8]| u32::from(row[c0]) * (256 - wx) + u32::from(row[c1]) * wx;
                // 0..255
                *m = (lerp(top) * (256 - wy) + lerp(bottom) * wy + (1 << 15)) >> 16;
            }
            let fg = &mut frame[y * w * 2..][..w * 2];
            let bg = &picture[y * w * 2..][..w * 2];
            for ((px, bg), m) in fg
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(bg.as_chunks::<4>().0)
                .zip(self.line.as_chunks::<2>().0)
            {
                let (a, b) = (m[0], m[1]);
                let uv = (a + b) / 2;
                let mix = |f: u8, g: u8, m: u32| {
                    ((u32::from(f) * m + u32::from(g) * (255 - m) + 127) / 255) as u8
                };
                px[0] = mix(px[0], bg[0], a);
                px[1] = mix(px[1], bg[1], uv);
                px[2] = mix(px[2], bg[2], b);
                px[3] = mix(px[3], bg[3], uv);
            }
        }
    }
}

// --------------------------------------------------------------- compositor
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

#[derive(Default)]
struct State {
    /// Newest model input, waiting for the segmentation thread.
    input: Option<Vec<f32>>,
    /// Latest mask and its sequence number, bumped per mask.
    mask: Vec<u8>,
    seq: u64,
    stop: bool,
}

/// One phone connection's background: [`Compositor::segment`] runs on its own
/// thread, [`Compositor::apply`] on the decoded frames.
pub struct Compositor {
    size: (u32, u32),
    picture: Vec<u8>,
    model: Model,
    shared: Shared,
}

/// The reader's side: its copy of the latest mask.
pub struct Applier<'a> {
    c: &'a Compositor,
    blender: Blender,
    mask: Vec<u8>,
    seq: u64,
}

impl Compositor {
    /// `picture`: from [`decode_picture`], at `size`.
    pub fn new(picture: Vec<u8>, size: (u32, u32)) -> Result<Self, String> {
        Ok(Compositor {
            size,
            picture,
            model: model()?,
            shared: Shared::default(),
        })
    }

    /// Segmentation loop, until [`Compositor::stop`].
    pub fn segment(&self) {
        let mut ema = Vec::new();
        loop {
            let input = {
                let st = self.shared.state.lock().unwrap();
                let mut st = self
                    .shared
                    .cv
                    .wait_while(st, |s| s.input.is_none() && !s.stop)
                    .unwrap();
                if st.stop {
                    return;
                }
                st.input.take().unwrap()
            };
            match infer(&self.model, input) {
                Ok(probs) => {
                    let mask = update_mask(&mut ema, &probs);
                    let mut st = self.shared.state.lock().unwrap();
                    st.mask = mask;
                    st.seq += 1;
                }
                Err(e) => {
                    log!("camera: background segmentation failed, keeping the last mask: {e}");
                    return;
                }
            }
        }
    }

    pub fn stop(&self) {
        self.shared.state.lock().unwrap().stop = true;
        self.shared.cv.notify_one();
    }

    pub fn applier(&self) -> Applier<'_> {
        Applier {
            c: self,
            blender: Blender::new(self.size),
            mask: Vec::new(),
            seq: 0,
        }
    }
}

impl Applier<'_> {
    /// Queue `frame` for segmentation and blend it with the latest mask.
    pub fn apply(&mut self, frame: &mut [u8]) {
        let c = self.c;
        if frame.len() != c.picture.len() {
            return;
        }
        let input = model_input(frame, c.size);
        {
            let mut st = c.shared.state.lock().unwrap();
            st.input = Some(input);
            if st.seq != self.seq {
                self.seq = st.seq;
                self.mask.clone_from(&st.mask);
            }
        }
        c.shared.cv.notify_one();
        if self.mask.is_empty() {
            frame.copy_from_slice(&c.picture);
        } else {
            self.blender.blend(frame, &c.picture, &self.mask);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// YUYV frame of one colour.
    fn solid((w, h): (u32, u32), y: u8, u: u8, v: u8) -> Vec<u8> {
        [y, u, y, v].repeat((w * h / 2) as usize)
    }

    #[test]
    fn model_input_converts_and_lays_out_nchw() {
        // Limited-range white and black.
        let white = model_input(&solid((640, 360), 235, 128, 128), (640, 360));
        assert_eq!(white.len(), 3 * MW * MH);
        assert!(white.iter().all(|&v| v > 0.99), "{:?}", &white[..4]);
        let black = model_input(&solid((640, 360), 16, 128, 128), (640, 360));
        assert!(black.iter().all(|&v| v < 0.01));
        // Pure-ish red: R plane high, G and B low.
        let red = model_input(&solid((512, 288), 81, 90, 240), (512, 288));
        let plane = |c: usize| red[c * MW * MH];
        assert!(
            plane(0) > 0.9 && plane(1) < 0.1 && plane(2) < 0.1,
            "{red:?}"
        );
    }

    #[test]
    fn mask_smooths_and_has_soft_edges() {
        let mut ema = Vec::new();
        let m = update_mask(&mut ema, &[0.0, 0.2, 0.5, 0.8, 1.0]);
        assert_eq!(m, [0, 0, 128, 255, 255]);
        // The second mask moves 60% of the way.
        let m = update_mask(&mut ema, &[1.0, 1.0, 1.0, 1.0, 0.0]);
        assert_eq!(m[0], 215); // 0.6
        assert_eq!(m[4], 40); // 0.4
    }

    #[test]
    fn taps_cover_the_source() {
        let t = taps(1280, MW);
        assert_eq!(t.len(), 1280);
        assert_eq!(t[0], (0, 1, 0));
        assert_eq!(t[1279].1, MW - 1);
        assert!(t.iter().all(|&(a, b, w)| a < MW && b < MW && w < 256));
    }

    #[test]
    fn blend_keeps_person_and_replaces_background() {
        let size = (64, 36);
        let picture = solid(size, 200, 50, 60);
        let mut blender = Blender::new(size);

        let mut frame = solid(size, 100, 120, 130);
        blender.blend(&mut frame, &picture, &[255; MW * MH]);
        assert_eq!(frame, solid(size, 100, 120, 130));

        blender.blend(&mut frame, &picture, &[0; MW * MH]);
        assert_eq!(frame, picture);

        // Half: halfway between the two.
        let mut frame = solid(size, 100, 120, 130);
        blender.blend(&mut frame, &picture, &[128; MW * MH]);
        assert_eq!(&frame[..4], [150, 85, 150, 95]);
    }

    #[test]
    fn applier_shows_the_picture_until_the_first_mask() {
        let size = (64, 36);
        let c = Compositor::new(solid(size, 200, 50, 60), size).unwrap();
        let mut a = c.applier();
        let mut frame = solid(size, 100, 120, 130);
        a.apply(&mut frame);
        assert_eq!(frame, c.picture);
        assert!(c.shared.state.lock().unwrap().input.is_some());
    }

    #[test]
    fn model_runs() {
        let m = model().unwrap();
        let probs = infer(&m, vec![0.5; 3 * MW * MH]).unwrap();
        assert_eq!(probs.len(), MW * MH);
        assert!(probs.iter().all(|p| (0.0..=1.0).contains(p)));
    }

    #[test]
    fn picture_args_fill_the_webcam() {
        let a = picture_args((1280, 720));
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(
            vf.starts_with("scale=1280:720:force_original_aspect_ratio=increase,crop=1280:720")
        );
        assert!(a.windows(2).any(|p| p == ["-c:v", "mjpeg"]));
    }
}
