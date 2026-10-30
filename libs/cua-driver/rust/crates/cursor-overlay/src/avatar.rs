// SPDX-License-Identifier: MIT
//! Bounded, renderer-local avatar decoding. Never reads a URL or a file.
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tiny_skia::Pixmap;

pub fn decode_avatar(encoded: &str) -> Option<Pixmap> {
    if encoded.len() > 48 * 1024 {
        return None;
    }
    let bytes = STANDARD.decode(encoded).ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(128);
    limits.max_image_height = Some(128);
    limits.max_alloc = Some(1024 * 1024);
    let decoder =
        image::codecs::png::PngDecoder::with_limits(std::io::Cursor::new(bytes), limits).ok()?;
    use image::ImageDecoder;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > 128 || height > 128 {
        return None;
    }
    let image = image::DynamicImage::from_decoder(decoder).ok()?.to_rgba8();
    let mut pixmap = Pixmap::new(width, height)?;
    for (dest, source) in pixmap.data_mut().chunks_exact_mut(4).zip(image.pixels()) {
        let [r, g, b, a] = source.0;
        let premultiply = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
        dest.copy_from_slice(&[premultiply(r), premultiply(g), premultiply(b), a]);
    }
    let mut mask = tiny_skia::Mask::new(width, height)?;
    let path = crate::session_badge::rounded_rect(
        tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32)?,
        width.min(height) as f32 * 0.2,
    )?;
    mask.fill_path(
        &path,
        tiny_skia::FillRule::Winding,
        true,
        tiny_skia::Transform::identity(),
    );
    pixmap.apply_mask(&mask);
    Some(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_invalid_or_oversized_avatars() {
        assert!(decode_avatar("https://example.com/avatar.png").is_none());
        assert!(decode_avatar(&"A".repeat(48 * 1024 + 1)).is_none());
        let image = image::RgbaImage::new(129, 1);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        assert!(decode_avatar(&STANDARD.encode(bytes.into_inner())).is_none());
    }
}
