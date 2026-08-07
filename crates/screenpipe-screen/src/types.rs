use std::fmt;

use anyhow::{Result, bail};
use image::{DynamicImage, RgbaImage};
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq, Eq)]
pub struct ForegroundMetadata {
    pub window_handle: isize,
    pub app_key: String,
    pub app_title: String,
    pub window_title: String,
    pub browser_url: Option<String>,
}

impl fmt::Debug for ForegroundMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForegroundMetadata")
            .field("window_handle", &self.window_handle)
            .field("app_key", &self.app_key)
            .field("app_title", &self.app_title)
            .field("window_title", &self.window_title)
            .field(
                "browser_url",
                &self.browser_url.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FrameFingerprint([u8; 32]);

impl fmt::Debug for FrameFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FrameFingerprint")
            .field(&"<redacted>")
            .finish()
    }
}

pub struct TransientFrame {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

impl TransientFrame {
    pub fn from_bgra(width: u32, height: u32, stride: u32, pixels: Vec<u8>) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("transient frame dimensions must be nonzero");
        }
        let packed_stride = width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("transient frame stride overflow"))?;
        if stride < packed_stride {
            bail!("transient frame stride is smaller than BGRA row width");
        }
        let expected_len = stride
            .checked_mul(height)
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(|| anyhow::anyhow!("transient frame byte length overflow"))?;
        if pixels.len() != expected_len {
            bail!(
                "transient frame has {} bytes but layout requires {expected_len}",
                pixels.len()
            );
        }

        Ok(Self {
            pixels,
            width,
            height,
            stride,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Digests only the colour bytes of each pixel, deliberately skipping
    /// alpha. `to_opaque_rgba_image` forces alpha to 255 before OCR, so alpha
    /// can never change what OCR reads - but DWM composites rounded corners,
    /// shadows, and acrylic/mica backdrops with varying alpha. Hashing alpha
    /// therefore reported a visual change on every tick for a static screen,
    /// which pinned the cadence at 2s and defeated the unchanged-frame OCR
    /// cache. Fingerprints are memory-only and never persisted, so changing
    /// the digest domain has no migration cost.
    pub fn fingerprint(&self) -> FrameFingerprint {
        let mut digest = Sha256::new();
        digest.update(b"screenpipe-frame-fingerprint-v2\0");
        digest.update(self.width.to_le_bytes());
        digest.update(self.height.to_le_bytes());

        let packed_stride = usize::try_from(self.width * 4)
            .expect("validated transient frame row width must fit usize");
        let stride =
            usize::try_from(self.stride).expect("validated transient frame stride must fit usize");
        for row in self.pixels.chunks_exact(stride) {
            for pixel in row[..packed_stride].chunks_exact(4) {
                digest.update(&pixel[..3]);
            }
        }

        FrameFingerprint(digest.finalize().into())
    }

    pub(crate) fn to_opaque_rgba_image(&self) -> Result<DynamicImage> {
        let capacity = self
            .width
            .checked_mul(self.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| anyhow::anyhow!("transient RGBA image size overflow"))?;
        let mut rgba = Vec::with_capacity(capacity);
        let packed_stride = usize::try_from(self.width * 4)?;
        let stride = usize::try_from(self.stride)?;
        for row in self.pixels.chunks_exact(stride) {
            for pixel in row[..packed_stride].chunks_exact(4) {
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
        }
        let image = RgbaImage::from_raw(self.width, self.height, rgba)
            .ok_or_else(|| anyhow::anyhow!("failed to construct transient RGBA image"))?;
        Ok(DynamicImage::ImageRgba8(image))
    }
}

impl Drop for TransientFrame {
    fn drop(&mut self) {
        self.pixels.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::TransientFrame;

    #[test]
    fn fingerprint_is_stable_for_identical_visual_bgra_rows() {
        let packed = TransientFrame::from_bgra(2, 1, 8, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let padded =
            TransientFrame::from_bgra(2, 1, 12, vec![1, 2, 3, 4, 5, 6, 7, 8, 91, 92, 93, 94])
                .unwrap();

        assert_eq!(packed.fingerprint(), padded.fingerprint());
    }

    #[test]
    fn fingerprint_changes_when_any_colour_channel_changes() {
        // Bytes 4, 5, 6 are the blue, green, and red channels of the second
        // BGRA pixel. Do not assert on byte 7 - that is alpha, which the
        // fingerprint ignores on purpose.
        //
        // Every channel is probed separately: a fixture that varies only red
        // leaves the digest free to drop green or blue entirely, and a frame
        // whose only change was in the dropped channel would then be treated
        // as unchanged - serving stale cached OCR and holding the cadence at
        // its stable interval.
        for (offset, channel) in [(0_usize, "blue"), (1, "green"), (2, "red")] {
            let baseline = vec![1, 2, 3, 4, 5, 6, 7, 8];
            let mut changed = baseline.clone();
            changed[4 + offset] ^= 0xFF;

            let before = TransientFrame::from_bgra(2, 1, 8, baseline).unwrap();
            let after = TransientFrame::from_bgra(2, 1, 8, changed).unwrap();

            assert_ne!(
                before.fingerprint(),
                after.fingerprint(),
                "the {channel} channel must be inside the fingerprint digest domain"
            );
        }
    }

    #[test]
    fn fingerprint_ignores_alpha_because_ocr_never_sees_it() {
        // DWM composites rounded corners, shadows, and acrylic backdrops with
        // varying alpha. OCR reads the frame through `to_opaque_rgba_image`,
        // which forces alpha to 255, so an alpha-only delta is not a visual
        // change and must not force a re-OCR or reset the cadence.
        let opaque = TransientFrame::from_bgra(2, 1, 8, vec![1, 2, 3, 255, 5, 6, 7, 255]).unwrap();
        let translucent =
            TransientFrame::from_bgra(2, 1, 8, vec![1, 2, 3, 7, 5, 6, 7, 64]).unwrap();

        assert_eq!(opaque.fingerprint(), translucent.fingerprint());
    }

    #[test]
    fn fingerprint_ignores_padding_between_multiple_visual_rows() {
        let packed = TransientFrame::from_bgra(
            2,
            2,
            8,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18],
        )
        .unwrap();
        let padded = TransientFrame::from_bgra(
            2,
            2,
            12,
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 91, 92, 93, 94, 11, 12, 13, 14, 15, 16, 17, 18, 95, 96, 97,
                98,
            ],
        )
        .unwrap();

        assert_eq!(packed.fingerprint(), padded.fingerprint());
    }

    #[test]
    fn fingerprint_observes_a_second_row_pixel_after_interrow_padding() {
        let before = TransientFrame::from_bgra(
            2,
            2,
            12,
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 91, 92, 93, 94, 11, 12, 13, 14, 15, 16, 17, 18, 95, 96, 97,
                98,
            ],
        )
        .unwrap();
        let after = TransientFrame::from_bgra(
            2,
            2,
            12,
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 91, 92, 93, 94, 11, 12, 13, 14, 15, 16, 19, 18, 95, 96, 97,
                98,
            ],
        )
        .unwrap();

        assert_ne!(before.fingerprint(), after.fingerprint());
    }

    #[test]
    fn fingerprint_includes_frame_dimensions() {
        let wide = TransientFrame::from_bgra(2, 1, 8, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let tall = TransientFrame::from_bgra(1, 2, 4, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

        assert_ne!(wide.fingerprint(), tall.fingerprint());
    }
}
