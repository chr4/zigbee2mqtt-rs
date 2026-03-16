/// Import device data from an existing zigbee2mqtt `database.db` file.
///
/// The database.db is a newline-delimited JSON file where each line is a device
/// record from zigbee-herdsman. This allows drop-in replacement of zigbee2mqtt
/// without re-pairing devices.
use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::config::DeviceConfig;
use crate::devices::Device;
use crate::zigbee::{EndpointDesc, IeeeAddr};

/// A single device record from zigbee2mqtt's database.db (NDJSON format).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DbEntry {
    #[serde(default, rename = "type")]
    device_type: String,
    #[serde(default)]
    ieee_addr: String,
    #[serde(default)]
    nwk_addr: u16,
    #[serde(default)]
    manuf_name: Option<String>,
    #[serde(default)]
    power_source: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    sw_build_id: Option<String>,
    #[serde(default)]
    endpoints: HashMap<String, DbEndpoint>,
    #[serde(default)]
    interview_completed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DbEndpoint {
    #[serde(default)]
    prof_id: u16,
    #[serde(default)]
    ep_id: u8,
    #[serde(default)]
    dev_id: u16,
    #[serde(default)]
    in_cluster_list: Vec<u16>,
    #[serde(default)]
    out_cluster_list: Vec<u16>,
}

/// Load devices from a zigbee2mqtt database.db file.
/// Returns the list of imported devices and the coordinator IEEE if found.
pub fn load_database(
    path: &Path,
    device_configs: &HashMap<String, DeviceConfig>,
) -> (Vec<Device>, Option<IeeeAddr>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Cannot read database file {}: {e}", path.display());
            return (vec![], None);
        }
    };

    let mut devices = Vec::new();
    let mut coordinator_ieee = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: DbEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                debug!("Skipping unparseable database line: {e}");
                continue;
            }
        };

        // Skip the coordinator entry
        if entry.device_type == "Coordinator" {
            if let Some(ieee) = parse_ieee(&entry.ieee_addr) {
                coordinator_ieee = Some(ieee);
            }
            continue;
        }

        // Skip group entries or entries without IEEE
        if entry.ieee_addr.is_empty() {
            continue;
        }

        let ieee = match parse_ieee(&entry.ieee_addr) {
            Some(a) => a,
            None => {
                debug!("Skipping entry with invalid IEEE: {}", entry.ieee_addr);
                continue;
            }
        };

        let mut dev = Device::new(ieee, entry.nwk_addr);

        // Apply friendly name from configuration.yaml
        let ieee_hex = ieee.as_hex();
        if let Some(cfg) = device_configs.get(&ieee_hex) {
            if let Some(ref name) = cfg.friendly_name {
                dev.friendly_name = name.clone();
            }
            dev.disabled = cfg.disabled.unwrap_or(false);
        }

        dev.manufacturer = entry.manuf_name;
        dev.power_source = entry.power_source;
        dev.model = entry.model_id;
        dev.sw_build_id = entry.sw_build_id;
        dev.interview_complete = entry.interview_completed;

        // Import endpoints
        for (ep_key, ep_data) in &entry.endpoints {
            let ep_id = ep_key.parse::<u8>().unwrap_or(ep_data.ep_id);
            dev.endpoints.push(EndpointDesc {
                endpoint: ep_id,
                profile_id: ep_data.prof_id,
                device_id: ep_data.dev_id,
                input_clusters: ep_data.in_cluster_list.clone(),
                output_clusters: ep_data.out_cluster_list.clone(),
            });
        }

        debug!(
            "Imported device {} ({}), NWK=0x{:04X}, {} endpoints, interviewed={}",
            dev.friendly_name,
            ieee_hex,
            dev.nwk_addr,
            dev.endpoints.len(),
            dev.interview_complete
        );

        devices.push(dev);
    }

    info!(
        "Imported {} devices from {}",
        devices.len(),
        path.display()
    );
    (devices, coordinator_ieee)
}

fn parse_ieee(s: &str) -> Option<IeeeAddr> {
    let hex = s.trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() != 16 {
        return None;
    }
    let mut bytes = [0u8; 8];
    for i in 0..8 {
        bytes[7 - i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(IeeeAddr(bytes))
}

/// Find the database.db file. Checks common locations relative to the config file.
pub fn find_database(config_path: &Path) -> Option<std::path::PathBuf> {
    // Same directory as configuration.yaml
    let dir = config_path.parent().unwrap_or(Path::new("."));
    let candidates = [
        dir.join("database.db"),
        dir.join("data/database.db"),
        // Common z2m install locations
        Path::new("/opt/zigbee2mqtt/data/database.db").to_path_buf(),
        Path::new("/var/lib/zigbee2mqtt/database.db").to_path_buf(),
    ];

    for path in &candidates {
        if path.exists() {
            info!("Found zigbee2mqtt database: {}", path.display());
            return Some(path.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SAMPLE_DB: &str = r#"{"id":1,"type":"Coordinator","ieeeAddr":"0x00124b00120144ae","nwkAddr":0,"manufId":4169,"epList":[1,242],"endpoints":{"1":{"profId":260,"epId":1,"devId":101,"inClusterList":[0,3,6,8],"outClusterList":[0,3,4,5,6,8],"clusters":{},"binds":[],"configuredReportings":[],"meta":{}}},"interviewCompleted":true,"meta":{}}
{"id":2,"type":"Router","ieeeAddr":"0xec1bbdfffeaa66db","nwkAddr":44718,"manufId":4476,"manufName":"IKEA of Sweden","powerSource":"Mains (single phase)","modelId":"TRADFRI bulb E27 CWS 806lm","epList":[1],"endpoints":{"1":{"profId":260,"epId":1,"devId":512,"inClusterList":[0,3,4,5,6,8,768,4096,64636],"outClusterList":[5,25,32,4096],"clusters":{},"binds":[],"configuredReportings":[],"meta":{}}},"appVersion":1,"stackVersion":6,"hwVersion":1,"dateCode":"20210331","swBuildId":"2.3.093","zclVersion":3,"interviewCompleted":true,"meta":{},"lastSeen":1710612345000}
{"id":3,"type":"EndDevice","ieeeAddr":"0xcc86ecfffe9fd1b1","nwkAddr":17895,"manufName":"Xiaomi","powerSource":"Battery","modelId":"lumi.sensor_ht.agl02","epList":[1],"endpoints":{"1":{"profId":260,"epId":1,"devId":770,"inClusterList":[0,1,3,1026,1029],"outClusterList":[],"clusters":{},"binds":[],"configuredReportings":[],"meta":{}}},"interviewCompleted":true,"meta":{}}"#;

    fn sample_configs() -> HashMap<String, DeviceConfig> {
        let mut m = HashMap::new();
        m.insert(
            "0xec1bbdfffeaa66db".to_string(),
            DeviceConfig {
                friendly_name: Some("living_room_bulb".to_string()),
                ..Default::default()
            },
        );
        m.insert(
            "0xcc86ecfffe9fd1b1".to_string(),
            DeviceConfig {
                friendly_name: Some("bedroom_sensor".to_string()),
                ..Default::default()
            },
        );
        m
    }

    fn load_sample() -> (Vec<Device>, Option<IeeeAddr>) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("z2m_test_db_{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.db");
        std::fs::write(&path, SAMPLE_DB).unwrap();
        let configs = sample_configs();
        let result = load_database(&path, &configs);
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    #[test]
    fn loads_devices_not_coordinator() {
        let (devices, coord) = load_sample();
        // Should have 2 devices (Router + EndDevice), not the Coordinator
        assert_eq!(devices.len(), 2);
        assert!(coord.is_some());
    }

    #[test]
    fn coordinator_ieee_extracted() {
        let (_, coord) = load_sample();
        assert_eq!(coord.unwrap().as_hex(), "0x00124b00120144ae");
    }

    #[test]
    fn device_ieee_and_nwk() {
        let (devices, _) = load_sample();
        let bulb = devices.iter().find(|d| d.friendly_name == "living_room_bulb").unwrap();
        assert_eq!(bulb.ieee_addr.as_hex(), "0xec1bbdfffeaa66db");
        assert_eq!(bulb.nwk_addr, 44718);
    }

    #[test]
    fn friendly_names_from_config() {
        let (devices, _) = load_sample();
        let names: Vec<&str> = devices.iter().map(|d| d.friendly_name.as_str()).collect();
        assert!(names.contains(&"living_room_bulb"));
        assert!(names.contains(&"bedroom_sensor"));
    }

    #[test]
    fn manufacturer_and_model_imported() {
        let (devices, _) = load_sample();
        let bulb = devices.iter().find(|d| d.friendly_name == "living_room_bulb").unwrap();
        assert_eq!(bulb.manufacturer.as_deref(), Some("IKEA of Sweden"));
        assert_eq!(bulb.model.as_deref(), Some("TRADFRI bulb E27 CWS 806lm"));
        assert_eq!(bulb.sw_build_id.as_deref(), Some("2.3.093"));
    }

    #[test]
    fn endpoints_imported() {
        let (devices, _) = load_sample();
        let bulb = devices.iter().find(|d| d.friendly_name == "living_room_bulb").unwrap();
        assert_eq!(bulb.endpoints.len(), 1);
        let ep = &bulb.endpoints[0];
        assert_eq!(ep.endpoint, 1);
        assert_eq!(ep.profile_id, 260); // HA profile
        assert!(ep.input_clusters.contains(&0x0006)); // On/Off
        assert!(ep.input_clusters.contains(&0x0008)); // Level
        assert!(ep.input_clusters.contains(&0x0300)); // Color (768)
    }

    #[test]
    fn sensor_endpoints() {
        let (devices, _) = load_sample();
        let sensor = devices.iter().find(|d| d.friendly_name == "bedroom_sensor").unwrap();
        let ep = &sensor.endpoints[0];
        assert!(ep.input_clusters.contains(&0x0402)); // Temperature (1026)
        assert!(ep.input_clusters.contains(&0x0405)); // Humidity (1029)
        assert!(ep.input_clusters.contains(&0x0001)); // Power config
    }

    #[test]
    fn interview_state() {
        let (devices, _) = load_sample();
        assert!(devices.iter().all(|d| d.interview_complete));
    }

    #[test]
    fn power_source_imported() {
        let (devices, _) = load_sample();
        let sensor = devices.iter().find(|d| d.friendly_name == "bedroom_sensor").unwrap();
        assert_eq!(sensor.power_source.as_deref(), Some("Battery"));
        assert_eq!(sensor.device_type(), "EndDevice");
    }

    #[test]
    fn device_type_from_power_source() {
        let (devices, _) = load_sample();
        let bulb = devices.iter().find(|d| d.friendly_name == "living_room_bulb").unwrap();
        assert_eq!(bulb.power_source.as_deref(), Some("Mains (single phase)"));
        assert_eq!(bulb.device_type(), "Router");
    }

    #[test]
    fn parse_ieee_valid() {
        let addr = parse_ieee("0xec1bbdfffeaa66db").unwrap();
        assert_eq!(addr.as_hex(), "0xec1bbdfffeaa66db");
    }

    #[test]
    fn parse_ieee_invalid() {
        assert!(parse_ieee("0x1234").is_none());
        assert!(parse_ieee("not_hex").is_none());
    }

    #[test]
    fn empty_database_returns_empty() {
        let dir = std::env::temp_dir().join(format!("z2m_empty_db_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.db");
        std::fs::write(&path, "").unwrap();
        let (devices, coord) = load_database(&path, &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        assert!(devices.is_empty());
        assert!(coord.is_none());
    }

    #[test]
    fn corrupted_lines_skipped() {
        let db = "not json\n{\"id\":1,\"type\":\"Router\",\"ieeeAddr\":\"0xec1bbdfffeaa66db\",\"nwkAddr\":100,\"endpoints\":{},\"interviewCompleted\":true}\n{broken";
        let dir = std::env::temp_dir().join(format!("z2m_corrupt_db_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("database.db");
        std::fs::write(&path, db).unwrap();
        let (devices, _) = load_database(&path, &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(devices.len(), 1); // only the valid line
    }
}
