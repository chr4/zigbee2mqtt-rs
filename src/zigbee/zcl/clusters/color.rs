use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct ColorCluster;

// Cluster 0x0300 – Color Control
//   0x0000 – CurrentHue       (Uint8, 0-254 → 0-360°)
//   0x0001 – CurrentSaturation (Uint8, 0-254 → 0-100%)
//   0x0003 – CurrentX         (Uint16, CIE x * 65536)
//   0x0004 – CurrentY         (Uint16, CIE y * 65536)
//   0x0007 – ColorTemperatureMireds (Uint16)
//   0x0008 – ColorMode        (Enum8)

const CURRENT_HUE:         u16 = 0x0000;
const CURRENT_SATURATION:  u16 = 0x0001;
const CURRENT_X:           u16 = 0x0003;
const CURRENT_Y:           u16 = 0x0004;
const COLOR_TEMPERATURE:   u16 = 0x0007;
const COLOR_MODE:          u16 = 0x0008;

impl ClusterHandler for ColorCluster {
    fn cluster_id(&self) -> u16 { 0x0300 }

    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            match r.attr_id {
                CURRENT_HUE => {
                    if let Some(v) = r.value.as_f64() {
                        let hue_degrees = (v / 254.0 * 360.0).round() as u16;
                        out.push(("color_hue".into(), json!(hue_degrees)));
                    }
                }
                CURRENT_SATURATION => {
                    if let Some(v) = r.value.as_f64() {
                        let sat_pct = (v / 254.0 * 100.0).round() as u8;
                        out.push(("color_saturation".into(), json!(sat_pct)));
                    }
                }
                CURRENT_X => {
                    if let Some(v) = r.value.as_f64() {
                        out.push(("color_x".into(), json!(v / 65536.0)));
                    }
                }
                CURRENT_Y => {
                    if let Some(v) = r.value.as_f64() {
                        out.push(("color_y".into(), json!(v / 65536.0)));
                    }
                }
                COLOR_TEMPERATURE => {
                    if let Some(v) = r.value.as_f64() {
                        out.push(("color_temp".into(), json!(v as u16)));
                        // Also publish Kelvin
                        if v > 0.0 {
                            out.push(("color_temp_kelvin".into(), json!((1_000_000.0 / v) as u32)));
                        }
                    }
                }
                COLOR_MODE => {
                    if let Some(v) = r.value.as_f64() {
                        let mode = match v as u8 {
                            0 => "hs",
                            1 => "xy",
                            2 => "color_temperature",
                            _ => "unknown",
                        };
                        out.push(("color_mode".into(), json!(mode)));
                    }
                }
                _ => {}
            }
        }
        out
    }
}
