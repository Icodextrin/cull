//! Background JPEG decoding: a small worker pool that decodes, orients and downsizes images.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_channel::{select_biased, unbounded, Receiver, Sender};
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::meta::Meta;

/// Long edge of review-grid thumbnails, in physical pixels.
pub const THUMB_PX: u32 = 360;
/// Screen-fit images further than this from the cursor are not worth decoding.
pub const PREFETCH_WINDOW: usize = 4;

/// Tightly packed 8-bit RGBA.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// Screen-sized image (plus a thumbnail) for browsing.
    Fit,
    /// Full resolution image for focus peek.
    Full,
    /// Thumbnail only, for the review grid.
    Thumb,
}

pub enum Payload {
    Fit { fit: Rgba, thumb: Rgba, full_width: u32, full_height: u32, meta: Meta },
    Full(Rgba),
    Thumb(Rgba),
    /// The job became irrelevant before it started.
    Skipped,
}

pub struct Loaded {
    pub idx: usize,
    pub kind: JobKind,
    pub result: Result<Payload, String>,
}

struct Shared {
    current: AtomicUsize,
    peeking: AtomicBool,
    target_w: AtomicU32,
    target_h: AtomicU32,
}

pub struct Loader {
    hi: Sender<(usize, JobKind)>,
    lo: Sender<(usize, JobKind)>,
    pub results: Receiver<Loaded>,
    shared: Arc<Shared>,
}

impl Loader {
    pub fn new(paths: Vec<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Loader {
        let (hi, hi_rx) = unbounded();
        let (lo, lo_rx) = unbounded();
        let (tx, results) = unbounded();
        let shared = Arc::new(Shared {
            current: AtomicUsize::new(0),
            peeking: AtomicBool::new(false),
            target_w: AtomicU32::new(1920),
            target_h: AtomicU32::new(1080),
        });
        let paths = Arc::new(paths);
        let wake = Arc::new(wake);
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).clamp(1, 8));
        for _ in 0..threads {
            let (hi_rx, lo_rx, tx) = (hi_rx.clone(), lo_rx.clone(), tx.clone());
            let (shared, paths, wake) = (shared.clone(), paths.clone(), wake.clone());
            std::thread::spawn(move || {
                let mut resizer = Resizer::new();
                loop {
                    let job = hi_rx.try_recv().or_else(|_| {
                        select_biased! {
                            recv(hi_rx) -> j => j,
                            recv(lo_rx) -> j => j,
                        }
                    });
                    let Ok((idx, kind)) = job else { return };
                    let result = run_job(&paths[idx], idx, kind, &shared, &mut resizer);
                    if tx.send(Loaded { idx, kind, result }).is_err() {
                        return;
                    }
                    wake();
                }
            });
        }
        Loader { hi, lo, results, shared }
    }

    pub fn set_current(&self, idx: usize, peeking: bool) {
        self.shared.current.store(idx, Ordering::Relaxed);
        self.shared.peeking.store(peeking, Ordering::Relaxed);
    }

    pub fn set_target(&self, width: u32, height: u32) {
        self.shared.target_w.store(width.max(1), Ordering::Relaxed);
        self.shared.target_h.store(height.max(1), Ordering::Relaxed);
    }

    pub fn request(&self, idx: usize, kind: JobKind, urgent: bool) {
        let _ = if urgent { &self.hi } else { &self.lo }.send((idx, kind));
    }
}

fn run_job(
    path: &PathBuf,
    idx: usize,
    kind: JobKind,
    shared: &Shared,
    resizer: &mut Resizer,
) -> Result<Payload, String> {
    let current = shared.current.load(Ordering::Relaxed);
    let stale = match kind {
        JobKind::Fit => idx.abs_diff(current) > PREFETCH_WINDOW,
        JobKind::Full => idx != current || !shared.peeking.load(Ordering::Relaxed),
        JobKind::Thumb => false,
    };
    if stale {
        return Ok(Payload::Skipped);
    }

    let (raw, orientation, exif) = decode(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (ow, oh) = oriented_dims(raw.width, raw.height, orientation);
    Ok(match kind {
        JobKind::Full => Payload::Full(orient(raw, orientation)),
        JobKind::Fit => {
            let tw = shared.target_w.load(Ordering::Relaxed);
            let th = shared.target_h.load(Ordering::Relaxed);
            let fit = orient(resize_within(resizer, raw, orientation, tw, th), orientation);
            let (tw, th) = fit_within(fit.width, fit.height, THUMB_PX, THUMB_PX);
            let thumb = resize_to(resizer, &fit, tw, th);
            let meta = Meta::from_exif(exif.as_ref(), ow, oh);
            Payload::Fit { fit, thumb, full_width: ow, full_height: oh, meta }
        }
        JobKind::Thumb => {
            Payload::Thumb(orient(resize_within(resizer, raw, orientation, THUMB_PX, THUMB_PX), orientation))
        }
    })
}

fn decode(path: &PathBuf) -> anyhow::Result<(Rgba, u32, Option<exif::Exif>)> {
    let bytes = std::fs::read(path)?;
    let exif = exif::Reader::new().read_from_container(&mut Cursor::new(&bytes)).ok();
    let orientation = exif
        .as_ref()
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1);

    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&bytes), options);
    let data = decoder.decode()?;
    let (w, h) = decoder.dimensions().ok_or_else(|| anyhow::anyhow!("no dimensions"))?;
    Ok((Rgba { width: w as u32, height: h as u32, data }, orientation, exif))
}

/// Dimensions after applying an EXIF orientation (5-8 swap width and height).
pub fn oriented_dims(w: u32, h: u32, orientation: u32) -> (u32, u32) {
    if orientation >= 5 { (h, w) } else { (w, h) }
}

/// Largest size with the same aspect ratio that fits in `max_w`x`max_h`, never upscaling.
pub fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

/// Downscale an un-oriented image so that, once oriented, it fits in `max_w`x`max_h`.
fn resize_within(resizer: &mut Resizer, img: Rgba, orientation: u32, max_w: u32, max_h: u32) -> Rgba {
    let (ow, oh) = oriented_dims(img.width, img.height, orientation);
    let (fw, fh) = fit_within(ow, oh, max_w, max_h);
    let (dw, dh) = oriented_dims(fw, fh, orientation);
    if (dw, dh) == (img.width, img.height) {
        return img;
    }
    resize_to(resizer, &img, dw, dh)
}

fn resize_to(resizer: &mut Resizer, img: &Rgba, dw: u32, dh: u32) -> Rgba {
    let src = ImageRef::new(img.width, img.height, &img.data, PixelType::U8x4).expect("valid image buffer");
    let mut dst = Image::new(dw, dh, PixelType::U8x4);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)).use_alpha(false);
    resizer.resize(&src, &mut dst, &options).expect("resize");
    Rgba { width: dw, height: dh, data: dst.into_vec() }
}

/// Apply an EXIF orientation so the image displays upright.
pub fn orient(img: Rgba, orientation: u32) -> Rgba {
    if orientation <= 1 || orientation > 8 {
        return img;
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let src: &[u32] = bytemuck::cast_slice(&img.data);
    let (ow, oh) = if orientation >= 5 { (h, w) } else { (w, h) };
    let mut out = vec![0u32; ow * oh];
    for y in 0..oh {
        let row = &mut out[y * ow..(y + 1) * ow];
        for (x, px) in row.iter_mut().enumerate() {
            let (sx, sy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                _ => (w - 1 - y, x), // 8
            };
            *px = src[sy * w + sx];
        }
    }
    Rgba { width: ow as u32, height: oh as u32, data: bytemuck::cast_slice(&out).to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2x1 image: pixel values 1, 2 (in the first byte).
    fn img() -> Rgba {
        Rgba { width: 2, height: 1, data: vec![1, 0, 0, 0, 2, 0, 0, 0] }
    }

    fn firsts(i: &Rgba) -> Vec<u8> {
        i.data.chunks(4).map(|c| c[0]).collect()
    }

    #[test]
    fn orientations() {
        assert_eq!(firsts(&orient(img(), 1)), [1, 2]);
        assert_eq!(firsts(&orient(img(), 2)), [2, 1]);
        assert_eq!(firsts(&orient(img(), 3)), [2, 1]);
        // Rotate 90° clockwise: left pixel ends up on top.
        let r = orient(img(), 6);
        assert_eq!((r.width, r.height), (1, 2));
        assert_eq!(firsts(&r), [1, 2]);
        // Rotate 90° counter-clockwise: right pixel ends up on top.
        assert_eq!(firsts(&orient(img(), 8)), [2, 1]);
    }

    #[test]
    fn fit_never_upscales() {
        assert_eq!(fit_within(6000, 4000, 3000, 3000), (3000, 2000));
        assert_eq!(fit_within(4000, 6000, 3000, 2000), (1333, 2000));
        assert_eq!(fit_within(100, 50, 3000, 2000), (100, 50));
    }
}
