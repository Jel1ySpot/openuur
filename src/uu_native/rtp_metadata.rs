//! UU private RTP header extensions used by the official streaming core.
//!
//! The official hosts mark every encoded access unit with a capture-source
//! index and a "new frame" flag. Both extensions use the one-byte RTP header
//! extension profile; the identifiers are negotiated through SDP `extmap`.
//! The capture index is a big-endian `u16`; the new-frame flag is one byte.

use std::borrow::Cow;

use webrtc::rtp::extension::HeaderExtension;
use webrtc::util::{Marshal, MarshalSize};

pub const CAPTURE_INDEX_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index";
pub const NEW_FRAME_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame";

#[derive(Debug, Clone, Copy)]
pub struct CaptureIndex(pub u16);

impl MarshalSize for CaptureIndex {
    fn marshal_size(&self) -> usize {
        2
    }
}

impl Marshal for CaptureIndex {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let target = buffer
            .get_mut(..2)
            .ok_or_else(|| webrtc::util::Error::Other("capture index buffer too short".into()))?;
        target.copy_from_slice(&self.0.to_be_bytes());
        Ok(2)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NewFrame(pub bool);

impl MarshalSize for NewFrame {
    fn marshal_size(&self) -> usize {
        1
    }
}

impl Marshal for NewFrame {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let byte = buffer
            .first_mut()
            .ok_or_else(|| webrtc::util::Error::Other("new-frame buffer too short".into()))?;
        *byte = u8::from(self.0);
        Ok(1)
    }
}

pub fn capture_index(index: u16) -> HeaderExtension {
    HeaderExtension::Custom {
        uri: Cow::Borrowed(CAPTURE_INDEX_URI),
        extension: Box::new(CaptureIndex(index)),
    }
}

pub fn new_frame() -> HeaderExtension {
    HeaderExtension::Custom {
        uri: Cow::Borrowed(NEW_FRAME_URI),
        extension: Box::new(NewFrame(true)),
    }
}

/// Extensions attached to every desktop/synthetic video sample.
pub fn video_extensions(index: u16) -> [HeaderExtension; 2] {
    [capture_index(index), new_frame()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_index_is_big_endian_u16() {
        let mut bytes = [0u8; 2];
        assert_eq!(CaptureIndex(0x1234).marshal_to(&mut bytes).unwrap(), 2);
        assert_eq!(bytes, [0x12, 0x34]);
        assert!(CaptureIndex(1).marshal_to(&mut [0u8; 1]).is_err());
    }

    #[test]
    fn new_frame_is_single_byte_boolean() {
        let mut byte = [0u8];
        NewFrame(true).marshal_to(&mut byte).unwrap();
        assert_eq!(byte, [1]);
        NewFrame(false).marshal_to(&mut byte).unwrap();
        assert_eq!(byte, [0]);
        assert!(NewFrame(true).marshal_to(&mut []).is_err());
    }

    #[test]
    fn custom_extensions_keep_the_official_uris() {
        assert_eq!(capture_index(0).uri(), CAPTURE_INDEX_URI);
        assert_eq!(new_frame().uri(), NEW_FRAME_URI);
    }
}
