use std::io::Cursor;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use windows::{
    Graphics::Imaging::BitmapDecoder,
    Media::Ocr::OcrEngine,
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream},
};

use crate::TransientFrame;

/// Largest image side Windows OCR accepts, used only when the engine itself
/// cannot be asked. `OcrEngine::MaxImageDimension` is the authority; this is
/// the documented value and exists so a failed static-property read cannot
/// silently disable the bound.
const FALLBACK_MAX_IMAGE_DIMENSION: u32 = 10_000;

/// Thin OCR adapter retained from Screenpipe's pinned Windows.Media.Ocr path.
pub struct WindowsOcr;

impl WindowsOcr {
    pub fn preflight(&self) -> Result<()> {
        OcrEngine::TryCreateFromUserProfileLanguages()
            .context("create Windows OCR engine from installed user languages")?;
        Ok(())
    }

    pub async fn recognize(&self, frame: &TransientFrame) -> Result<String> {
        let mut image = frame.to_opaque_rgba_image()?;

        // Windows OCR refuses any bitmap whose longest side exceeds
        // `MaxImageDimension`, and that refusal is permanent in the only sense
        // that matters here: the same display produces the same oversized frame
        // on every tick, so the failure never clears and retrying it is
        // guaranteed waste. Left alone the recorder recognised nothing for the
        // life of the run on such a machine, while still paying for a capture,
        // an RGBA conversion, a PNG encode and a decode every two seconds.
        //
        // Scaling to fit is what makes the frame recoverable rather than merely
        // survivable. The typed `ocr_unavailable` gap and its exponential
        // backoff stay in place for everything else - they bound the cost of a
        // failure, but they cannot turn one into a reading.
        let max_dimension = OcrEngine::MaxImageDimension()
            .ok()
            .filter(|dimension| *dimension > 0)
            .unwrap_or(FALLBACK_MAX_IMAGE_DIMENSION);
        if let Some((width, height)) =
            fit_within_ocr_limit(image.width(), image.height(), max_dimension)
        {
            eprintln!("{}", downscale_line(max_dimension, width, height));
            // Bilinear, not Lanczos3. This runs inside a 2-second cadence on a
            // frame large enough to have failed outright, and a sharper kernel
            // would spend the cadence it was meant to save.
            image = image.resize(width, height, FilterType::Triangle);
        }

        let mut encoded = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
            .context("encode transient frame for Windows OCR")?;

        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(&encoded)?;
        writer.StoreAsync()?.get()?;
        writer.FlushAsync()?.get()?;
        stream.Seek(0)?;

        let decoder =
            BitmapDecoder::CreateWithIdAsync(BitmapDecoder::PngDecoderId()?, &stream)?.get()?;
        let bitmap = decoder.GetSoftwareBitmapAsync()?.get()?;
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .context("create Windows OCR engine from installed user languages")?;
        let result = engine.RecognizeAsync(&bitmap)?.get()?;

        Ok(result.Text()?.to_string())
    }
}

/// Dimensions that fit inside the engine's limit with the aspect ratio intact,
/// or `None` when the frame already fits.
///
/// Both sides are floored to stay inside the bound and then clamped to at least
/// one pixel: an extreme aspect ratio scaled by its longest side alone rounds
/// the short side to zero, and a zero-sided bitmap is simply a different
/// permanent failure.
fn fit_within_ocr_limit(width: u32, height: u32, max_dimension: u32) -> Option<(u32, u32)> {
    let longest = width.max(height);
    if max_dimension == 0 || longest <= max_dimension {
        return None;
    }

    let scale = f64::from(max_dimension) / f64::from(longest);
    let scaled = |side: u32| (f64::from(side) * scale).floor().max(1.0) as u32;
    Some((scaled(width), scaled(height)))
}

/// The downscale diagnostic. Dimensions only: this runs on a path holding the
/// captured frame, and nothing derived from its pixels may reach a log line.
fn downscale_line(max_dimension: u32, width: u32, height: u32) -> String {
    format!(
        "event=ocr_frame_downscaled max_dimension={max_dimension} width={width} height={height}"
    )
}

#[cfg(test)]
mod tests {
    use super::{FALLBACK_MAX_IMAGE_DIMENSION, downscale_line, fit_within_ocr_limit};

    #[test]
    fn a_frame_the_engine_already_accepts_is_never_rescaled() {
        // Resizing a frame that fits would cost a full resample every two
        // seconds and blur text the recognizer could already read.
        for (width, height) in [
            (1_920, 1_080),
            (3_840, 2_160),
            (FALLBACK_MAX_IMAGE_DIMENSION, 4_000),
            (4_000, FALLBACK_MAX_IMAGE_DIMENSION),
        ] {
            assert_eq!(
                fit_within_ocr_limit(width, height, FALLBACK_MAX_IMAGE_DIMENSION),
                None,
                "{width}x{height} fits and must not be rescaled"
            );
        }
    }

    #[test]
    fn an_oversized_frame_is_brought_inside_the_limit_on_both_axes() {
        // A window spanning three 4K displays is the case that produced this:
        // it exceeds the limit on one axis, fails OCR outright, and fails
        // identically on the next tick and every tick after it.
        for (width, height) in [
            (11_520_u32, 2_160_u32),
            (2_160, 11_520),
            (30_000, 30_000),
            (10_001, 10_001),
        ] {
            let (scaled_width, scaled_height) =
                fit_within_ocr_limit(width, height, FALLBACK_MAX_IMAGE_DIMENSION)
                    .unwrap_or_else(|| panic!("{width}x{height} exceeds the limit and must scale"));

            assert!(
                scaled_width <= FALLBACK_MAX_IMAGE_DIMENSION
                    && scaled_height <= FALLBACK_MAX_IMAGE_DIMENSION,
                "{width}x{height} scaled to {scaled_width}x{scaled_height}, still over the limit"
            );
            assert_eq!(
                scaled_width.max(scaled_height),
                FALLBACK_MAX_IMAGE_DIMENSION,
                "{width}x{height} was scaled further than the limit required"
            );
            // Aspect ratio within one pixel of the original, or text is
            // distorted into shapes the recognizer was not trained on.
            let expected_short = (f64::from(width.min(height))
                * f64::from(FALLBACK_MAX_IMAGE_DIMENSION)
                / f64::from(width.max(height)))
            .floor() as u32;
            assert_eq!(scaled_width.min(scaled_height), expected_short);
        }
    }

    #[test]
    fn an_extreme_aspect_ratio_never_produces_a_zero_side() {
        // The naive scale rounds the short side to zero here, and a zero-sided
        // bitmap fails just as permanently as the oversized one it replaced.
        let (width, height) = fit_within_ocr_limit(2_000_000, 3, 10_000).unwrap();

        assert_eq!(width, 10_000);
        assert!(height >= 1, "scaled height collapsed to {height}");
    }

    #[test]
    fn a_zero_limit_is_treated_as_no_answer_rather_than_a_bound() {
        // `MaxImageDimension` returning zero would otherwise scale every frame
        // to a single pixel.
        assert_eq!(fit_within_ocr_limit(1_920, 1_080, 0), None);
    }

    #[test]
    fn the_downscale_diagnostic_carries_only_dimensions() {
        assert_eq!(
            downscale_line(10_000, 10_000, 1_875),
            "event=ocr_frame_downscaled max_dimension=10000 width=10000 height=1875"
        );
    }
}
