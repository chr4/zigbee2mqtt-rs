use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct PowerCluster;

// Cluster 0x0001 – Power Configuration
//   0x0020 – Battery voltage     (Uint8, unit: 100mV)
//   0x0021 – Battery percentage remaining (Uint8, unit: 0.5%)
//   0x0034 – Battery alarm mask  (Bitmap8)

const BATTERY_VOLTAGE:    u16 = 0x0020;
const BATTERY_PERCENTAGE: u16 = 0x0021;

/// ZCL sentinel for "invalid/not reported" on these Uint8 attributes.
const ATTR_INVALID: f64 = 0xFF as f64;

impl ClusterHandler for PowerCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            match r.attr_id {
                BATTERY_VOLTAGE => {
                    if let Some(v) = r.value.as_f64() {
                        if v == ATTR_INVALID {
                            continue;
                        }
                        // value in units of 100mV, publish as volts
                        out.push(("battery_voltage".into(), json!(v / 10.0)));
                    }
                }
                BATTERY_PERCENTAGE => {
                    if let Some(v) = r.value.as_f64() {
                        if v == ATTR_INVALID {
                            continue;
                        }
                        // value is half-percent; clamp to 0-100
                        let pct = (v / 2.0).clamp(0.0, 100.0);
                        out.push(("battery".into(), json!(pct.round() as u8)));
                        // low battery boolean (< 10 %)
                        out.push(("battery_low".into(), json!(pct < 10.0)));
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::AttributeValue;

    #[test]
    fn battery_percentage_reports_half_percent_units() {
        let reports = vec![AttributeReport {
            attr_id: BATTERY_PERCENTAGE,
            value: AttributeValue::U8(160), // 80%
        }];
        let out = PowerCluster.process_reports(&reports);
        assert_eq!(out, vec![
            ("battery".into(), json!(80)),
            ("battery_low".into(), json!(false)),
        ]);
    }

    #[test]
    fn battery_percentage_invalid_sentinel_is_ignored() {
        let reports = vec![AttributeReport {
            attr_id: BATTERY_PERCENTAGE,
            value: AttributeValue::U8(0xFF),
        }];
        assert_eq!(PowerCluster.process_reports(&reports), vec![]);
    }

    #[test]
    fn battery_voltage_invalid_sentinel_is_ignored() {
        let reports = vec![AttributeReport {
            attr_id: BATTERY_VOLTAGE,
            value: AttributeValue::U8(0xFF),
        }];
        assert_eq!(PowerCluster.process_reports(&reports), vec![]);
    }

    #[test]
    fn battery_voltage_reports_volts() {
        let reports = vec![AttributeReport {
            attr_id: BATTERY_VOLTAGE,
            value: AttributeValue::U8(30), // 3.0V
        }];
        let out = PowerCluster.process_reports(&reports);
        assert_eq!(out, vec![("battery_voltage".into(), json!(3.0))]);
    }
}
