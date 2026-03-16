use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct HumidityCluster;

// Cluster 0x0405 – Relative Humidity Measurement
//   0x0000 – MeasuredValue (Uint16, unit: 0.01 %)

const MEASURED_VALUE: u16 = 0x0000;

impl ClusterHandler for HumidityCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == MEASURED_VALUE {
                if let Some(v) = r.value.as_f64() {
                    if v < 10_001.0 {
                        let humidity = (v / 100.0 * 100.0).round() / 100.0;
                        out.push(("humidity".into(), json!(humidity)));
                    }
                }
            }
        }
        out
    }
}
