pub mod attribute;
pub mod clusters;
pub mod frame;

use serde_json::{Map, Value};

use attribute::AttributeReport;
use clusters::handler_for;
use frame::{ZclFrameHeader, FrameType, global};

use crate::error::Result;

/// Decoded result from a ZCL message.
#[derive(Debug, Clone)]
pub struct ZclMessage {
    pub cluster_id: u16,
    /// Flattened key/value pairs ready for MQTT publishing
    pub values:     Map<String, Value>,
    /// True if this message should trigger a "set" rather than "state" publish
    pub is_command: bool,
}

/// Parse a raw ZCL payload (bytes from AF_INCOMING_MSG) and produce a `ZclMessage`.
pub fn parse_message(cluster_id: u16, raw: &[u8]) -> Result<Option<ZclMessage>> {
    let (header, payload_offset) = ZclFrameHeader::parse(raw)?;

    let payload = &raw[payload_offset..];

    let handler = match handler_for(cluster_id) {
        Some(h) => h,
        None => {
            tracing::debug!("No handler for cluster 0x{cluster_id:04X}");
            return Ok(None);
        }
    };

    let pairs = match header.frame_type {
        FrameType::Global => {
            if header.command_id == global::REPORT_ATTRIBUTES {
                let reports = AttributeReport::parse_all(payload);
                handler.process_reports(&reports)
            } else if header.command_id == global::READ_ATTRIBUTES_RSP {
                // Parse Read Attributes Response (includes status byte per attribute)
                let reports = parse_read_attr_rsp(payload);
                handler.process_reports(&reports)
            } else {
                return Ok(None);
            }
        }
        FrameType::ClusterSpecific => {
            handler.process_command(header.command_id, payload)
        }
    };

    if pairs.is_empty() {
        return Ok(None);
    }

    let mut values = Map::new();
    for (k, v) in pairs {
        values.insert(k, v);
    }

    Ok(Some(ZclMessage {
        cluster_id,
        values,
        is_command: header.frame_type == FrameType::ClusterSpecific,
    }))
}

/// Parse a Read Attributes Response payload into AttributeReports.
/// Format per record: attr_id (u16) | status (u8) | [data_type (u8) | value]
fn parse_read_attr_rsp(buf: &[u8]) -> Vec<AttributeReport> {
    let mut reports = Vec::new();
    let mut pos = 0;
    while pos + 3 <= buf.len() {
        let attr_id = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
        let status  = buf[pos + 2];
        pos += 3;
        if status != 0x00 {
            continue; // attribute not found
        }
        if pos >= buf.len() { break; }
        let data_type = attribute::DataType::from_u8(buf[pos]);
        pos += 1;
        match attribute::AttributeValue::parse(data_type, &buf[pos..]) {
            Ok((value, consumed)) => {
                reports.push(AttributeReport { attr_id, value });
                pos += consumed;
            }
            Err(e) => {
                tracing::warn!("Error in read_attr_rsp attr=0x{attr_id:04X}: {e}");
                break;
            }
        }
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_on_off_report_attributes() {
        // ZCL frame: global, client→server, seq=1, cmd=0x0A (Report Attributes)
        // Report: attr_id=0x0000, type=Boolean(0x10), value=0x01 (ON)
        #[rustfmt::skip]
        let raw = [
            0x18, // frame control: global, server→client, disable default rsp
            0x01, // sequence
            0x0A, // command: Report Attributes
            0x00, 0x00, // attr_id = 0x0000
            0x10,       // data_type = Boolean
            0x01,       // value = true
        ];
        let msg = parse_message(0x0006, &raw).unwrap().unwrap();
        assert_eq!(msg.values["state"], "ON");
    }

    #[test]
    fn parse_temperature_report() {
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x0A, // header: global, report attributes
            0x00, 0x00,       // attr_id = 0x0000
            0x29,             // data_type = Int16
            0xCA, 0x08,       // value = 2250 (22.50°C)
        ];
        let msg = parse_message(0x0402, &raw).unwrap().unwrap();
        assert_eq!(msg.values["temperature"], 22.5);
    }

    #[test]
    fn parse_unsupported_cluster() {
        let raw = [0x18, 0x01, 0x0A, 0x00, 0x00, 0x10, 0x01];
        let result = parse_message(0xFFFF, &raw).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_read_attr_rsp_basic_cluster() {
        // Read Attributes Response for basic cluster (manufacturer + model)
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x01, // header: global, Read Attributes Response
            // Attribute 0x0004 (manufacturer), status=OK, type=CharStr
            0x04, 0x00, 0x00, 0x42, 0x04, b'I', b'K', b'E', b'A',
            // Attribute 0x0005 (model), status=OK, type=CharStr
            0x05, 0x00, 0x00, 0x42, 0x05, b'B', b'U', b'L', b'B', b'1',
        ];
        let msg = parse_message(0x0000, &raw).unwrap().unwrap();
        assert_eq!(msg.values["manufacturer"], "IKEA");
        assert_eq!(msg.values["model"], "BULB1");
    }

    #[test]
    fn parse_cluster_specific_on_off() {
        // Cluster-specific On command
        let raw = [
            0x01, // frame control: cluster-specific, client→server
            0x01, // sequence
            0x01, // command: On
        ];
        let msg = parse_message(0x0006, &raw).unwrap().unwrap();
        assert_eq!(msg.values["state"], "ON");
    }

    #[test]
    fn empty_zcl_frame_errors() {
        assert!(parse_message(0x0006, &[]).is_err());
    }
}
