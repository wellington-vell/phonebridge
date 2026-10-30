//! Virtual background: the person cut out of each webcam frame and put over a
//! picture chosen in the phone app.
//!
//! The phone sends the picture (JPEG, or an animated GIF) in its video hello;
//! ffmpeg turns it into webcam-size YUYV frames: one for a JPEG, the GIF's
//! loop at [`ANIM_FPS`] for a GIF (scaled with nearest-neighbour, as those
//! are pixel art), played over and over. A segmentation thread runs MediaPipe Selfie
//! Segmentation (models/, Apache-2.0) with tract on the phone's picture area
//! of the newest frame (the landscape model for landscape, the square one for
//! portrait, so the person gets all of the model's resolution), refines the
//! coarse result with a guided filter so its edges follow the real ones in
//! the image, and publishes a half-resolution mask. Every frame is blended
//! with the latest mask, so a slow inference never holds the stream back.
//! Until the first mask the webcam shows only the picture.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tract_onnx::prelude::*;

use crate::log;
use crate::video::{PIX_FMT, frame_size, strings};

/// 256x144, for landscape pictures.
const LANDSCAPE: &[u8] = include_bytes!("../models/selfie_segmentation_landscape.onnx");
/// 256x256, for portrait ones.
const GENERAL: &[u8] = include_bytes!("../models/selfie_segmentation.onnx");
/// Weight of the new inference against the running one (less flicker).
const EMA_NEW: f32 = 0.6;
/// Guided filter window radius, in mask pixels (half the webcam size).
const RADIUS: usize = 4;
/// Guided filter regularization: lower follows image edges more closely.
const EPS: f32 = 0.002;
/// Refined probabilities between these fade from background to person.
const EDGE: (f32, f32) = (0.25, 0.75);
/// Animated backgrounds play at this rate, whatever the GIF's own timing.
const ANIM_FPS: u32 = 10;
/// Memory for an animation's decoded frames (37 MB for a 2 s loop at 720p).
const ANIM_BYTES: usize = 96 << 20;
/// Longest loop kept, in frames (the rest of a longer GIF is dropped).
const ANIM_FRAMES: usize = 60;

// ------------------------------------------------------------------ picture
/// Whether the phone's picture is a GIF (else it is taken for a JPEG).
fn is_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

/// Frames kept of an animation at this webcam size.
fn max_frames(size: (u32, u32)) -> usize {
    (ANIM_BYTES / frame_size(size)).clamp(1, ANIM_FRAMES)
}

/// One-shot ffmpeg: the phone's picture on stdin, cropped to fill the webcam
/// size, YUYV frames out (one for a JPEG). Only the MJPEG or GIF decoder, and
/// allocations and frames capped, as the picture comes from the network.
fn picture_args((w, h): (u32, u32), gif: bool) -> Vec<String> {
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
    ]);
    a.extend(strings(if gif {
        ["gif", "-c:v", "gif"]
    } else {
        ["image2pipe", "-c:v", "mjpeg"]
    }));
    a.extend(strings(["-i", "-", "-vf"]));
    let (fps, flags) = if gif {
        (format!("fps={ANIM_FPS},"), ":flags=neighbor")
    } else {
        (String::new(), "")
    };
    a.push(format!(
        "{fps}scale={w}:{h}:force_original_aspect_ratio=increase{flags},crop={w}:{h},setsar=1,format={PIX_FMT}"
    ));
    let frames = if gif { max_frames((w, h)) } else { 1 };
    a.extend(["-frames:v".into(), frames.to_string()]);
    a.extend(strings(["-pix_fmt", PIX_FMT, "-f", "rawvideo", "-"]));
    a
}

/// The background picture as webcam-size YUYV frames: one for a still
/// picture, the loop for an animated one.
pub fn decode_picture(bytes: &[u8], size: (u32, u32)) -> Result<Vec<Vec<u8>>, String> {
    let mut child = Command::new("ffmpeg")
        .args(picture_args(size, is_gif(bytes)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Fed from a thread: ffmpeg may fill stdout before reading all of stdin.
    let out = thread::scope(|s| {
        s.spawn(move || {
            let _ = stdin.write_all(bytes);
        });
        child.wait_with_output()
    })
    .map_err(|e| format!("ffmpeg: {e}"))?;
    let n = frame_size(size);
    if out.status.success() && !out.stdout.is_empty() && out.stdout.len() % n == 0 {
        Ok(out.stdout.chunks_exact(n).map(<[u8]>::to_vec).collect())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.lines().next().unwrap_or("no picture decoded");
        Err(format!("cannot decode the background picture: {err}"))
    }
}

// -------------------------------------------------------------------- model
/// A segmentation model and its input / output size.
struct Net {
    plan: Arc<TypedRunnableModel>,
    w: usize,
    h: usize,
}

fn load(bytes: &[u8], w: usize, h: usize) -> Result<Net, String> {
    (|| -> TractResult<Net> {
        // The files' shape hints use a symbolic batch size; pin it to 1.
        let plan = tract_onnx::onnx()
            .with_ignore_output_shapes(true)
            .with_ignore_value_info(true)
            .model_for_read(&mut io::Cursor::new(bytes))?
            .with_input_fact(0, f32::fact([1, 3, h, w]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Net { plan, w, h })
    })()
    .map_err(|e| format!("segmentation model: {e}"))
}

/// The model for a picture area of this shape; loaded on first use, then
/// shared by every session.
fn net(landscape: bool) -> Result<&'static Net, String> {
    static NETS: [OnceLock<Result<Net, String>>; 2] = [OnceLock::new(), OnceLock::new()];
    let r = if landscape {
        NETS[0].get_or_init(|| load(LANDSCAPE, 256, 144))
    } else {
        NETS[1].get_or_init(|| load(GENERAL, 256, 256))
    };
    r.as_ref().map_err(Clone::clone)
}

impl Net {
    /// Person probability per pixel of an RGB input (NCHW, 0..1).
    fn infer(&self, input: Vec<f32>) -> TractResult<Vec<f32>> {
        let t = Tensor::from_shape(&[1, 3, self.h, self.w], &input)?;
        let out = self.plan.run(tvec!(t.into()))?;
        let m = out[0].to_plain_array_view::<f32>()?;
        if m.len() != self.w * self.h {
            return Err(TractError::msg(format!(
                "unexpected mask shape {:?}",
                m.shape()
            )));
        }
        Ok(m.iter().copied().collect())
    }
}

// ------------------------------------------------------------------ pixels
/// Where the phone's picture is in the webcam frame; the rest is black bars.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn full((w, h): (u32, u32)) -> Self {
        Rect {
            x: 0,
            y: 0,
            w: w as usize,
            h: h as usize,
        }
    }

    /// The same area in a frame scaled down by `div`.
    fn scaled(self, div: usize) -> Self {
        Rect {
            x: self.x / div,
            y: self.y / div,
            w: (self.w / div).max(1),
            h: (self.h / div).max(1),
        }
    }
}

/// The model's input from the picture area of a YUYV frame (`w` wide):
/// stretched to the model size (as MediaPipe does), nearest sample per cell,
/// BT.601 limited range (what ffmpeg's scale writes) to RGB, NCHW.
fn model_input(yuyv: &[u8], w: usize, r: Rect, (nw, nh): (usize, usize)) -> Vec<f32> {
    let mut out = vec![0f32; 3 * nw * nh];
    for my in 0..nh {
        let sy = r.y + (2 * my + 1) * r.h / (2 * nh);
        for mx in 0..nw {
            let sx = r.x + (2 * mx + 1) * r.w / (2 * nw);
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
                out[(c * nh + my) * nw + mx] = (val / 255.0).clamp(0.0, 1.0);
            }
        }
    }
    out
}

/// Luma at half size (2x2 averages), 0..1: the guided filter's guide.
fn half_luma(yuyv: &[u8], (w, h): (usize, usize)) -> Vec<f32> {
    let (gw, gh) = (w / 2, h / 2);
    let mut out = Vec::with_capacity(gw * gh);
    for gy in 0..gh {
        let (a, b) = (
            &yuyv[2 * gy * w * 2..][..w * 2],
            &yuyv[(2 * gy + 1) * w * 2..][..w * 2],
        );
        for gx in 0..gw {
            // Two pixels per 4 bytes: Y at 0 and 2.
            let i = gx * 4;
            let sum = u32::from(a[i]) + u32::from(a[i + 2]) + u32::from(b[i]) + u32::from(b[i + 2]);
            out.push(sum as f32 / (4.0 * 255.0));
        }
    }
    out
}

/// Fold a new inference into the running one.
fn smooth(ema: &mut Vec<f32>, probs: &[f32]) {
    if ema.len() != probs.len() {
        ema.clear();
        ema.extend_from_slice(probs);
    } else {
        for (e, p) in ema.iter_mut().zip(probs) {
            *e += (p - *e) * EMA_NEW;
        }
    }
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

/// The `(sw, sh)` probabilities stretched bilinearly over `r` of a `(w, h)`
/// map; 0 outside it.
fn upsample(src: &[f32], (sw, sh): (usize, usize), (w, h): (usize, usize), r: Rect) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    let cols = taps(r.w, sw);
    for (y, &(r0, r1, wy)) in taps(r.h, sh).iter().enumerate() {
        let wy = wy as f32 / 256.0;
        let row = &mut out[(r.y + y) * w + r.x..][..r.w];
        for (o, &(c0, c1, wx)) in row.iter_mut().zip(&cols) {
            let wx = wx as f32 / 256.0;
            let lerp = |row: usize| src[row * sw + c0] * (1.0 - wx) + src[row * sw + c1] * wx;
            *o = lerp(r0) * (1.0 - wy) + lerp(r1) * wy;
        }
    }
    out
}

/// Mean over a (2r+1)² window, clamped at the borders (separable running
/// sums; the vertical pass adds and drops whole rows, for the cache).
fn box_mean(src: &[f32], (w, h): (usize, usize), r: usize) -> Vec<f32> {
    let mut tmp = vec![0f32; w * h];
    for (row, out) in src.chunks_exact(w).zip(tmp.chunks_exact_mut(w)) {
        let mut sum: f32 = row[..=r.min(w - 1)].iter().sum();
        for x in 0..w {
            let (lo, hi) = (x.saturating_sub(r), (x + r).min(w - 1));
            out[x] = sum / (hi - lo + 1) as f32;
            if x + r + 1 < w {
                sum += row[x + r + 1];
            }
            if x >= r {
                sum -= row[x - r];
            }
        }
    }
    let mut out = vec![0f32; w * h];
    let mut sums = vec![0f32; w];
    for row in tmp.chunks_exact(w).take(r + 1) {
        sums.iter_mut().zip(row).for_each(|(s, v)| *s += v);
    }
    for y in 0..h {
        let (lo, hi) = (y.saturating_sub(r), (y + r).min(h - 1));
        let n = (hi - lo + 1) as f32;
        out[y * w..][..w]
            .iter_mut()
            .zip(&sums)
            .for_each(|(o, s)| *o = s / n);
        if y + r + 1 < h {
            let add = &tmp[(y + r + 1) * w..][..w];
            sums.iter_mut().zip(add).for_each(|(s, v)| *s += v);
        }
        if y >= r {
            let drop = &tmp[(y - r) * w..][..w];
            sums.iter_mut().zip(drop).for_each(|(s, v)| *s -= v);
        }
    }
    out
}

/// Guided filter (He et al.): `p` made locally linear in the guide `i`, so
/// the mask's edges snap to the image's instead of the model's coarse grid.
fn guided(i: &[f32], p: &[f32], size: (usize, usize)) -> Vec<f32> {
    let mean = |v: &[f32]| box_mean(v, size, RADIUS);
    let (mi, mp) = (mean(i), mean(p));
    let ip: Vec<f32> = i.iter().zip(p).map(|(a, b)| a * b).collect();
    let ii: Vec<f32> = i.iter().map(|a| a * a).collect();
    let (mip, mii) = (mean(&ip), mean(&ii));
    let mut a = vec![0f32; i.len()];
    let mut b = vec![0f32; i.len()];
    for k in 0..i.len() {
        let var = mii[k] - mi[k] * mi[k];
        let cov = mip[k] - mi[k] * mp[k];
        a[k] = cov / (var + EPS);
        b[k] = mp[k] - a[k] * mi[k];
    }
    let (ma, mb) = (mean(&a), mean(&b));
    i.iter()
        .zip(ma.iter().zip(&mb))
        .map(|(g, (a, b))| a * g + b)
        .collect()
}

/// Refined probabilities to 0..255 weights with a soft edge.
fn weights(q: &[f32]) -> Vec<u8> {
    q.iter()
        .map(|&p| {
            let t = ((p - EDGE.0) / (EDGE.1 - EDGE.0)).clamp(0.0, 1.0);
            (t * t * (3.0 - 2.0 * t) * 255.0).round() as u8
        })
        .collect()
}

/// Upsamples the mask to the webcam size and blends the frame over the
/// picture, in place.
struct Blender {
    mw: usize,
    cols: Vec<(usize, usize, u32)>,
    rows: Vec<(usize, usize, u32)>,
    /// One upsampled mask row.
    line: Vec<u32>,
}

impl Blender {
    fn new((w, h): (u32, u32), (mw, mh): (usize, usize)) -> Self {
        Blender {
            mw,
            cols: taps(w as usize, mw),
            rows: taps(h as usize, mh),
            line: vec![0; w as usize],
        }
    }

    fn blend(&mut self, frame: &mut [u8], picture: &[u8], mask: &[u8]) {
        let (w, mw) = (self.cols.len(), self.mw);
        for (y, &(r0, r1, wy)) in self.rows.iter().enumerate() {
            let (top, bottom) = (&mask[r0 * mw..][..mw], &mask[r1 * mw..][..mw]);
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
/// One frame for the segmentation thread.
struct Job {
    input: Vec<f32>,
    guide: Vec<f32>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

#[derive(Default)]
struct State {
    /// Newest frame, waiting for the segmentation thread.
    job: Option<Job>,
    /// Latest mask (half the webcam size) and its sequence number, bumped
    /// per mask.
    mask: Vec<u8>,
    seq: u64,
    stop: bool,
}

/// One phone connection's background: [`Compositor::segment`] runs on its own
/// thread, [`Applier::apply`] on the decoded frames.
pub struct Compositor {
    size: (u32, u32),
    /// The phone's picture area in the webcam frame.
    rect: Rect,
    /// The background: one frame, or an animation's loop at [`ANIM_FPS`].
    pictures: Vec<Vec<u8>>,
    start: Instant,
    net: &'static Net,
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
    /// `pictures`: from [`decode_picture`] (at least one), at `size`; `rect`:
    /// where the phone's picture is in each frame.
    pub fn new(pictures: Vec<Vec<u8>>, size: (u32, u32), rect: Rect) -> Result<Self, String> {
        if pictures.is_empty() {
            return Err("no background picture".into());
        }
        Ok(Compositor {
            size,
            rect,
            pictures,
            start: Instant::now(),
            net: net(rect.w >= rect.h)?,
            shared: Shared::default(),
        })
    }

    /// The background to show now: an animation plays in a loop.
    fn picture(&self) -> &[u8] {
        self.picture_at(self.start.elapsed())
    }

    fn picture_at(&self, t: Duration) -> &[u8] {
        let frame = Duration::from_secs(1) / ANIM_FPS;
        let i = t.as_nanos() / frame.as_nanos();
        &self.pictures[(i % self.pictures.len() as u128) as usize]
    }

    fn mask_size(&self) -> (usize, usize) {
        (self.size.0 as usize / 2, self.size.1 as usize / 2)
    }

    /// Segmentation loop, until [`Compositor::stop`].
    pub fn segment(&self) {
        let mut ema = Vec::new();
        let size = self.mask_size();
        let rect = self.rect.scaled(2);
        loop {
            let job = {
                let st = self.shared.state.lock().unwrap();
                let mut st = self
                    .shared
                    .cv
                    .wait_while(st, |s| s.job.is_none() && !s.stop)
                    .unwrap();
                if st.stop {
                    return;
                }
                st.job.take().unwrap()
            };
            match self.net.infer(job.input) {
                Ok(probs) => {
                    smooth(&mut ema, &probs);
                    let p = upsample(&ema, (self.net.w, self.net.h), size, rect);
                    let mask = weights(&guided(&job.guide, &p, size));
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
            blender: Blender::new(self.size, self.mask_size()),
            mask: Vec::new(),
            seq: 0,
        }
    }
}

impl Applier<'_> {
    /// Queue `frame` for segmentation and blend it with the latest mask.
    pub fn apply(&mut self, frame: &mut [u8]) {
        let c = self.c;
        let picture = c.picture();
        if frame.len() != picture.len() {
            return;
        }
        let (w, h) = (c.size.0 as usize, c.size.1 as usize);
        let job = Job {
            input: model_input(frame, w, c.rect, (c.net.w, c.net.h)),
            guide: half_luma(frame, (w, h)),
        };
        {
            let mut st = c.shared.state.lock().unwrap();
            st.job = Some(job);
            if st.seq != self.seq {
                self.seq = st.seq;
                self.mask.clone_from(&st.mask);
            }
        }
        c.shared.cv.notify_one();
        if self.mask.is_empty() {
            frame.copy_from_slice(picture);
        } else {
            self.blender.blend(frame, picture, &self.mask);
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
        let n = (256, 144);
        // Limited-range white and black.
        let size = (640, 360);
        let white = model_input(&solid(size, 235, 128, 128), 640, Rect::full(size), n);
        assert_eq!(white.len(), 3 * 256 * 144);
        assert!(white.iter().all(|&v| v > 0.99), "{:?}", &white[..4]);
        let black = model_input(&solid(size, 16, 128, 128), 640, Rect::full(size), n);
        assert!(black.iter().all(|&v| v < 0.01));
        // Pure-ish red: R plane high, G and B low.
        let size = (512, 288);
        let red = model_input(&solid(size, 81, 90, 240), 512, Rect::full(size), n);
        let plane = |c: usize| red[c * 256 * 144];
        assert!(
            plane(0) > 0.9 && plane(1) < 0.1 && plane(2) < 0.1,
            "{red:?}"
        );
    }

    #[test]
    fn model_input_reads_only_the_picture_area() {
        // White picture in the middle third, black bars around it.
        let (w, h) = (96usize, 32usize);
        let mut f = solid((w as u32, h as u32), 16, 128, 128);
        for y in 0..h {
            for x in 32..64 {
                f[(y * w + x) * 2] = 235;
            }
        }
        let r = Rect {
            x: 32,
            y: 0,
            w: 32,
            h: 32,
        };
        let input = model_input(&f, w, r, (8, 8));
        assert!(input.iter().all(|&v| v > 0.99));
    }

    #[test]
    fn half_luma_averages() {
        let mut f = solid((4, 2), 16, 128, 128);
        f[0] = 255; // top-left Y
        let g = half_luma(&f, (4, 2));
        assert_eq!(g.len(), 2);
        assert!((g[0] - (255.0 + 3.0 * 16.0) / (4.0 * 255.0)).abs() < 1e-6);
        assert!((g[1] - 16.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn smoothing_moves_part_way() {
        let mut ema = Vec::new();
        smooth(&mut ema, &[0.0, 1.0]);
        assert_eq!(ema, [0.0, 1.0]);
        smooth(&mut ema, &[1.0, 0.0]);
        assert!((ema[0] - 0.6).abs() < 1e-6 && (ema[1] - 0.4).abs() < 1e-6);
        assert_eq!(weights(&[0.0, 0.25, 0.5, 0.75, 1.0]), [0, 0, 128, 255, 255]);
    }

    #[test]
    fn taps_cover_the_source() {
        let t = taps(1280, 256);
        assert_eq!(t.len(), 1280);
        assert_eq!(t[0], (0, 1, 0));
        assert_eq!(t[1279].1, 255);
        assert!(t.iter().all(|&(a, b, w)| a < 256 && b < 256 && w < 256));
    }

    #[test]
    fn upsample_fills_only_the_rect() {
        let r = Rect {
            x: 2,
            y: 1,
            w: 4,
            h: 2,
        };
        let m = upsample(&[1.0; 4], (2, 2), (8, 4), r);
        for y in 0..4 {
            for x in 0..8 {
                let inside = (2..6).contains(&x) && (1..3).contains(&y);
                assert_eq!(m[y * 8 + x], if inside { 1.0 } else { 0.0 }, "{x},{y}");
            }
        }
    }

    #[test]
    fn box_mean_clamps_at_borders() {
        let m = box_mean(&[0.0, 0.0, 3.0, 0.0, 0.0], (5, 1), 1);
        assert_eq!(m, [0.0, 1.0, 1.0, 1.0, 0.0]);
        let flat = box_mean(&[2.0; 12], (4, 3), 2);
        assert!(flat.iter().all(|&v| (v - 2.0).abs() < 1e-6));
    }

    #[test]
    fn guided_filter_snaps_to_image_edges() {
        // Image: bright left half, dark right half. Mask: a blurry ramp whose
        // midpoint sits on the edge. The filtered mask is sharper at the edge.
        let (w, h) = (16, 4);
        let i: Vec<f32> = (0..w * h)
            .map(|k| if k % w < 8 { 0.9 } else { 0.1 })
            .collect();
        let p: Vec<f32> = (0..w * h).map(|k| (k % w) as f32 / 15.0).collect();
        let q = guided(&i, &p, (w, h));
        let step = |m: &[f32]| m[8] - m[7];
        assert!(step(&q) > 3.0 * step(&p), "{:?}", &q[..w]);
    }

    #[test]
    fn blend_keeps_person_and_replaces_background() {
        let size = (64, 36);
        let ms = (32, 18);
        let picture = solid(size, 200, 50, 60);
        let mut blender = Blender::new(size, ms);

        let mut frame = solid(size, 100, 120, 130);
        blender.blend(&mut frame, &picture, &[255; 32 * 18]);
        assert_eq!(frame, solid(size, 100, 120, 130));

        blender.blend(&mut frame, &picture, &[0; 32 * 18]);
        assert_eq!(frame, picture);

        // Half: halfway between the two.
        let mut frame = solid(size, 100, 120, 130);
        blender.blend(&mut frame, &picture, &[128; 32 * 18]);
        assert_eq!(&frame[..4], [150, 85, 150, 95]);
    }

    #[test]
    fn applier_shows_the_picture_until_the_first_mask() {
        let size = (64, 36);
        let c = Compositor::new(vec![solid(size, 200, 50, 60)], size, Rect::full(size)).unwrap();
        let mut a = c.applier();
        let mut frame = solid(size, 100, 120, 130);
        a.apply(&mut frame);
        assert_eq!(frame, c.pictures[0]);
        assert!(c.shared.state.lock().unwrap().job.is_some());
    }

    #[test]
    fn models_run() {
        for (landscape, n) in [(true, (256, 144)), (false, (256, 256))] {
            let net = net(landscape).unwrap();
            assert_eq!((net.w, net.h), n);
            let probs = net.infer(vec![0.5; 3 * n.0 * n.1]).unwrap();
            assert_eq!(probs.len(), n.0 * n.1);
            assert!(probs.iter().all(|p| (0.0..=1.0).contains(p)));
        }
    }

    #[test]
    fn picture_args_fill_the_webcam() {
        let a = picture_args((1280, 720), false);
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(
            vf.starts_with("scale=1280:720:force_original_aspect_ratio=increase,crop=1280:720")
        );
        assert!(a.windows(2).any(|p| p == ["-c:v", "mjpeg"]));
        assert!(a.windows(2).any(|p| p == ["-frames:v", "1"]));
    }

    #[test]
    fn gif_args_play_crisp_and_capped() {
        assert!(is_gif(b"GIF89a...") && is_gif(b"GIF87a") && !is_gif(b"\xff\xd8\xff"));
        let a = picture_args((1280, 720), true);
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(vf.starts_with(
            "fps=10,scale=1280:720:force_original_aspect_ratio=increase:flags=neighbor,crop="
        ));
        assert!(a.windows(2).any(|p| p == ["-c:v", "gif"]));
        assert!(a.windows(2).any(|p| p == ["-frames:v", "54"]));
        assert_eq!(max_frames((1920, 1080)), 24);
        assert_eq!(max_frames((640, 360)), ANIM_FRAMES);
    }

    #[test]
    fn animation_loops_in_time() {
        let size = (8, 2);
        let c = Compositor::new(
            vec![solid(size, 1, 0, 0), solid(size, 2, 0, 0)],
            size,
            Rect::full(size),
        )
        .unwrap();
        let at = |ms| c.picture_at(Duration::from_millis(ms))[0];
        assert_eq!(
            [at(0), at(99), at(100), at(199), at(200), at(350)],
            [1, 1, 2, 2, 1, 2]
        );
        assert!(Compositor::new(Vec::new(), size, Rect::full(size)).is_err());
    }
}
