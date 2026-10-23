//! Frame orientation for portal/PipeWire buffers.
//!
//! `SPA_META_VideoTransform` (and `wl_output.transform`) uses the Wayland
//! transform values: 90/180/270 degree counter-clockwise rotations, plus
//! flipped variants. Portal buffers are raw; the host rotates them before
//! encoding so the viewer sees the desktop upright.

use super::ScreenInfo;

/// Whether a raw PipeWire buffer has the orientation and size the portal
/// advertised for this monitor. Transform 90/270 swaps the buffer's axes.
pub fn frame_matches(screen: &ScreenInfo, raw: (u32, u32), transform: u32) -> bool {
    let swapped = matches!(transform, 1 | 3 | 5 | 7);
    let expected = if swapped {
        (screen.height, screen.width)
    } else {
        (screen.width, screen.height)
    };
    raw == expected
}

/// Rotate/flip a tightly packed BGRA image according to a
/// `SPA_META_VideoTransform`/`wl_output.transform` value.
pub fn orient_frame(
    src: &[u8],
    width: u32,
    height: u32,
    transform: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if transform > 7 || width == 0 || height == 0 {
        return None;
    }
    let swapped = matches!(transform, 1 | 3 | 5 | 7);
    let (out_width, out_height) = if swapped {
        (height, width)
    } else {
        (width, height)
    };
    if transform == 0 {
        return Some((src.to_vec(), out_width, out_height));
    }
    let expected = width as usize * height as usize * 4;
    if src.len() < expected {
        return None;
    }
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    for y in 0..out_height {
        for x in 0..out_width {
            let (sx, sy) = match transform {
                1 => (width - 1 - y, x),              // 90° counter-clockwise
                2 => (width - 1 - x, height - 1 - y), // 180°
                3 => (y, height - 1 - x),             // 90° clockwise
                4 => (width - 1 - x, y),              // horizontal flip
                5 => (y, x),                          // flip, then 90° CCW
                6 => (x, height - 1 - y),             // vertical flip
                7 => (width - 1 - y, height - 1 - x), // flip, then 270° CCW
                _ => return None,
            };
            let source = (sy as usize * width as usize + sx as usize) * 4;
            let target = (y as usize * out_width as usize + x as usize) * 4;
            out[target..target + 4].copy_from_slice(&src[source..source + 4]);
        }
    }
    Some((out, out_width, out_height))
}
