/// ZCL (Zigbee Cluster Library) frame header parsing.
///
/// ZCL frame structure:
///   Frame Control  [1 byte]
///   Manufacturer Code [0 or 2 bytes]
///   Sequence Number [1 byte]
///   Command ID      [1 byte]
///   Payload         [variable]
///
/// Frame Control bits:
///   [1:0] Frame type: 0=global, 1=cluster-specific
///   [2]   Manufacturer specific
///   [3]   Direction: 0=client→server, 1=server→client
///   [4]   Disable default response
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Global = 0,
    ClusterSpecific = 1,
}

#[derive(Debug, Clone)]
pub struct ZclFrameHeader {
    pub frame_type: FrameType,
    pub command_id: u8,
    /// Manufacturer code, present when the frame control's manufacturer-
    /// specific bit is set (e.g. IKEA TRADFRI remotes' extra Scenes commands
    /// -- see `clusters::scenes`).
    pub mfr_code: Option<u16>,
    /// Direction bit: set when the frame was sent by a cluster *server* to a
    /// client (e.g. attribute reports), clear for client→server commands.
    pub server_to_client: bool,
    /// The sender doesn't want a ZCL Default Response.
    pub disable_default_response: bool,
    /// ZCL transaction sequence number.
    pub sequence: u8,
}

impl ZclFrameHeader {
    pub fn parse(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.is_empty() {
            return Err(Error::Zcl("empty ZCL frame".into()));
        }
        let fc = buf[0];
        let frame_type = if (fc & 0x03) == 1 {
            FrameType::ClusterSpecific
        } else {
            FrameType::Global
        };
        let mfr_specific = (fc & 0x04) != 0;
        let server_to_client = (fc & 0x08) != 0;
        let disable_default_response = (fc & 0x10) != 0;

        let mut pos = 1usize;
        let mfr_code = if mfr_specific {
            if buf.len() < pos + 2 {
                return Err(Error::Zcl("truncated manufacturer code".into()));
            }
            let code = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            pos += 2;
            Some(code)
        } else {
            None
        };

        if buf.len() < pos + 2 {
            return Err(Error::Zcl(
                "ZCL frame too short for sequence+command".into(),
            ));
        }
        let sequence = buf[pos];
        pos += 1;
        let command_id = buf[pos];
        pos += 1;

        Ok((
            ZclFrameHeader {
                frame_type,
                command_id,
                mfr_code,
                server_to_client,
                disable_default_response,
                sequence,
            },
            pos,
        ))
    }

    fn encode_global(sequence: u8, command_id: u8) -> Vec<u8> {
        // Client→server, disable default response (the device still sends
        // one if the command fails).
        vec![0x10, sequence, command_id]
    }
}

pub mod global {
    pub const READ_ATTRIBUTES: u8 = 0x00;
    pub const READ_ATTRIBUTES_RSP: u8 = 0x01;
    pub const WRITE_ATTRIBUTES: u8 = 0x02;
    pub const WRITE_ATTRIBUTES_UNDIVIDED: u8 = 0x03;
    pub const WRITE_ATTRIBUTES_RSP: u8 = 0x04;
    pub const WRITE_ATTRIBUTES_NO_RSP: u8 = 0x05;
    pub const CONFIGURE_REPORTING: u8 = 0x06;
    pub const CONFIGURE_REPORTING_RSP: u8 = 0x07;
    pub const READ_REPORTING_CONFIG: u8 = 0x08;
    pub const REPORT_ATTRIBUTES: u8 = 0x0A;
    pub const DEFAULT_RESPONSE: u8 = 0x0B;
    pub const DISCOVER_ATTRIBUTES: u8 = 0x0C;
    pub const DISCOVER_COMMANDS_RECEIVED: u8 = 0x11;
    pub const DISCOVER_COMMANDS_GENERATED: u8 = 0x13;
    pub const DISCOVER_ATTRIBUTES_EXTENDED: u8 = 0x15;
}

/// The ZCL Default Response we owe the sender of `header`, if any (ZCL spec
/// 2.5.12, as zigbee-herdsman's `Device.onZclData` applies it): only for
/// frames that didn't set Disable Default Response, aren't themselves a
/// Default Response, and aren't requests whose own specific response is what
/// the sender expects (we answer none of those -- a Default Response would
/// claim success). Callers must also skip broadcasts. The response mirrors
/// the request: opposite direction, same transaction sequence number and
/// manufacturer code, with status SUCCESS.
pub fn default_response_for(header: &ZclFrameHeader) -> Option<Vec<u8>> {
    if header.disable_default_response {
        return None;
    }
    if header.frame_type == FrameType::Global
        && matches!(
            header.command_id,
            global::DEFAULT_RESPONSE
                | global::READ_ATTRIBUTES
                | global::WRITE_ATTRIBUTES
                | global::WRITE_ATTRIBUTES_UNDIVIDED
                | global::WRITE_ATTRIBUTES_NO_RSP
                | global::CONFIGURE_REPORTING
                | global::READ_REPORTING_CONFIG
                | global::DISCOVER_ATTRIBUTES
                | global::DISCOVER_COMMANDS_RECEIVED
                | global::DISCOVER_COMMANDS_GENERATED
                | global::DISCOVER_ATTRIBUTES_EXTENDED
        )
    {
        return None;
    }
    // Global frame, disable default response, direction flipped.
    let mut fc = 0x10u8;
    if !header.server_to_client {
        fc |= 0x08;
    }
    let mut payload = Vec::with_capacity(7);
    if let Some(code) = header.mfr_code {
        payload.push(fc | 0x04);
        payload.extend_from_slice(&code.to_le_bytes());
    } else {
        payload.push(fc);
    }
    payload.push(header.sequence);
    payload.push(global::DEFAULT_RESPONSE);
    payload.push(header.command_id);
    payload.push(0x00); // status: SUCCESS
    Some(payload)
}

/// One attribute reporting configuration for `configure_reporting_payload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportingConfig {
    pub attr_id: u16,
    pub data_type: u8,
    /// Minimum/maximum reporting interval in seconds.
    pub min_interval: u16,
    pub max_interval: u16,
    /// Reportable change, little-endian, sized for `data_type`. `None` for
    /// discrete types (boolean, bitmap, enum), which carry no such field.
    pub reportable_change: Option<u32>,
}

/// Build a ZCL Configure Reporting request (client→server, attributes
/// reported by the device to us).
pub fn configure_reporting_payload(sequence: u8, configs: &[ReportingConfig]) -> Vec<u8> {
    let mut payload = ZclFrameHeader::encode_global(sequence, global::CONFIGURE_REPORTING);
    for c in configs {
        payload.push(0x00); // direction: reported by the server (the device)
        payload.extend_from_slice(&c.attr_id.to_le_bytes());
        payload.push(c.data_type);
        payload.extend_from_slice(&c.min_interval.to_le_bytes());
        payload.extend_from_slice(&c.max_interval.to_le_bytes());
        if let Some(change) = c.reportable_change {
            let len = super::attribute::DataType::from_u8(c.data_type)
                .fixed_len()
                .unwrap_or(0)
                .min(4);
            payload.extend_from_slice(&change.to_le_bytes()[..len]);
        }
    }
    payload
}

/// Build a ZCL Read Attributes request payload.
pub fn read_attributes_payload(sequence: u8, attr_ids: &[u16]) -> Vec<u8> {
    let mut payload = ZclFrameHeader::encode_global(sequence, global::READ_ATTRIBUTES);
    for &id in attr_ids {
        payload.extend_from_slice(&id.to_le_bytes());
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frame_without_mfr_code() {
        let raw = [0x01, 0x01, 0x02]; // cluster-specific, seq=1, cmd=Toggle
        let (header, offset) = ZclFrameHeader::parse(&raw).unwrap();
        assert_eq!(header.frame_type, FrameType::ClusterSpecific);
        assert_eq!(header.command_id, 0x02);
        assert_eq!(header.mfr_code, None);
        assert_eq!(offset, 3);
    }

    #[test]
    fn parse_frame_with_mfr_code() {
        // Frame control: cluster-specific (0x01) | manufacturer-specific (0x04)
        let raw = [0x05, 0x7C, 0x11, 0x01, 0x07, 0xAA];
        let (header, offset) = ZclFrameHeader::parse(&raw).unwrap();
        assert_eq!(header.frame_type, FrameType::ClusterSpecific);
        assert_eq!(header.mfr_code, Some(0x117C));
        assert_eq!(header.command_id, 0x07);
        assert_eq!(offset, 5);
        assert_eq!(&raw[offset..], [0xAA]);
    }

    #[test]
    fn parse_frame_control_bits_and_sequence() {
        // Report Attributes from a device: global, server→client, DDR set.
        let (h, _) = ZclFrameHeader::parse(&[0x18, 0x42, 0x0A]).unwrap();
        assert!(h.server_to_client);
        assert!(h.disable_default_response);
        assert_eq!(h.sequence, 0x42);
        // Toggle from a remote: cluster-specific, client→server, DDR clear.
        let (h, _) = ZclFrameHeader::parse(&[0x01, 0x07, 0x02]).unwrap();
        assert!(!h.server_to_client);
        assert!(!h.disable_default_response);
        assert_eq!(h.sequence, 7);
    }

    #[test]
    fn default_response_mirrors_request() {
        let (h, _) = ZclFrameHeader::parse(&[0x01, 0x07, 0x02]).unwrap();
        // Global, server→client (flipped), DDR set; seq 7; cmd 0x0B; [0x02, SUCCESS].
        assert_eq!(default_response_for(&h), Some(vec![0x18, 0x07, 0x0B, 0x02, 0x00]));

        // Manufacturer-specific server→client report without DDR.
        let (h, _) = ZclFrameHeader::parse(&[0x0C, 0x7C, 0x11, 0x09, 0x0A]).unwrap();
        assert_eq!(
            default_response_for(&h),
            Some(vec![0x14, 0x7C, 0x11, 0x09, 0x0B, 0x0A, 0x00])
        );
    }

    #[test]
    fn no_default_response_when_disabled_or_not_owed() {
        // DDR set.
        let (h, _) = ZclFrameHeader::parse(&[0x18, 0x01, 0x0A]).unwrap();
        assert_eq!(default_response_for(&h), None);
        // A Default Response itself.
        let (h, _) = ZclFrameHeader::parse(&[0x08, 0x01, 0x0B]).unwrap();
        assert_eq!(default_response_for(&h), None);
        // A Read Attributes request expects a Read Attributes Response.
        let (h, _) = ZclFrameHeader::parse(&[0x00, 0x01, 0x00]).unwrap();
        assert_eq!(default_response_for(&h), None);
    }

    #[test]
    fn configure_reporting_payload_format() {
        let p = configure_reporting_payload(
            9,
            &[
                ReportingConfig {
                    attr_id: 0x0000,
                    data_type: 0x10, // boolean: no reportable change
                    min_interval: 0,
                    max_interval: 3600,
                    reportable_change: None,
                },
                ReportingConfig {
                    attr_id: 0x0000,
                    data_type: 0x29, // int16
                    min_interval: 10,
                    max_interval: 3600,
                    reportable_change: Some(100),
                },
            ],
        );
        assert_eq!(
            p,
            vec![
                0x10, 9, 0x06, // header
                0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x10, 0x0E, // onOff
                0x00, 0x00, 0x00, 0x29, 0x0A, 0x00, 0x10, 0x0E, 100, 0, // int16, change 100
            ]
        );
    }

    #[test]
    fn parse_frame_truncated_mfr_code_errors() {
        let raw = [0x05, 0x7C];
        assert!(ZclFrameHeader::parse(&raw).is_err());
    }
}
