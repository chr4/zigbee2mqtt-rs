use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct IasZoneCluster;

// Cluster 0x0500 – IAS Zone (door/window sensors, motion sensors, smoke detectors)
//   Attribute 0x0000 – ZoneState  (Enum8)
//   Attribute 0x0001 – ZoneType   (Enum16)
//   Attribute 0x0002 – ZoneStatus (Bitmap16)
//
// Cluster-specific commands (server → client):
//   0x00 – Zone Status Change Notification

const ZONE_STATUS: u16 = 0x0002;

const ALARM1: u16 = 0x0001;
const TAMPER: u16 = 0x0004;
const BATTERY: u16 = 0x0008;
const TROUBLE: u16 = 0x0040;

impl ClusterHandler for IasZoneCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == ZONE_STATUS {
                if let Some(v) = r.value.as_f64() {
                    out.extend(decode_zone_status(v as u16));
                }
            }
        }
        out
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        // 0x00 = Zone Status Change Notification
        // payload: zone_status (u16) | extended_status (u8) | zone_id (u8) | delay (u16)
        if command_id == 0x00 && payload.len() >= 2 {
            let zone_status = u16::from_le_bytes([payload[0], payload[1]]);
            return decode_zone_status(zone_status);
        }
        vec![]
    }
}

fn decode_zone_status(status: u16) -> Vec<(String, Value)> {
    vec![
        ("contact".into(), json!((status & ALARM1) == 0)), // contact closed = no alarm
        ("tamper".into(), json!((status & TAMPER) != 0)),
        ("battery_low".into(), json!((status & BATTERY) != 0)),
        ("trouble".into(), json!((status & TROUBLE) != 0)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeReport, AttributeValue};

    #[test]
    fn zone_status_closed() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0x0000), // all clear
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(true)));
        assert!(result.iter().any(|(k, v)| k == "tamper" && v == &json!(false)));
    }

    #[test]
    fn zone_status_open() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0x0001), // ALARM1 = open
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(false)));
    }

    #[test]
    fn zone_status_tamper() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0x0004), // TAMPER
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "tamper" && v == &json!(true)));
    }

    #[test]
    fn zone_status_change_notification() {
        // Command 0x00 with zone_status = open + tamper
        let payload = [0x05, 0x00, 0x00, 0x01, 0x00, 0x00]; // status=0x0005
        let result = IasZoneCluster.process_command(0x00, &payload);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(false)));
        assert!(result.iter().any(|(k, v)| k == "tamper" && v == &json!(true)));
    }
}
