use serde_json::{json, Value};

use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct ColorCluster;

// Cluster 0x0300 – Color Control
const CURRENT_HUE: u16 = 0x0000;
const CURRENT_SATURATION: u16 = 0x0001;
const CURRENT_X: u16 = 0x0003;
const CURRENT_Y: u16 = 0x0004;
const COLOR_TEMPERATURE: u16 = 0x0007;
const COLOR_MODE: u16 = 0x0008;

impl ClusterHandler for ColorCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        // Collect raw values to build nested color object (z2m format)
        let mut hue: Option<u16> = None;
        let mut saturation: Option<u8> = None;
        let mut x: Option<f64> = None;
        let mut y: Option<f64> = None;

        for r in reports {
            match r.attr_id {
                CURRENT_HUE => {
                    if let Some(v) = r.value.as_f64() {
                        let h = (v / 254.0 * 360.0).round() as u16;
                        hue = Some(h);
                    }
                }
                CURRENT_SATURATION => {
                    if let Some(v) = r.value.as_f64() {
                        let s = (v / 254.0 * 100.0).round() as u8;
                        saturation = Some(s);
                    }
                }
                CURRENT_X => {
                    if let Some(v) = r.value.as_f64() {
                        x = Some((v / 65536.0 * 10000.0).round() / 10000.0);
                    }
                }
                CURRENT_Y => {
                    if let Some(v) = r.value.as_f64() {
                        y = Some((v / 65536.0 * 10000.0).round() / 10000.0);
                    }
                }
                COLOR_TEMPERATURE => {
                    if let Some(v) = r.value.as_f64() {
                        let mireds = v as u16;
                        out.push(("color_temp".into(), json!(mireds)));
                    }
                }
                COLOR_MODE => {
                    if let Some(v) = r.value.as_f64() {
                        let mode = match v as u8 {
                            0 => "hs",
                            1 => "xy",
                            2 => "color_temp",
                            _ => "unknown",
                        };
                        out.push(("color_mode".into(), json!(mode)));
                    }
                }
                _ => {}
            }
        }

        // Build nested color object (z2m compatible)
        if hue.is_some() || saturation.is_some() || x.is_some() || y.is_some() {
            let mut color = serde_json::Map::new();
            if let Some(h) = hue {
                color.insert("hue".into(), json!(h));
            }
            if let Some(s) = saturation {
                color.insert("saturation".into(), json!(s));
            }
            if let Some(xv) = x {
                color.insert("x".into(), json!(xv));
            }
            if let Some(yv) = y {
                color.insert("y".into(), json!(yv));
            }
            out.push(("color".into(), Value::Object(color)));
        }

        out
    }
}

/// Build ZCL Move to Color Temperature payload.
/// mireds: color temperature in mireds, transition: time in 1/10s units
pub fn move_to_color_temp_payload(sequence: u8, mireds: u16, transition: u16) -> Vec<u8> {
    vec![
        0x11,
        sequence,
        0x0A, // Move to Color Temperature
        (mireds & 0xFF) as u8,
        (mireds >> 8) as u8,
        (transition & 0xFF) as u8,
        (transition >> 8) as u8,
    ]
}

/// Build ZCL Move to Hue and Saturation payload.
/// hue: 0-254, saturation: 0-254, transition: time in 1/10s units
pub fn move_to_hue_sat_payload(sequence: u8, hue: u8, saturation: u8, transition: u16) -> Vec<u8> {
    vec![
        0x11,
        sequence,
        0x06, // Move to Hue and Saturation
        hue,
        saturation,
        (transition & 0xFF) as u8,
        (transition >> 8) as u8,
    ]
}

/// Build ZCL Move to Color (XY) payload.
/// x, y: CIE 1931 coordinates (0.0-1.0), transition: time in 1/10s units
pub fn move_to_color_xy_payload(sequence: u8, x: f64, y: f64, transition: u16) -> Vec<u8> {
    let xi = (x * 65536.0).round() as u16;
    let yi = (y * 65536.0).round() as u16;
    vec![
        0x11,
        sequence,
        0x07, // Move to Color
        (xi & 0xFF) as u8,
        (xi >> 8) as u8,
        (yi & 0xFF) as u8,
        (yi >> 8) as u8,
        (transition & 0xFF) as u8,
        (transition >> 8) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeValue, AttributeReport};

    #[test]
    fn color_temp_report() {
        let reports = vec![AttributeReport {
            attr_id: COLOR_TEMPERATURE,
            value: AttributeValue::U16(370),
        }];
        let result = ColorCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "color_temp" && v == &json!(370)));
    }

    #[test]
    fn color_hs_reports_nested() {
        let reports = vec![
            AttributeReport {
                attr_id: CURRENT_HUE,
                value: AttributeValue::U8(127), // ~180°
            },
            AttributeReport {
                attr_id: CURRENT_SATURATION,
                value: AttributeValue::U8(254), // 100%
            },
        ];
        let result = ColorCluster.process_reports(&reports);
        let color = result.iter().find(|(k, _)| k == "color");
        assert!(color.is_some());
        let color_obj = color.unwrap().1.as_object().unwrap();
        assert!(color_obj.contains_key("hue"));
        assert!(color_obj.contains_key("saturation"));
        assert_eq!(color_obj["saturation"], json!(100));
    }

    #[test]
    fn color_xy_reports_nested() {
        let reports = vec![
            AttributeReport {
                attr_id: CURRENT_X,
                value: AttributeValue::U16(19660), // ~0.3
            },
            AttributeReport {
                attr_id: CURRENT_Y,
                value: AttributeValue::U16(19660),
            },
        ];
        let result = ColorCluster.process_reports(&reports);
        let color = result.iter().find(|(k, _)| k == "color");
        assert!(color.is_some());
        let color_obj = color.unwrap().1.as_object().unwrap();
        assert!(color_obj.contains_key("x"));
        assert!(color_obj.contains_key("y"));
    }

    #[test]
    fn color_mode_report() {
        let reports = vec![AttributeReport {
            attr_id: COLOR_MODE,
            value: AttributeValue::U8(2),
        }];
        let result = ColorCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "color_mode" && v == &json!("color_temp")));
    }

    #[test]
    fn move_to_color_temp_payload_format() {
        let p = move_to_color_temp_payload(1, 370, 10);
        assert_eq!(p[0], 0x11); // cluster-specific frame control
        assert_eq!(p[2], 0x0A); // Move to Color Temperature command
        assert_eq!(u16::from_le_bytes([p[3], p[4]]), 370);
        assert_eq!(u16::from_le_bytes([p[5], p[6]]), 10);
    }
}
