use anyhow::{Result, bail};
use image::{DynamicImage, RgbaImage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundMetadata {
    pub window_handle: isize,
    pub app_key: String,
    pub app_title: String,
    pub window_title: String,
    pub browser_url: Option<String>,
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
