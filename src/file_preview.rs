//! Internal decoder subprocess. No paths, sockets, config, or server startup.
use crate::api::schema::PreviewRaster;
use base64::Engine;
use std::io::{self, Read, Write};

pub(crate) const IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_EDGE: u32 = 8192;
pub(crate) const MAX_PIXELS: u64 = 16 * 1024 * 1024;
pub(crate) const THUMB_EDGE: u32 = 160;

pub(crate) fn maybe_run(args: &[String]) -> Option<io::Result<()>> {
    if args.get(1).map(String::as_str) != Some("--internal-file-preview") {
        return None;
    }
    Some(run(&args[2..]))
}

fn run(args: &[String]) -> io::Result<()> {
    if args == ["--help"] {
        eprintln!("Internal bounded image decoder. Usage: herdr --internal-file-preview --width N --height N < image\nExample: herdr --internal-file-preview --width 40 --height 80 < fixture.png\nNo files or server are opened; stdout is one JSON raster.");
        return Ok(());
    }
    if args.len() != 4 || args[0] != "--width" || args[2] != "--height" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected --width N --height N; use --internal-file-preview --help",
        ));
    }
    let width = args[1].parse::<u32>().map_err(io::Error::other)?;
    let height = args[3].parse::<u32>().map_err(io::Error::other)?;
    validate_size(width, height)?;
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take((IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > IMAGE_BYTES {
        return Err(io::Error::other("too_large: encoded image exceeds 16 MiB"));
    }
    let raster = decode(&bytes, width, height)?;
    serde_json::to_writer(io::stdout().lock(), &raster).map_err(io::Error::other)?;
    io::stdout().lock().write_all(b"\n")
}

pub(crate) fn validate_size(width: u32, height: u32) -> io::Result<()> {
    if width == 0 || height == 0 || width > THUMB_EDGE || height > THUMB_EDGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "thumbnail dimensions must be 1..=160",
        ));
    }
    Ok(())
}

fn reader(bytes: &[u8]) -> io::Result<image::ImageReader<std::io::Cursor<&[u8]>>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    if !matches!(
        reader.format(),
        Some(
            image::ImageFormat::Png
                | image::ImageFormat::Jpeg
                | image::ImageFormat::Gif
                | image::ImageFormat::WebP
                | image::ImageFormat::Bmp
        )
    ) {
        return Err(io::Error::other("unsupported: image format"));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader)
}

pub(crate) fn decode(bytes: &[u8], width: u32, height: u32) -> io::Result<PreviewRaster> {
    validate_size(width, height)?;
    let (original_width, original_height) =
        reader(bytes)?.into_dimensions().map_err(io::Error::other)?;
    if original_width == 0
        || original_height == 0
        || original_width > MAX_EDGE
        || original_height > MAX_EDGE
        || u64::from(original_width) * u64::from(original_height) > MAX_PIXELS
    {
        return Err(io::Error::other(
            "too_large: source image dimensions exceed preview budget",
        ));
    }
    // First composited frame only. The helper's parent enforces a hard deadline;
    // decoder allocation limits are best effort, not a memory sandbox.
    let image = reader(bytes)?
        .decode()
        .map_err(io::Error::other)?
        .thumbnail(width, height)
        .to_rgba8();
    let mut rgb = Vec::with_capacity(image.width() as usize * image.height() as usize * 3);
    for pixel in image.pixels() {
        let alpha = u32::from(pixel[3]);
        for component in &pixel.0[..3] {
            rgb.push(((u32::from(*component) * alpha + 32 * (255 - alpha)) / 255) as u8);
        }
    }
    Ok(PreviewRaster {
        width: image.width(),
        height: image.height(),
        original_width,
        original_height,
        rgb_base64: base64::engine::general_purpose::STANDARD.encode(rgb),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoder_supports_common_formats_and_preserves_aspect_ratio() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::WebP,
            image::ImageFormat::Bmp,
        ] {
            let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                16,
                8,
                image::Rgb([200, 50, 30]),
            ));
            let mut encoded = std::io::Cursor::new(Vec::new());
            image.write_to(&mut encoded, format).unwrap();
            let raster = decode(encoded.get_ref(), 8, 8).unwrap();
            assert_eq!((raster.width, raster.height), (8, 4));
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(raster.rgb_base64)
                    .unwrap()
                    .len(),
                8 * 4 * 3
            );
        }
        assert!(decode(b"bad image", 40, 80).is_err());
        assert!(validate_size(0, 80).is_err());
        assert!(validate_size(161, 80).is_err());
    }
}
