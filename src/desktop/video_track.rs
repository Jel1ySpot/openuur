//! One video m-line with H.264 and H.265 payload types.
//!
//! A single RTP sequence/timestamp timeline survives codec changes. The
//! official viewer may switch codecs during a session; keeping one SSRC avoids
//! two independent timestamp bases.

use async_trait::async_trait;
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use webrtc::error::{Error, Result};
use webrtc::media::Sample;
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Payloader;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalContext};

use super::hevc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn mime(self) -> &'static str {
        match self {
            Self::H264 => "video/H264",
            Self::H265 => "video/H265",
        }
    }

    pub fn wire(self) -> u64 {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
        }
    }
}

pub fn register_hevc(media: &mut webrtc::api::media_engine::MediaEngine) -> Result<()> {
    media.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: Codec::H265.mime().to_owned(),
                clock_rate: 90_000,
                rtcp_feedback: vec![
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: String::new(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                ],
                ..Default::default()
            },
            payload_type: 126,
            ..Default::default()
        },
        RTPCodecType::Video,
    )
}

struct Timeline {
    sequence: u16,
    timestamp: u32,
    h264: H264Payloader,
}

pub struct VideoTrack {
    h264: TrackLocalStaticRTP,
    h265: TrackLocalStaticRTP,
    has_h264: AtomicBool,
    has_h265: AtomicBool,
    timeline: Mutex<Timeline>,
}

impl Default for VideoTrack {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for VideoTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoTrack")
            .field("has_h264", &self.has_h264)
            .field("has_h265", &self.has_h265)
            .finish()
    }
}

impl VideoTrack {
    pub fn new() -> Self {
        let track = |codec: Codec| {
            TrackLocalStaticRTP::new(
                RTCRtpCodecCapability {
                    mime_type: codec.mime().to_owned(),
                    clock_rate: 90_000,
                    ..Default::default()
                },
                "video_0".to_owned(),
                "video_0".to_owned(),
            )
        };
        Self {
            h264: track(Codec::H264),
            h265: track(Codec::H265),
            has_h264: AtomicBool::new(false),
            has_h265: AtomicBool::new(false),
            timeline: Mutex::new(Timeline {
                sequence: rand::random(),
                timestamp: rand::random(),
                h264: H264Payloader::default(),
            }),
        }
    }

    pub fn supports(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => &self.has_h264,
            Codec::H265 => &self.has_h265,
        }
        .load(Ordering::Acquire)
    }

    pub fn available(&self) -> (bool, bool) {
        (
            self.has_h264.load(Ordering::Acquire),
            self.has_h265.load(Ordering::Acquire),
        )
    }

    pub async fn write_sample(&self, codec: Codec, sample: &Sample) -> Result<()> {
        self.write_sample_with_extensions(codec, sample, &[]).await
    }

    /// Write one encoded access unit, attaching negotiation-mapped RTP header
    /// extensions (UU capture index / new-frame flags).
    pub async fn write_sample_with_extensions(
        &self,
        codec: Codec,
        sample: &Sample,
        extensions: &[webrtc::rtp::extension::HeaderExtension],
    ) -> Result<()> {
        if !self.supports(codec) {
            return Err(Error::ErrUnsupportedCodec);
        }
        let track = match codec {
            Codec::H264 => &self.h264,
            Codec::H265 => &self.h265,
        };
        if track.all_binding_paused().await {
            return Ok(());
        }
        let packets = {
            let mut timeline = self
                .timeline
                .lock()
                .map_err(|_| Error::new("video timeline poisoned".to_owned()))?;
            let payloads = match codec {
                Codec::H264 => timeline
                    .h264
                    .payload(1160, &sample.data)
                    .map_err(Error::Rtp)?,
                Codec::H265 => hevc::payload(1160, &sample.data).map_err(Error::new)?,
            };
            let count = payloads.len();
            let timestamp = timeline.timestamp;
            timeline.timestamp = timeline
                .timestamp
                .wrapping_add((sample.duration.as_secs_f64() * 90_000.0) as u32);
            let mut packets = Vec::with_capacity(count);
            for (index, payload) in payloads.into_iter().enumerate() {
                packets.push(Packet {
                    header: Header {
                        version: 2,
                        sequence_number: timeline.sequence,
                        timestamp,
                        marker: index + 1 == count,
                        ..Default::default()
                    },
                    payload,
                });
                timeline.sequence = timeline.sequence.wrapping_add(1);
            }
            packets
        };
        for packet in packets {
            track.write_rtp_with_extensions(&packet, extensions).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl TrackLocal for VideoTrack {
    async fn bind(&self, context: &TrackLocalContext) -> Result<RTCRtpCodecParameters> {
        let h264 = self.h264.bind(context).await.ok();
        let h265 = self.h265.bind(context).await.ok();
        self.has_h264.store(h264.is_some(), Ordering::Release);
        self.has_h265.store(h265.is_some(), Ordering::Release);
        tracing::info!(
            "desktop video track negotiated: h264={} h265={}",
            h264.is_some(),
            h265.is_some()
        );
        h264.or(h265).ok_or(Error::ErrUnsupportedCodec)
    }

    async fn unbind(&self, context: &TrackLocalContext) -> Result<()> {
        self.has_h264.store(false, Ordering::Release);
        self.has_h265.store(false, Ordering::Release);
        let _ = self.h264.unbind(context).await;
        let _ = self.h265.unbind(context).await;
        Ok(())
    }

    fn id(&self) -> &str {
        "video_0"
    }

    fn stream_id(&self) -> &str {
        "video_0"
    }

    fn kind(&self) -> RTPCodecType {
        RTPCodecType::Video
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
