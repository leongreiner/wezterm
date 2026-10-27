//! Screencast frames: JPEG -> RGBA, and the latest-wins slot the GUI reads from.

use crate::Frame;
use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Absolute decoded size above which a frame is refused (a corrupt header must not allocate
/// gigabytes); callers pass a lower bound derived from the view size.
const MAX_PIXELS: usize = 16384 * 16384 / 4;

pub(crate) struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode a JPEG to opaque RGBA. `recycle` donates a buffer to avoid a fresh multi-megabyte
/// allocation (and its page faults) per frame.
pub(crate) fn decode_jpeg(data: &[u8], recycle: Option<Vec<u8>>, max_pixels: usize) -> Result<Decoded> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), options);
    dec.decode_headers().map_err(|e| anyhow!("jpeg header: {e:?}"))?;
    let info = dec.info().ok_or_else(|| anyhow!("jpeg without image info"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || w * h > max_pixels.min(MAX_PIXELS) {
        bail!("implausible jpeg size {w}x{h}");
    }
    let len = dec
        .output_buffer_size()
        .ok_or_else(|| anyhow!("jpeg output size unknown"))?;
    // The renderer builds a `w * h * 4` byte view of the frame: the size must be exact.
    if len != w * h * 4 {
        bail!("jpeg output of {len} bytes for {w}x{h}");
    }
    let mut buf = recycle.unwrap_or_default();
    buf.resize(len, 0);
    dec.decode_into(&mut buf).map_err(|e| anyhow!("jpeg decode: {e:?}"))?;
    Ok(Decoded {
        width: w as u32,
        height: h as u32,
        rgba: buf,
    })
}

const SAMPLE_CAP: usize = 16384;
static DECODE_SAMPLES_US: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Record how long one decode took (histogram `hivegrid_web.decode` plus a bounded sample list
/// that `take_decode_times` drains, for the spike and for diagnostics).
pub(crate) fn record_decode(d: Duration) {
    metrics::histogram!("hivegrid_web.decode").record(d.as_secs_f64());
    let mut s = DECODE_SAMPLES_US.lock();
    if s.len() < SAMPLE_CAP {
        s.push(d.as_micros().min(u32::MAX as u128) as u32);
    }
}

/// Drain the recorded decode durations (oldest first, at most 16384 since the last call).
pub fn take_decode_times() -> Vec<Duration> {
    std::mem::take(&mut *DECODE_SAMPLES_US.lock())
        .into_iter()
        .map(|us| Duration::from_micros(us as u64))
        .collect()
}

/// The most recent decoded frame of a view. Producers overwrite, never queue: a consumer that
/// is slower than Chrome sees the newest frame and skips the ones in between.
#[derive(Default)]
pub(crate) struct FrameSlot {
    latest: Mutex<Option<Arc<Frame>>>,
    spare: Mutex<Option<Vec<u8>>>,
    seq: AtomicU64,
}

impl FrameSlot {
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest.lock().clone()
    }

    /// Sequence number of the latest frame (0 = none yet).
    #[cfg(test)]
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// A pixel buffer to decode into: the one the previous frame had, when nobody else holds
    /// that frame any more.
    pub fn take_spare(&self) -> Option<Vec<u8>> {
        self.spare.lock().take()
    }

    pub fn store(&self, d: Decoded) -> Arc<Frame> {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let frame = Arc::new(Frame {
            seq,
            width: d.width,
            height: d.height,
            rgba: d.rgba,
        });
        let old = self.latest.lock().replace(frame.clone());
        if let Some(Ok(f)) = old.map(Arc::try_unwrap) {
            *self.spare.lock() = Some(f.rgba);
        }
        frame
    }

    /// Forget the frame (browser gone): the pane falls back to its placeholder.
    pub fn clear(&self) {
        *self.latest.lock() = None;
        *self.spare.lock() = None;
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ExtendedColorType, ImageEncoder};

    /// A solid-colour JPEG, as Chrome would send for a flat page.
    pub fn solid_jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..w * h {
            px.extend_from_slice(&rgb);
        }
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 90)
            .write_image(&px, w, h, ExtendedColorType::Rgb8)
            .unwrap();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::solid_jpeg;
    use super::*;

    #[test]
    fn decodes_to_opaque_rgba_with_the_right_colour() {
        let jpeg = solid_jpeg(64, 48, [255, 0, 255]);
        let d = decode_jpeg(&jpeg, None, MAX_PIXELS).unwrap();
        assert_eq!((d.width, d.height), (64, 48));
        assert_eq!(d.rgba.len(), 64 * 48 * 4);
        let centre = &d.rgba[((24 * 64 + 32) * 4) as usize..][..4];
        assert!(centre[0] >= 250 && centre[1] <= 6 && centre[2] >= 250, "{centre:?}");
        assert_eq!(centre[3], 255);
    }

    #[test]
    fn garbage_and_truncated_input_is_an_error_not_a_panic() {
        assert!(decode_jpeg(b"not a jpeg", None, MAX_PIXELS).is_err());
        assert!(decode_jpeg(&[], None, MAX_PIXELS).is_err());
        let jpeg = solid_jpeg(64, 48, [1, 2, 3]);
        // A truncated stream either errors or decodes best-effort; it must not panic.
        let _ = decode_jpeg(&jpeg[..jpeg.len() / 2], None, MAX_PIXELS);
    }

    #[test]
    fn frames_larger_than_the_caller_allows_are_refused() {
        let jpeg = solid_jpeg(64, 48, [1, 2, 3]);
        assert!(decode_jpeg(&jpeg, None, 64 * 48).is_ok());
        assert!(decode_jpeg(&jpeg, None, 64 * 48 - 1).is_err());
    }

    #[test]
    fn recycled_buffers_are_reused_and_resized() {
        let a = decode_jpeg(&solid_jpeg(32, 32, [9, 9, 9]), None, MAX_PIXELS).unwrap();
        let cap = a.rgba.capacity();
        let b = decode_jpeg(&solid_jpeg(16, 16, [9, 9, 9]), Some(a.rgba), MAX_PIXELS).unwrap();
        assert_eq!(b.rgba.len(), 16 * 16 * 4);
        assert!(b.rgba.capacity() >= 16 * 16 * 4 && b.rgba.capacity() <= cap);
    }

    #[test]
    fn slot_keeps_only_the_latest_frame_and_numbers_them() {
        let slot = FrameSlot::default();
        assert!(slot.latest().is_none());
        assert_eq!(slot.seq(), 0);
        for i in 1..=3u8 {
            let d = decode_jpeg(&solid_jpeg(8, 8, [i * 40, 0, 0]), slot.take_spare(), MAX_PIXELS).unwrap();
            let f = slot.store(d);
            assert_eq!(f.seq, i as u64);
        }
        let latest = slot.latest().unwrap();
        assert_eq!(latest.seq, 3);
        assert_eq!(slot.seq(), 3);
        slot.clear();
        assert!(slot.latest().is_none());
        assert!(slot.take_spare().is_none());
    }

    #[test]
    fn slot_recycles_the_buffer_of_a_frame_nobody_holds() {
        let slot = FrameSlot::default();
        let f1 = slot.store(decode_jpeg(&solid_jpeg(8, 8, [1, 1, 1]), None, MAX_PIXELS).unwrap());
        drop(f1);
        slot.store(decode_jpeg(&solid_jpeg(8, 8, [2, 2, 2]), None, MAX_PIXELS).unwrap());
        assert!(slot.take_spare().is_some(), "frame 1 was unreferenced when replaced");

        // A frame the GUI still holds is not recycled.
        let held = slot.latest().unwrap();
        slot.store(decode_jpeg(&solid_jpeg(8, 8, [3, 3, 3]), None, MAX_PIXELS).unwrap());
        assert!(slot.take_spare().is_none());
        assert_eq!(held.seq, 2);
    }

    #[test]
    fn decode_samples_are_drained() {
        let _ = take_decode_times();
        record_decode(Duration::from_micros(1500));
        record_decode(Duration::from_micros(500));
        let s = take_decode_times();
        assert!(s.contains(&Duration::from_micros(1500)));
    }
}
