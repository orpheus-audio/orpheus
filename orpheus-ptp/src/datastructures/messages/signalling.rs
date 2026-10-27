use super::MessageType;
use crate::{
    datastructures::{
        common::{PortIdentity, Tlv, TlvSetBuilder, TlvType},
        WireFormat, WireFormatError,
    },
    time::Interval,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignalingMessage {
    pub(crate) target_port_identity: PortIdentity,
}

impl SignalingMessage {
    pub(crate) fn content_size(&self) -> usize {
        10
    }

    pub(crate) fn serialize_content(&self, buffer: &mut [u8]) -> Result<(), WireFormatError> {
        if buffer.len() < 10 {
            return Err(WireFormatError::BufferTooShort);
        }

        let (left, _) = buffer.split_at_mut(10);

        self.target_port_identity.serialize(left)?;

        Ok(())
    }

    pub(crate) fn deserialize_content(buffer: &[u8]) -> Result<Self, WireFormatError> {
        let identity_bytes = buffer.get(0..10).ok_or(WireFormatError::BufferTooShort)?;
        let target_port_identity = PortIdentity::deserialize(identity_bytes)?;

        Ok(Self {
            target_port_identity,
        })
    }
}

/// The TLVs of unicast negotiation (IEEE 1588-2019 16.1.4), carried in
/// signaling messages. `message_type` sits in the upper nibble of the first
/// octet of the value, as linuxptp sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnicastTlv {
    /// Asks for `message_type` every `interval` for `duration_s` seconds.
    Request {
        message_type: MessageType,
        interval: Interval,
        duration_s: u32,
    },
    /// Grants a request; a `duration_s` of 0 refuses it.
    Grant {
        message_type: MessageType,
        interval: Interval,
        duration_s: u32,
        renewal_invited: bool,
    },
    Cancel {
        message_type: MessageType,
        maintain_request: bool,
        maintain_grant: bool,
    },
    AcknowledgeCancel {
        message_type: MessageType,
        maintain_request: bool,
        maintain_grant: bool,
    },
}

impl UnicastTlv {
    const MAX_VALUE_LEN: usize = 8;

    /// Reads a unicast negotiation TLV; `None` for other TLVs and malformed
    /// ones.
    pub(crate) fn parse(tlv: &Tlv<'_>) -> Option<Self> {
        let value: &[u8] = tlv.value.as_ref();
        let message_type = MessageType::try_from(*value.first()? >> 4).ok()?;
        let flags = |at: usize| value.get(at).copied();
        let parsed = match tlv.tlv_type {
            TlvType::RequestUnicastTransmission if value.len() >= 6 => Self::Request {
                message_type,
                interval: Interval::from_log_2(value[1] as i8),
                duration_s: u32::from_be_bytes(value[2..6].try_into().unwrap()),
            },
            TlvType::GrantUnicastTransmission if value.len() >= 8 => Self::Grant {
                message_type,
                interval: Interval::from_log_2(value[1] as i8),
                duration_s: u32::from_be_bytes(value[2..6].try_into().unwrap()),
                renewal_invited: flags(7)? & 1 != 0,
            },
            TlvType::CancelUnicastTransmission if value.len() >= 2 => Self::Cancel {
                message_type,
                maintain_request: value[0] & 1 != 0,
                maintain_grant: value[0] & 2 != 0,
            },
            TlvType::AcknowledgeCancelUnicastTransmission if value.len() >= 2 => {
                Self::AcknowledgeCancel {
                    message_type,
                    maintain_request: value[0] & 1 != 0,
                    maintain_grant: value[0] & 2 != 0,
                }
            }
            _ => return None,
        };
        Some(parsed)
    }

    /// Writes the TLV into `builder`.
    pub(crate) fn add_to(&self, builder: &mut TlvSetBuilder<'_>) -> Result<(), WireFormatError> {
        let mut value = [0u8; Self::MAX_VALUE_LEN];
        let (tlv_type, len) = match *self {
            Self::Request {
                message_type,
                interval,
                duration_s,
            } => {
                value[0] = (message_type as u8) << 4;
                value[1] = interval.as_log_2() as u8;
                value[2..6].copy_from_slice(&duration_s.to_be_bytes());
                (TlvType::RequestUnicastTransmission, 6)
            }
            Self::Grant {
                message_type,
                interval,
                duration_s,
                renewal_invited,
            } => {
                value[0] = (message_type as u8) << 4;
                value[1] = interval.as_log_2() as u8;
                value[2..6].copy_from_slice(&duration_s.to_be_bytes());
                value[7] = renewal_invited as u8;
                (TlvType::GrantUnicastTransmission, 8)
            }
            Self::Cancel {
                message_type,
                maintain_request,
                maintain_grant,
            } => {
                value[0] = (message_type as u8) << 4
                    | maintain_request as u8
                    | (maintain_grant as u8) << 1;
                (TlvType::CancelUnicastTransmission, 2)
            }
            Self::AcknowledgeCancel {
                message_type,
                maintain_request,
                maintain_grant,
            } => {
                value[0] = (message_type as u8) << 4
                    | maintain_request as u8
                    | (maintain_grant as u8) << 1;
                (TlvType::AcknowledgeCancelUnicastTransmission, 2)
            }
        };
        builder.add(Tlv {
            tlv_type,
            value: value[..len].into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;
    use crate::datastructures::{
        common::{ClockIdentity, TlvSet},
        messages::{Header, Message, MessageBody},
    };

    #[test]
    fn unicast_tlv_wireformat() {
        let representations: [(&[u8], UnicastTlv); 4] = [
            (
                &[0x00, 0x04, 0x00, 0x06, 0x00, 0xfd, 0x00, 0x00, 0x00, 0x3c],
                UnicastTlv::Request {
                    message_type: MessageType::Sync,
                    interval: Interval::from_log_2(-3),
                    duration_s: 60,
                },
            ),
            (
                &[
                    0x00, 0x05, 0x00, 0x08, 0x90, 0x00, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x01,
                ],
                UnicastTlv::Grant {
                    message_type: MessageType::DelayResp,
                    interval: Interval::from_log_2(0),
                    duration_s: 60,
                    renewal_invited: true,
                },
            ),
            (
                &[0x00, 0x06, 0x00, 0x02, 0xb2, 0x00],
                UnicastTlv::Cancel {
                    message_type: MessageType::Announce,
                    maintain_request: false,
                    maintain_grant: true,
                },
            ),
            (
                &[0x00, 0x07, 0x00, 0x02, 0x01, 0x00],
                UnicastTlv::AcknowledgeCancel {
                    message_type: MessageType::Sync,
                    maintain_request: true,
                    maintain_grant: false,
                },
            ),
        ];

        for (bytes, tlv) in representations {
            let mut buffer = [0; 32];
            let mut builder = TlvSetBuilder::new(&mut buffer);
            tlv.add_to(&mut builder).unwrap();
            let set = builder.build();
            let mut wire = [0; 32];
            let n = set.serialize(&mut wire).unwrap();
            assert_eq!(&wire[..n], bytes);

            let set = TlvSet::deserialize(bytes).unwrap();
            let parsed: Vec<_> = set.tlv().filter_map(|t| UnicastTlv::parse(&t)).collect();
            assert_eq!(parsed, [tlv]);
        }
    }

    #[test]
    fn unicast_tlv_rejects_malformed() {
        let parse = |tlv_type, value: &[u8]| {
            UnicastTlv::parse(&Tlv {
                tlv_type,
                value: value.into(),
            })
        };
        // Too short.
        assert_eq!(
            parse(
                TlvType::GrantUnicastTransmission,
                &[0x00, 0xfd, 0, 0, 0, 60]
            ),
            None
        );
        // No such message type.
        assert_eq!(
            parse(TlvType::RequestUnicastTransmission, &[0x40, 0, 0, 0, 0, 60]),
            None
        );
        // Not a negotiation TLV.
        assert_eq!(parse(TlvType::PathTrace, &[0x00, 0, 0, 0, 0, 60]), None);
    }

    #[test]
    fn signaling_message_roundtrip() {
        let mut tlv_buffer = [0; 64];
        let mut builder = TlvSetBuilder::new(&mut tlv_buffer);
        let requests = [
            UnicastTlv::Request {
                message_type: MessageType::Sync,
                interval: Interval::from_log_2(-3),
                duration_s: 60,
            },
            UnicastTlv::Request {
                message_type: MessageType::DelayResp,
                interval: Interval::from_log_2(0),
                duration_s: 60,
            },
        ];
        for r in &requests {
            r.add_to(&mut builder).unwrap();
        }
        let source = PortIdentity {
            clock_identity: ClockIdentity([1, 2, 3, 4, 5, 6, 7, 8]),
            port_number: 1,
        };
        let message = Message::signaling(
            Header {
                source_port_identity: source,
                sequence_id: 7,
                ..Header::new(1)
            },
            PortIdentity::WILDCARD,
            builder.build(),
        );

        let mut wire = [0; 128];
        let n = message.serialize(&mut wire).unwrap();
        // Header, target port identity, two TLVs of 10 bytes each.
        assert_eq!(n, 34 + 10 + 20);
        assert_eq!(wire[0] & 0x0f, 0x0c);
        assert_eq!(wire[6] & 0x04, 0x04, "unicast flag");
        assert_eq!(wire[32], 0x05, "control field");
        assert_eq!(wire[33], 0x7f, "log message interval");
        assert_eq!(&wire[34..44], &[0xff; 10]);

        let parsed = Message::deserialize(&wire[..n]).unwrap();
        assert_eq!(parsed.header().source_port_identity, source);
        assert!(matches!(
            parsed.body,
            MessageBody::Signaling(SignalingMessage {
                target_port_identity: PortIdentity::WILDCARD
            })
        ));
        let tlvs: Vec<_> = parsed.unicast_tlvs().collect();
        assert_eq!(tlvs, requests);
    }
}
