//! Parâmetros de codecs e codificação RTP (H.264 e Opus) usados nas conexões WebRTC.
//! Define os payload types negociados e configura os parâmetros de SDP e RTCP feedback
//! compartilhados entre o Broadcaster e os Peers.

use ::rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use ::rtc::rtp_transceiver::PayloadType;
use ::rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters,
};

use crate::capture::audio::{CHANNELS, SAMPLE_RATE};

/// Payload types negociados (iguais em todos os peers, pois o MediaEngine é o mesmo).
pub const VIDEO_PAYLOAD_TYPE: PayloadType = 102;
pub const AUDIO_PAYLOAD_TYPE: PayloadType = 120;

/// Codec H.264 (Constrained Baseline, packetization-mode=1).
pub fn video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "goog-remb".to_owned(),
                    parameter: "".to_owned(),
                },
                RTCPFeedback {
                    typ: "ccm".to_owned(),
                    parameter: "fir".to_owned(),
                },
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "".to_owned(),
                },
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "pli".to_owned(),
                },
            ],
        },
        payload_type: VIDEO_PAYLOAD_TYPE,
        ..Default::default()
    }
}

/// Codec Opus estéreo a 48 kHz.
pub fn audio_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: SAMPLE_RATE,
            channels: CHANNELS as u16,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: AUDIO_PAYLOAD_TYPE,
        ..Default::default()
    }
}

pub fn encoding(ssrc: u32, codec: RTCRtpCodecParameters) -> RTCRtpEncodingParameters {
    RTCRtpEncodingParameters {
        rtp_coding_parameters: RTCRtpCodingParameters {
            ssrc: Some(ssrc),
            ..Default::default()
        },
        codec: codec.rtp_codec,
        ..Default::default()
    }
}
