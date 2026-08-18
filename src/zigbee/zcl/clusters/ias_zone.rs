use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct IasZoneCluster;

// Cluster 0x0500 – IAS Zone (door/window sensors, motion sensors, smoke detectors, ...)
//   Attribute 0x0000 – ZoneState  (Enum8)
//   Attribute 0x0001 – ZoneType   (Enum16)
//   Attribute 0x0002 – ZoneStatus (Bitmap16)
//
// Cluster-specific commands (server → client):
//   0x00 – Zone Status Change Notification

pub const ZONE_TYPE: u16 = 0x0001;
const ZONE_STATUS: u16 = 0x0002;

const ALARM1: u16 = 0x0001;
const TAMPER: u16 = 0x0004;
const BATTERY: u16 = 0x0008;
const TROUBLE: u16 = 0x0040;

/// ZCL Zone Type enum values (ZCL spec section 8.2.2.2) this bridge
/// distinguishes for reporting. Anything not listed here falls back to the
/// "contact" mapping used by Contact Switch, which was this bridge's only
/// supported behavior before ZoneType was read during interview.
const ZONE_TYPE_MOTION_SENSOR: u16 = 0x000D;
const ZONE_TYPE_FIRE_SENSOR: u16 = 0x0028;
const ZONE_TYPE_WATER_SENSOR: u16 = 0x002A;
const ZONE_TYPE_CARBON_MONOXIDE_SENSOR: u16 = 0x002B;
const ZONE_TYPE_VIBRATION_MOVEMENT_SENSOR: u16 = 0x002D;

/// The JSON key ZoneStatus's ALARM1 bit is reported under, and whether that
/// bit should be inverted (Contact Switch reports ALARM1=1 as "open", i.e.
/// contact=false; every other zone type here reports ALARM1=1 as the alarm
/// condition being *true*).
fn alarm1_field(zone_type: Option<u16>) -> (&'static str, bool) {
    match zone_type {
        Some(ZONE_TYPE_MOTION_SENSOR) => ("occupancy", false),
        Some(ZONE_TYPE_FIRE_SENSOR) => ("smoke", false),
        Some(ZONE_TYPE_WATER_SENSOR) => ("water_leak", false),
        Some(ZONE_TYPE_CARBON_MONOXIDE_SENSOR) => ("carbon_monoxide", false),
        Some(ZONE_TYPE_VIBRATION_MOVEMENT_SENSOR) => ("vibration", false),
        _ => ("contact", true), // Contact Switch (0x0015) and unknown/unread types
    }
}

impl ClusterHandler for IasZoneCluster {
    // The generic ClusterHandler trait has no per-device context, so this
    // always falls back to the "contact" mapping (zone_type=None). zcl::
    // parse_message() special-cases cluster 0x0500 to call
    // process_reports_with_zone_type()/process_command_with_zone_type()
    // directly with the device's stored ZoneType instead of going through
    // this trait.
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        process_reports_with_zone_type(reports, None)
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        process_command_with_zone_type(command_id, payload, None)
    }
}

/// Zone-type-aware equivalent of `ClusterHandler::process_reports`.
pub fn process_reports_with_zone_type(
    reports: &[AttributeReport],
    zone_type: Option<u16>,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for r in reports {
        if r.attr_id == ZONE_STATUS {
            if let Some(v) = r.value.as_f64() {
                out.extend(decode_zone_status(v as u16, zone_type));
            }
        }
    }
    out
}

/// Zone-type-aware equivalent of `ClusterHandler::process_command`.
pub fn process_command_with_zone_type(
    command_id: u8,
    payload: &[u8],
    zone_type: Option<u16>,
) -> Vec<(String, Value)> {
    // 0x00 = Zone Status Change Notification
    // payload: zone_status (u16) | extended_status (u8) | zone_id (u8) | delay (u16)
    if command_id == 0x00 && payload.len() >= 2 {
        let zone_status = u16::from_le_bytes([payload[0], payload[1]]);
        return decode_zone_status(zone_status, zone_type);
    }
    vec![]
}

/// Decode a ZoneStatus bitmap into state key/value pairs, using `zone_type`
/// (if known) to pick the correct field name and ALARM1 polarity.
pub fn decode_zone_status(status: u16, zone_type: Option<u16>) -> Vec<(String, Value)> {
    let (field, invert) = alarm1_field(zone_type);
    let alarm1 = (status & ALARM1) != 0;
    vec![
        (field.into(), json!(if invert { !alarm1 } else { alarm1 })),
        ("tamper".into(), json!((status & TAMPER) != 0)),
        ("battery_low".into(), json!((status & BATTERY) != 0)),
        ("trouble".into(), json!((status & TROUBLE) != 0)),
    ]
}

/// Extract the ZoneType attribute value from a Read Attributes Response,
/// if present -- used during interview to learn how to classify this device's
/// zone status reports.
pub fn extract_zone_type(reports: &[AttributeReport]) -> Option<u16> {
    reports
        .iter()
        .find(|r| r.attr_id == ZONE_TYPE)
        .and_then(|r| r.value.as_f64())
        .map(|v| v as u16)
}

/// Home Assistant `device_class` for the binary_sensor matching `alarm1_field`'s
/// mapping. `None` falls back to the generic "contact"/door class.
pub fn ha_device_class(zone_type: Option<u16>) -> &'static str {
    match zone_type {
        Some(ZONE_TYPE_MOTION_SENSOR) => "motion",
        Some(ZONE_TYPE_FIRE_SENSOR) => "smoke",
        Some(ZONE_TYPE_WATER_SENSOR) => "moisture",
        Some(ZONE_TYPE_CARBON_MONOXIDE_SENSOR) => "carbon_monoxide",
        Some(ZONE_TYPE_VIBRATION_MOVEMENT_SENSOR) => "vibration",
        _ => "door",
    }
}

/// (state field key, payload_on, payload_off) for the Home Assistant
/// binary_sensor discovery config matching `alarm1_field`'s mapping.
pub fn ha_binary_sensor_fields(zone_type: Option<u16>) -> (&'static str, bool, bool) {
    let (field, invert) = alarm1_field(zone_type);
    if invert {
        (field, false, true)
    } else {
        (field, true, false)
    }
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

    #[test]
    fn motion_sensor_zone_type_reports_occupancy_not_contact() {
        let result = decode_zone_status(0x0001, Some(ZONE_TYPE_MOTION_SENSOR));
        assert!(result.iter().any(|(k, v)| k == "occupancy" && v == &json!(true)));
        assert!(!result.iter().any(|(k, _)| k == "contact"));
    }

    #[test]
    fn motion_sensor_zone_type_clear() {
        let result = decode_zone_status(0x0000, Some(ZONE_TYPE_MOTION_SENSOR));
        assert!(result.iter().any(|(k, v)| k == "occupancy" && v == &json!(false)));
    }

    #[test]
    fn water_sensor_zone_type_reports_water_leak() {
        let result = decode_zone_status(0x0001, Some(ZONE_TYPE_WATER_SENSOR));
        assert!(result.iter().any(|(k, v)| k == "water_leak" && v == &json!(true)));
    }

    #[test]
    fn fire_sensor_zone_type_reports_smoke() {
        let result = decode_zone_status(0x0001, Some(ZONE_TYPE_FIRE_SENSOR));
        assert!(result.iter().any(|(k, v)| k == "smoke" && v == &json!(true)));
    }

    #[test]
    fn unknown_zone_type_falls_back_to_contact() {
        let result = decode_zone_status(0x0001, Some(0x9999));
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(false)));
    }

    #[test]
    fn no_zone_type_falls_back_to_contact() {
        let result = decode_zone_status(0x0000, None);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(true)));
    }

    #[test]
    fn extract_zone_type_from_read_attr_rsp() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_TYPE,
            value: AttributeValue::U16(ZONE_TYPE_MOTION_SENSOR),
        }];
        assert_eq!(extract_zone_type(&reports), Some(ZONE_TYPE_MOTION_SENSOR));
    }

    #[test]
    fn extract_zone_type_absent() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0),
        }];
        assert_eq!(extract_zone_type(&reports), None);
    }

    #[test]
    fn ha_device_class_mapping() {
        assert_eq!(ha_device_class(Some(ZONE_TYPE_MOTION_SENSOR)), "motion");
        assert_eq!(ha_device_class(Some(ZONE_TYPE_WATER_SENSOR)), "moisture");
        assert_eq!(ha_device_class(Some(ZONE_TYPE_FIRE_SENSOR)), "smoke");
        assert_eq!(ha_device_class(None), "door");
        assert_eq!(ha_device_class(Some(0x0015)), "door"); // Contact Switch
    }
}
