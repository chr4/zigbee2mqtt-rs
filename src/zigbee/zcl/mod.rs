pub mod attribute;
pub mod clusters;
pub mod frame;

use serde_json::{Map, Value};

use attribute::AttributeReport;
use clusters::{handler_for, ClusterHandler};
use frame::{global, FrameType, ZclFrameHeader};

use crate::error::Result;

/// Decoded result from a ZCL message.
#[derive(Debug, Clone)]
pub struct ZclMessage {
    pub values: Map<String, Value>,
    /// Update to apply to `Device.arrow_hold_direction`, if any -- only ever
    /// set by IKEA TRADFRI-style genScenes commands (see `clusters::scenes`).
    /// `None` = leave the device's stored direction unchanged, `Some(None)`
    /// = clear it, `Some(Some(dir))` = set/replace it.
    pub arrow_hold_direction: Option<Option<String>>,
}

/// Parse a raw ZCL payload (bytes from AF_INCOMING_MSG) and produce a `ZclMessage`.
///
/// `zone_type` is the device's IAS Zone (0x0500) ZoneType attribute, if known
/// from a prior interview read -- it has no effect for any other cluster.
/// Plumbing it through here (rather than via the generic `ClusterHandler`
/// trait, which carries no per-device context) is what lets IAS Zone status
/// reports be classified as "occupancy"/"smoke"/"water_leak"/etc. instead of
/// always being reported as a door/contact sensor.
///
/// `held_arrow_direction` is the device's currently-remembered TRADFRI arrow
/// hold direction (see `clusters::scenes`), if any -- likewise only relevant
/// to genScenes manufacturer-specific commands.
pub fn parse_message(
    cluster_id: u16,
    raw: &[u8],
    zone_type: Option<u16>,
    held_arrow_direction: Option<&str>,
) -> Result<Option<ZclMessage>> {
    let (header, payload_offset) = ZclFrameHeader::parse(raw)?;

    let payload = &raw[payload_offset..];

    let mut arrow_hold_direction: Option<Option<String>> = None;
    let pairs = if cluster_id == 0x0500 {
        match header.frame_type {
            FrameType::Global if header.command_id == global::REPORT_ATTRIBUTES => {
                let reports = AttributeReport::parse_all(payload);
                clusters::ias_zone::process_reports_with_zone_type(&reports, zone_type)
            }
            FrameType::Global if header.command_id == global::READ_ATTRIBUTES_RSP => {
                let reports = parse_read_attr_rsp(payload);
                clusters::ias_zone::process_reports_with_zone_type(&reports, zone_type)
            }
            FrameType::ClusterSpecific => clusters::ias_zone::process_command_with_zone_type(
                header.command_id,
                payload,
                zone_type,
            ),
            FrameType::Global => return Ok(None),
        }
    } else if cluster_id == 0x0005 {
        // Scenes' manufacturer-specific commands need the manufacturer code
        // from the frame header, which the generic ClusterHandler trait has
        // no way to carry -- bypass it the same way IAS Zone bypasses it for
        // zone_type above.
        match header.frame_type {
            FrameType::Global if header.command_id == global::REPORT_ATTRIBUTES => {
                let reports = AttributeReport::parse_all(payload);
                clusters::scenes::ScenesCluster.process_reports(&reports)
            }
            FrameType::Global if header.command_id == global::READ_ATTRIBUTES_RSP => {
                let reports = parse_read_attr_rsp(payload);
                clusters::scenes::ScenesCluster.process_reports(&reports)
            }
            FrameType::ClusterSpecific => {
                if let Some(mfr) = header.mfr_code {
                    tracing::info!(
                        "Manufacturer-specific Scenes command: mfr=0x{mfr:04X} cmd=0x{:02X} payload={:02X?}",
                        header.command_id, payload
                    );
                }
                let (pairs, direction_update) = clusters::scenes::process_command_with_mfr_code(
                    header.command_id,
                    payload,
                    header.mfr_code,
                    held_arrow_direction,
                );
                arrow_hold_direction = direction_update;
                pairs
            }
            FrameType::Global => return Ok(None),
        }
    } else {
        let handler = match handler_for(cluster_id) {
            Some(h) => h,
            None => {
                tracing::debug!("No handler for cluster 0x{cluster_id:04X}");
                return Ok(None);
            }
        };

        match header.frame_type {
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
            FrameType::ClusterSpecific => handler.process_command(header.command_id, payload),
        }
    };

    if pairs.is_empty() && arrow_hold_direction.is_none() {
        return Ok(None);
    }

    let mut values = Map::new();
    for (k, v) in pairs {
        values.insert(k, v);
    }

    Ok(Some(ZclMessage {
        values,
        arrow_hold_direction,
    }))
}

/// Extract the IAS Zone ZoneType attribute from a raw ZCL Read Attributes
/// Response for cluster 0x0500, if present. Used during interview to learn
/// how to classify a device's zone status reports.
pub fn extract_ias_zone_type(raw: &[u8]) -> Result<Option<u16>> {
    let (header, payload_offset) = ZclFrameHeader::parse(raw)?;
    if header.frame_type != FrameType::Global || header.command_id != global::READ_ATTRIBUTES_RSP {
        return Ok(None);
    }
    let reports = parse_read_attr_rsp(&raw[payload_offset..]);
    Ok(clusters::ias_zone::extract_zone_type(&reports))
}

/// Parse a Read Attributes Response payload into AttributeReports.
/// Format per record: attr_id (u16) | status (u8) | [data_type (u8) | value]
fn parse_read_attr_rsp(buf: &[u8]) -> Vec<AttributeReport> {
    let mut reports = Vec::new();
    let mut pos = 0;
    while pos + 3 <= buf.len() {
        let attr_id = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
        let status = buf[pos + 2];
        pos += 3;
        if status != 0x00 {
            continue; // attribute not found
        }
        if pos >= buf.len() {
            break;
        }
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
        let msg = parse_message(0x0006, &raw, None, None).unwrap().unwrap();
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
        let msg = parse_message(0x0402, &raw, None, None).unwrap().unwrap();
        assert_eq!(msg.values["temperature"], 22.5);
    }

    #[test]
    fn parse_unsupported_cluster() {
        let raw = [0x18, 0x01, 0x0A, 0x00, 0x00, 0x10, 0x01];
        let result = parse_message(0xFFFF, &raw, None, None).unwrap();
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
        let msg = parse_message(0x0000, &raw, None, None).unwrap().unwrap();
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
        let msg = parse_message(0x0006, &raw, None, None).unwrap().unwrap();
        // Incoming genOnOff commands are a controller button-press action,
        // not device state -- see `clusters::on_off`.
        assert_eq!(msg.values["action"], "on");
    }

    #[test]
    fn empty_zcl_frame_errors() {
        assert!(parse_message(0x0006, &[], None, None).is_err());
    }

    #[test]
    fn ias_zone_status_change_uses_zone_type() {
        // Cluster-specific Zone Status Change Notification, status=ALARM1 set
        let raw = [
            0x01, // frame control: cluster-specific, client→server
            0x01, // sequence
            0x00, // command: Zone Status Change Notification
            0x01, 0x00, // zone_status = ALARM1
            0x00, 0x00, 0x00, 0x00, // extended_status, zone_id, delay
        ];
        let msg = parse_message(0x0500, &raw, Some(0x000D /* motion sensor */), None)
            .unwrap()
            .unwrap();
        assert_eq!(msg.values["occupancy"], true);
        assert!(!msg.values.contains_key("contact"));
    }

    #[test]
    fn ias_zone_status_change_defaults_to_contact() {
        let raw = [0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        let msg = parse_message(0x0500, &raw, None, None).unwrap().unwrap();
        assert_eq!(msg.values["contact"], false);
    }

    #[test]
    fn extract_ias_zone_type_from_read_attr_rsp() {
        #[rustfmt::skip]
        let raw = [
            0x18, 0x01, 0x01, // header: global, Read Attributes Response
            // Attribute 0x0001 (ZoneType), status=OK, type=Enum16, value=0x000D (motion)
            0x01, 0x00, 0x00, 0x31, 0x0D, 0x00,
        ];
        assert_eq!(extract_ias_zone_type(&raw).unwrap(), Some(0x000D));
    }

    #[test]
    fn extract_ias_zone_type_none_for_other_commands() {
        let raw = [0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(extract_ias_zone_type(&raw).unwrap(), None);
    }

    #[test]
    fn parse_scenes_recall_command() {
        let raw = [
            0x01, 0x01, 0x05, // cluster-specific, seq=1, cmd=Recall Scene
            0x01, 0x00, 0x05, // group=1, scene=5
        ];
        let msg = parse_message(0x0005, &raw, None, None).unwrap().unwrap();
        assert_eq!(msg.values["action"], "recall");
        assert_eq!(msg.values["action_scene"], 5);
    }

    #[test]
    fn parse_scenes_ikea_arrow_click() {
        // Frame control: cluster-specific | manufacturer-specific, mfr=0x117C
        // (IKEA), cmd=0x07 (ArrowSingle), value=257 (LE) -> left, value2=0
        let raw = [0x05, 0x7C, 0x11, 0x01, 0x07, 0x01, 0x01, 0x00, 0x00];
        let msg = parse_message(0x0005, &raw, None, None).unwrap().unwrap();
        assert_eq!(msg.values["action"], "arrow_left_click");
    }

    #[test]
    fn parse_scenes_ikea_arrow_hold_and_release() {
        // ArrowHold (0x08), value=3329 (LE) -> left
        let hold_raw = [0x05, 0x7C, 0x11, 0x01, 0x08, 0x01, 0x0D];
        let hold_msg = parse_message(0x0005, &hold_raw, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(hold_msg.values["action"], "arrow_left_hold");
        assert_eq!(
            hold_msg.arrow_hold_direction,
            Some(Some("left".to_string()))
        );

        // ArrowRelease (0x09), value=1500 -> 1.5s, using the direction just remembered
        let release_raw = [0x05, 0x7C, 0x11, 0x02, 0x09, 0xDC, 0x05];
        let release_msg = parse_message(0x0005, &release_raw, None, Some("left"))
            .unwrap()
            .unwrap();
        assert_eq!(release_msg.values["action"], "arrow_left_release");
        assert_eq!(release_msg.values["action_duration"], 1.5);
        assert_eq!(release_msg.arrow_hold_direction, Some(None));
    }

    #[test]
    fn parse_scenes_unmapped_manufacturer_specific_command() {
        // A manufacturer code we don't specifically handle stays generic/honest.
        let raw = [0x05, 0x99, 0x99, 0x01, 0x07];
        let msg = parse_message(0x0005, &raw, None, None).unwrap().unwrap();
        assert_eq!(msg.values["action"], "manufacturer_0x9999_cmd_0x07");
    }
}
