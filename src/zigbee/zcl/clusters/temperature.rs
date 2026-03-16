use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct TemperatureCluster;

// Cluster 0x0402 – Temperature Measurement
//   0x0000 – MeasuredValue (Int16, unit: 0.01 °C)
//   0x0001 – MinMeasuredValue
//   0x0002 – MaxMeasuredValue
//   0x0003 – Tolerance

const MEASURED_VALUE: u16 = 0x0000;

impl ClusterHandler for TemperatureCluster {
    fn cluster_id(&self) -> u16 { 0x0402 }

    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            if r.attr_id == MEASURED_VALUE {
                if let Some(v) = r.value.as_f64() {
                    // 0x8000 = invalid
                    if v as i16 != -32768i16 {
                        let celsius = v / 100.0;
                        // Round to 2 decimal places
                        let celsius = (celsius * 100.0).round() / 100.0;
                        out.push(("temperature".into(), json!(celsius)));
                    }
                }
            }
        }
        out
    }
}
