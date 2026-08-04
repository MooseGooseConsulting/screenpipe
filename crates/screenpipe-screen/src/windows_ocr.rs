use std::io::Cursor;

use anyhow::{Context, Result};
use windows::{
    Graphics::Imaging::BitmapDecoder,
    Media::Ocr::OcrEngine,
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream},
};

use crate::TransientFrame;

/// Thin OCR adapter retained from Screenpipe's pinned Windows.Media.Ocr path.
pub struct WindowsOcr;

impl WindowsOcr {
    pub fn preflight(&self) -> Result<()> {
        OcrEngine::TryCreateFromUserProfileLanguages()
            .context("create Windows OCR engine from installed user languages")?;
        Ok(())
    }

    pub async fn recognize(&self, frame: &TransientFrame) -> Result<String> {
        let image = frame.to_opaque_rgba_image()?;
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
