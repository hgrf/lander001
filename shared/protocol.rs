use crc32fast::Hasher;
use prost::Message;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/rustyface.rs"));
}

const FRAME_MAGIC: [u8; 2] = *b"RF";
const FRAME_VERSION: u8 = 1;
const FRAME_HEADER_LEN: usize = 2 + 1 + 2 + 4;
const MAX_PAYLOAD_LEN: usize = 4096;

pub const BLE_DEVICE_NAME: &str = "lander001";

#[allow(dead_code)]
#[derive(Debug)]
pub enum FrameError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u8),
    LengthMismatch,
    CrcMismatch,
    PayloadTooLarge,
    Decode(prost::DecodeError),
    SelfCheckFailed,
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort => write!(f, "frame too short"),
            Self::BadMagic => write!(f, "invalid frame magic"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported frame version: {v}"),
            Self::LengthMismatch => write!(f, "frame length mismatch"),
            Self::CrcMismatch => write!(f, "frame checksum mismatch"),
            Self::PayloadTooLarge => write!(f, "payload exceeds maximum allowed size"),
            Self::Decode(err) => write!(f, "protobuf decode error: {err}"),
            Self::SelfCheckFailed => write!(f, "startup protocol self-check failed"),
        }
    }
}

impl std::error::Error for FrameError {}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct BehaviorProfile {
    pub led_pattern_id: u32,
    pub led_repeats: u32,
    pub servo_target_deg: f32,
}

#[allow(dead_code)]
pub fn behavior_for_event(event: &pb::NotificationEvent) -> BehaviorProfile {
    use pb::Urgency;

    let urgency = Urgency::try_from(event.urgency).unwrap_or(Urgency::Normal);
    let source = format!(
        "{} {} {}",
        event.source_app, event.source_bundle_id, event.app_icon_hint
    )
    .to_ascii_lowercase();

    let mut profile = match urgency {
        Urgency::Low => BehaviorProfile {
            led_pattern_id: 1,
            led_repeats: 1,
            servo_target_deg: 60.0,
        },
        Urgency::Normal => BehaviorProfile {
            led_pattern_id: 2,
            led_repeats: 2,
            servo_target_deg: 90.0,
        },
        Urgency::High => BehaviorProfile {
            led_pattern_id: 3,
            led_repeats: 3,
            servo_target_deg: 150.0,
        },
        Urgency::Critical => BehaviorProfile {
            led_pattern_id: 4,
            led_repeats: 4,
            servo_target_deg: 210.0,
        },
    };

    if source.contains("whatsapp") || source.contains("discord") || source.contains("slack") {
        profile.led_pattern_id = 2;
        profile.servo_target_deg = 135.0;
    } else if source.contains("mail") || source.contains("outlook") {
        profile.led_pattern_id = 1;
        profile.servo_target_deg = 75.0;
    } else if source.contains("calendar") {
        profile.led_pattern_id = 3;
        profile.servo_target_deg = 180.0;
    }

    profile
}

pub fn encode_frame(msg: &pb::WireMessage) -> Result<Vec<u8>, FrameError> {
    let mut payload = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut payload)
        .expect("protobuf encode to vec should not fail");

    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge);
    }

    let mut hasher = Hasher::new();
    hasher.update(&payload);
    let crc = hasher.finalize();

    let len = payload.len() as u16;
    let mut frame = Vec::with_capacity(9 + payload.len());
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.push(FRAME_VERSION);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame(frame: &[u8]) -> Result<pb::WireMessage, FrameError> {
    if frame.len() < FRAME_HEADER_LEN {
        return Err(FrameError::TooShort);
    }

    if frame[0..2] != FRAME_MAGIC {
        return Err(FrameError::BadMagic);
    }

    let version = frame[2];
    if version != FRAME_VERSION {
        return Err(FrameError::UnsupportedVersion(version));
    }

    let payload_len = u16::from_le_bytes([frame[3], frame[4]]) as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge);
    }
    if frame.len() != FRAME_HEADER_LEN + payload_len {
        return Err(FrameError::LengthMismatch);
    }

    let expected_crc = u32::from_le_bytes([frame[5], frame[6], frame[7], frame[8]]);
    let payload = &frame[FRAME_HEADER_LEN..];

    let mut hasher = Hasher::new();
    hasher.update(payload);
    let actual_crc = hasher.finalize();
    if actual_crc != expected_crc {
        return Err(FrameError::CrcMismatch);
    }

    pb::WireMessage::decode(payload).map_err(FrameError::Decode)
}

pub struct StreamDecoder {
    buf: Vec<u8>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next_message(&mut self) -> Option<Result<pb::WireMessage, FrameError>> {
        loop {
            if self.buf.len() < FRAME_HEADER_LEN {
                return None;
            }

            if self.buf[0] != FRAME_MAGIC[0] || self.buf[1] != FRAME_MAGIC[1] {
                if let Some(pos) = self
                    .buf
                    .windows(2)
                    .position(|w| w[0] == FRAME_MAGIC[0] && w[1] == FRAME_MAGIC[1])
                {
                    self.buf.drain(0..pos);
                } else {
                    self.buf.clear();
                }
                continue;
            }

            let payload_len = u16::from_le_bytes([self.buf[3], self.buf[4]]) as usize;
            if payload_len > MAX_PAYLOAD_LEN {
                self.buf.drain(0..2);
                return Some(Err(FrameError::LengthMismatch));
            }

            let frame_len = FRAME_HEADER_LEN + payload_len;
            if self.buf.len() < frame_len {
                return None;
            }

            let frame = self.buf[0..frame_len].to_vec();
            self.buf.drain(0..frame_len);
            return Some(decode_frame(&frame));
        }
    }
}

#[allow(dead_code)]
pub fn startup_protocol_self_check() -> Result<(), FrameError> {
    let ping = pb::Ping { unix_ms: 0 };
    let msg = pb::WireMessage {
        msg_id: 1,
        payload: Some(pb::wire_message::Payload::Ping(ping)),
    };

    let frame = encode_frame(&msg)?;
    let parsed = decode_frame(&frame)?;
    if parsed.msg_id != msg.msg_id {
        return Err(FrameError::SelfCheckFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_works() {
        let msg = pb::WireMessage {
            msg_id: 42,
            payload: Some(pb::wire_message::Payload::SetServo(pb::SetServo {
                angle_deg: 135.0,
            })),
        };

        let frame = encode_frame(&msg).expect("encode should succeed");
        let parsed = decode_frame(&frame).expect("frame should decode");
        assert_eq!(parsed.msg_id, 42);
    }

    #[test]
    fn frame_crc_guard_rejects_corruption() {
        let msg = pb::WireMessage {
            msg_id: 11,
            payload: Some(pb::wire_message::Payload::Ping(pb::Ping { unix_ms: 123 })),
        };

        let mut frame = encode_frame(&msg).expect("encode should succeed");
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;

        let err = decode_frame(&frame).expect_err("corrupted frame must fail");
        assert!(matches!(err, FrameError::CrcMismatch));
    }
}
