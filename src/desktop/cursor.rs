//! Cursor image helpers shared by the X11 backend and (later) the CONTROL
//! channel.
//!
//! XFixes returns premultiplied `0xAARRGGBB` pixels. The official client wants
//! straight RGBA in a PNG, so the channels are unpremultiplied once here.

use std::io::Write;

use flate2::Compression;

/// Convert one premultiplied ARGB pixel to straight RGBA.
#[inline]
pub fn argb_to_rgba(pixel: u32) -> [u8; 4] {
    let alpha = (pixel >> 24) as u8;
    let red = ((pixel >> 16) & 0xff) as u8;
    let green = ((pixel >> 8) & 0xff) as u8;
    let blue = (pixel & 0xff) as u8;
    if alpha == 0 {
        return [0, 0, 0, 0];
    }
    if alpha == 255 {
        return [red, green, blue, 255];
    }
    let unpremultiply = |channel: u8| {
        ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
    };
    [
        unpremultiply(red),
        unpremultiply(green),
        unpremultiply(blue),
        alpha,
    ]
}

/// Convert one premultiplied ARGB pixel to straight BGRA (frame order).
#[inline]
pub fn argb_to_bgra(pixel: u32) -> [u8; 4] {
    let [red, green, blue, alpha] = argb_to_rgba(pixel);
    [blue, green, red, alpha]
}

/// Decode a whole premultiplied ARGB image into tightly packed RGBA8.
pub fn rgba_from_argb(pixels: &[u32]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for pixel in pixels {
        rgba.extend_from_slice(&argb_to_rgba(*pixel));
    }
    rgba
}

/// Encode tightly packed RGBA8 as a PNG with filter 0 and zlib compression.
///
/// Cursor images are small, so the non-optimized encoder is enough; the
/// official client only needs a valid RGBA PNG.
pub fn png_rgba8(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let stride = width as usize * 4;
    if rgba.len() < stride * height as usize {
        return Vec::new();
    }
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgba.chunks_exact(stride) {
        raw.push(0); // filter type: None
        raw.extend_from_slice(row);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
    if encoder.write_all(&raw).is_err() {
        return Vec::new();
    }
    let Ok(compressed) = encoder.finish() else {
        return Vec::new();
    };

    let mut png = Vec::with_capacity(compressed.len() + 64);
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // 8-bit channels, colour type 6 (RGBA), deflate, filter 0, no interlace.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &header);
    push_chunk(&mut png, b"IDAT", &compressed);
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn opaque_pixels_keep_their_channels() {
        assert_eq!(argb_to_rgba(0xff12_3456), [0x12, 0x34, 0x56, 0xff]);
        assert_eq!(argb_to_bgra(0xff12_3456), [0x56, 0x34, 0x12, 0xff]);
    }

    #[test]
    fn premultiplied_colors_are_restored() {
        assert_eq!(argb_to_rgba(0x80_8000_00), [255, 0, 0, 0x80]);
        assert_eq!(argb_to_rgba(0), [0, 0, 0, 0]);
        // A malformed channel above alpha stays clamped.
        assert_eq!(argb_to_rgba(0x40_ff_ff_ff), [255, 255, 255, 0x40]);
    }

    #[test]
    fn rgba_from_argb_unpremultiplies_every_pixel() {
        assert_eq!(
            rgba_from_argb(&[0x80_8000_00, 0xff_12_34_56]),
            vec![255, 0, 0, 0x80, 0x12, 0x34, 0x56, 0xff]
        );
    }

    #[test]
    fn png_encodes_rgba_rows_with_valid_crcs() {
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 128, // row 0
            0, 0, 255, 0, 10, 20, 30, 40, // row 1
        ];
        let png = png_rgba8(2, 2, &rgba);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

        let mut offset = 8;
        let mut kinds = Vec::new();
        let mut idat = Vec::new();
        while offset + 12 <= png.len() {
            let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = png[offset + 4..offset + 8].to_vec();
            let data = &png[offset + 8..offset + 8 + length];
            let crc = u32::from_be_bytes(
                png[offset + 8 + length..offset + 12 + length]
                    .try_into()
                    .unwrap(),
            );
            assert_eq!(crc, crc32(&png[offset + 4..offset + 8 + length]));
            if kind == b"IDAT" {
                idat.extend_from_slice(data);
            }
            kinds.push(kind);
            offset += 12 + length;
        }
        assert_eq!(offset, png.len());
        assert_eq!(
            kinds,
            vec![b"IHDR".to_vec(), b"IDAT".to_vec(), b"IEND".to_vec()]
        );
        assert_eq!(&png[16..24], &[0, 0, 0, 2, 0, 0, 0, 2]);

        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(&idat[..])
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(
            raw,
            vec![
                0, 255, 0, 0, 255, 0, 255, 0, 128, // filter + row 0
                0, 0, 0, 255, 0, 10, 20, 30, 40, // filter + row 1
            ]
        );
    }

    #[test]
    fn degenerate_cursor_images_do_not_produce_pngs() {
        assert!(png_rgba8(0, 4, &[]).is_empty());
        assert!(png_rgba8(2, 2, &[0; 4]).is_empty());
    }
}
