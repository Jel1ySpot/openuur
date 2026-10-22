//! Cursor image helpers shared by the X11 backend and (later) the CONTROL
//! channel.
//!
//! XFixes returns premultiplied `0xAARRGGBB` pixels. The official client wants
//! straight RGBA in a PNG, so the channels are unpremultiplied once here.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
