//! Decode, orient, shrink and re-encode pictures (feature `image`).
//!
//! The apps each had a copy of this: Proton Stream (JPEG quality 88, PNG for
//! alpha), proton-drive-linux (JPEG quality 82) and Proton Gallery's desktop
//! (decode only). One version, so every app gets the same EXIF orientation
//! handling and decode limits.
//!
//! Only JPEG, PNG and WebP decoding is enabled here. An app that needs more
//! formats enables them on its own `image` 0.25 dependency; cargo unifies the
//! features.
//!
//! All functions block. Call them from a blocking thread.

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

use crate::{Error, Result};

/// Bounds a decode, so a hostile or broken file cannot exhaust memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Largest width or height accepted, in pixels.
    pub max_side: u32,
    /// Largest allocation the decoder may make, in bytes.
    pub max_alloc: u64,
}

impl Default for DecodeLimits {
    /// 16384 pixels per side and 128 MiB, what Proton Gallery's desktop used.
    fn default() -> Self {
        Self {
            max_side: 16_384,
            max_alloc: 128 * 1024 * 1024,
        }
    }
}

/// A decoded picture, upright and fitted.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// The pixels, with EXIF orientation applied, no larger than requested.
    pub image: DynamicImage,
    /// Width of the source as displayed (after orientation), before scaling.
    pub source_width: u32,
    /// Height of the source as displayed (after orientation), before scaling.
    pub source_height: u32,
}

/// Decode `bytes`, apply EXIF orientation and shrink to fit a `side` by `side`
/// box, keeping the aspect ratio. Never enlarges. `side == 0` keeps the full
/// size.
pub fn decode(bytes: &[u8], side: u32, limits: DecodeLimits) -> Result<Decoded> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Image(format!("unknown format: {e}")))?;
    let mut bounds = Limits::default();
    bounds.max_image_width = Some(limits.max_side);
    bounds.max_image_height = Some(limits.max_side);
    bounds.max_alloc = Some(limits.max_alloc);
    reader.limits(bounds.clone());
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    bounds.reserve(decoder.total_bytes()).map_err(image_error)?;
    decoder.set_limits(bounds).map_err(image_error)?;

    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (mut source_width, mut source_height) = decoder.dimensions();
    let image = DynamicImage::from_decoder(decoder).map_err(image_error)?;
    // Shrink before rotating: rotating the full-size image would cost a copy
    // of every source pixel. The box is square, so the order does not change
    // the result.
    let mut image = if side > 0 && (source_width > side || source_height > side) {
        image.thumbnail(side, side)
    } else {
        image
    };
    image.apply_orientation(orientation);
    if matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    ) {
        std::mem::swap(&mut source_width, &mut source_height);
    }
    Ok(Decoded {
        image,
        source_width,
        source_height,
    })
}

/// Encode for a cache: PNG when the picture has transparency, otherwise JPEG
/// at `jpeg_quality` (1 to 100).
pub fn encode(image: &DynamicImage, jpeg_quality: u8) -> Result<Vec<u8>> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    if image.color().has_alpha() {
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .map_err(image_error)?;
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, jpeg_quality.clamp(1, 100))
            .encode_image(&image.to_rgb8())
            .map_err(image_error)?;
    }
    Ok(bytes.into_inner())
}

/// A picture made ready for an [`crate::ImageCache`].
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The encoded picture, see [`encode`].
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The source's displayed shape, see [`Decoded`].
    pub source_width: u32,
    pub source_height: u32,
}

/// [`decode`] then [`encode`]: upright, at most `side` pixels on its longest
/// edge.
pub fn thumbnail(
    bytes: &[u8],
    side: u32,
    jpeg_quality: u8,
    limits: DecodeLimits,
) -> Result<Rendered> {
    let decoded = decode(bytes, side, limits)?;
    Ok(Rendered {
        bytes: encode(&decoded.image, jpeg_quality)?,
        width: decoded.image.width(),
        height: decoded.image.height(),
        source_width: decoded.source_width,
        source_height: decoded.source_height,
    })
}

fn image_error(e: image::ImageError) -> Error {
    Error::Image(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let image = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::new(width, height))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode");
        bytes.into_inner()
    }

    #[test]
    fn a_large_picture_is_fitted_and_keeps_its_source_shape() {
        let decoded = decode(&png(800, 400, false), 200, DecodeLimits::default()).expect("decode");
        assert_eq!((decoded.image.width(), decoded.image.height()), (200, 100));
        assert_eq!((decoded.source_width, decoded.source_height), (800, 400));
    }

    #[test]
    fn a_small_picture_is_never_enlarged() {
        let decoded = decode(&png(50, 30, false), 200, DecodeLimits::default()).expect("decode");
        assert_eq!((decoded.image.width(), decoded.image.height()), (50, 30));
    }

    #[test]
    fn opaque_pictures_become_jpeg_and_transparent_ones_png() {
        let opaque =
            thumbnail(&png(64, 64, false), 32, 85, DecodeLimits::default()).expect("opaque");
        assert_eq!(&opaque.bytes[..3], &[0xFF, 0xD8, 0xFF]);
        let clear = thumbnail(&png(64, 64, true), 32, 85, DecodeLimits::default()).expect("clear");
        assert_eq!(&clear.bytes[..4], b"\x89PNG");
        assert_eq!((clear.width, clear.height), (32, 32));
    }

    #[test]
    fn pictures_over_the_limits_are_refused() {
        let limits = DecodeLimits {
            max_side: 100,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            decode(&png(200, 10, false), 50, limits),
            Err(Error::Image(_))
        ));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode(b"not a picture", 50, DecodeLimits::default()).is_err());
    }
}
