//! JPEGs of frames for clients' live monitors.

use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, RgbImage, imageops};

use crate::frames::{Frame, Kind};

/// `frame` as a JPEG of `quality` (0 to 1), scaled down to at most
/// `max_width` wide: the bytes, width and height.
pub fn encode(frame: &Frame, max_width: Option<u32>, quality: f32) -> Result<(Vec<u8>, u32, u32)> {
    frame.sync()?;
    let mut rgb = rgb(frame)?;
    if let Some(max) = max_width.filter(|max| *max > 0 && *max < rgb.width()) {
        let height = (u64::from(rgb.height()) * u64::from(max) / u64::from(rgb.width())).max(1);
        rgb = imageops::resize(&rgb, max, height as u32, imageops::FilterType::Triangle);
    }
    let quality = (quality.clamp(0.0, 1.0) * 100.0).round().max(1.0) as u8;
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .context("couldn't encode a JPEG")?;
    Ok((bytes, rgb.width(), rgb.height()))
}

/// The frame's pixels as 8-bit RGB.
fn rgb(frame: &Frame) -> Result<RgbImage> {
    let (width, height) = (frame.width(), frame.height());
    let mut out = RgbImage::new(width, height);
    match frame.kind() {
        Kind::Bgra => {
            let plane = frame.layout().planes[0];
            let pixels = frame.plane(0);
            for (y, row) in out.rows_mut().enumerate() {
                let src = &pixels[y * plane.row_pitch..];
                for (x, pixel) in row.enumerate() {
                    let bgra = &src[x * 4..x * 4 + 3];
                    pixel.0 = [bgra[2], bgra[1], bgra[0]];
                }
            }
        }
        Kind::YCbCr => {
            let (luma_plane, chroma_plane) = (frame.layout().planes[0], frame.layout().planes[1]);
            let (luma, chroma) = (frame.plane(0), frame.plane(1));
            for (y, row) in out.rows_mut().enumerate() {
                for (x, pixel) in row.enumerate() {
                    let l = f32::from(luma[y * luma_plane.row_pitch + x]);
                    let c = (y / 2) * chroma_plane.row_pitch + (x / 2) * 2;
                    let (cb, cr) = (
                        f32::from(chroma[c]) - 128.0,
                        f32::from(chroma[c + 1]) - 128.0,
                    );
                    // Rec. 709, video range.
                    let l = (l - 16.0) * 255.0 / 219.0;
                    let (cb, cr) = (cb * 255.0 / 224.0, cr * 255.0 / 224.0);
                    let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
                    pixel.0 = [
                        clamp(l + 1.5748 * cr),
                        clamp(l - 0.1873 * cb - 0.4681 * cr),
                        clamp(l + 1.8556 * cb),
                    ];
                }
            }
        }
    }
    Ok(out)
}
