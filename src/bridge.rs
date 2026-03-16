/// The main bridge – ties coordinator, MQTT, device registry, and ZCL together.
use std::sync::Arc;

use serde_json::json;
use tracing::{debug, error, info, warn};

use zigbee2mqtt_rs::config::Config;
use zigbee2mqtt_rs::coordinator::{open_coordinator, CoordinatorEvent, CoordinatorHandle};
use zigbee2mqtt_rs::devices::{Device, DeviceRegistry};
use zigbee2mqtt_rs::error::Result;
use zigbee2mqtt_rs::homeassistant;
use zigbee2mqtt_rs::mqtt::{MqttBridge, MqttCommand};
use zigbee2mqtt_rs::zigbee::zcl;
use zigbee2mqtt_rs::zigbee::zcl::clusters::color;
use zigbee2mqtt_rs::zigbee::zcl::clusters::level;
use zigbee2mqtt_rs::zigbee::zcl::clusters::on_off;
use zigbee2mqtt_rs::zigbee::{EndpointDesc, IeeeAddr};

pub struct Bridge {
    cfg: Config,
    devices: Arc<DeviceRegistry>,
}

impl Bridge {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            devices: Arc::new(DeviceRegistry::new()),
        }
    }

    pub async fn run(self) -> Result<()> {
        // 1. Connect MQTT
        let (mqtt, mut mqtt_rx) = MqttBridge::connect(&self.cfg.mqtt)?;
        mqtt.publish_bridge_state(true).await?;
        info!("MQTT bridge online");

        // 2. Open coordinator
        let mut coord = open_coordinator(&self.cfg).await?;
        info!("Coordinator ready");

        // 3. Publish bridge/info
        self.publish_bridge_info(&mqtt, &coord).await;

        // 4. Apply device configs (friendly names) from configuration
        self.apply_device_configs();

        // 5. Permit join if configured
        if self.cfg.permit_join {
            coord.permit_join(254).await?;
            info!("Permit join enabled (254 s)");
        }

        // 6. Publish current device list
        self.publish_device_list(&mqtt).await;

        let devices = Arc::clone(&self.devices);
        let base_topic = self.cfg.mqtt.base_topic.clone();
        let ha_enabled = self.cfg.homeassistant;
        let device_configs = self.cfg.devices.clone();
        let coordinator_ieee = coord
            .info
            .ieee_addr
            .map(|a| IeeeAddr(a).as_hex())
            .unwrap_or_default();

        // 7. Main event loop
        let mut trans_id: u8 = 0;

        loop {
            tokio::select! {
                // ── Coordinator events ────────────────────────────────────────
                event = coord.events.recv() => {
                    match event {
                        None => {
                            error!("Coordinator event channel closed");
                            break;
                        }
                        Some(CoordinatorEvent::DeviceJoined { ieee_addr, nwk_addr }) => {
                            let ieee = IeeeAddr(ieee_addr);
                            info!("Device joined: {ieee} (0x{nwk_addr:04X})");

                            if devices.get_by_ieee(&ieee).is_none() {
                                let mut dev = Device::new(ieee, nwk_addr);
                                // Apply friendly name from config
                                if let Some(cfg) = device_configs.get(&ieee.as_hex()) {
                                    if let Some(ref name) = cfg.friendly_name {
                                        dev.friendly_name = name.clone();
                                    }
                                    dev.disabled = cfg.disabled.unwrap_or(false);
                                }
                                devices.add(dev);
                            } else {
                                devices.update_nwk_addr(&ieee, nwk_addr);
                            }

                            mqtt.publish_bridge_log("info", &format!("Device joined: {ieee}")).await.ok();
                            coord.request_active_eps(nwk_addr).await.ok();
                        }

                        Some(CoordinatorEvent::DeviceLeft { ieee_addr, .. }) => {
                            let ieee = IeeeAddr(ieee_addr);
                            info!("Device left: {ieee}");
                            devices.remove_by_ieee(&ieee);
                            mqtt.publish_bridge_log("info", &format!("Device left: {ieee}")).await.ok();
                            Self::publish_device_list_static(&devices, &mqtt).await;
                        }

                        Some(CoordinatorEvent::AddressResolved { ieee_addr, nwk_addr }) => {
                            let ieee = IeeeAddr(ieee_addr);
                            debug!("Address resolved: {ieee} → 0x{nwk_addr:04X}");

                            let known = devices.get_by_ieee(&ieee).is_some();
                            if known {
                                info!("Linking {ieee} to NWK 0x{nwk_addr:04X}");
                                devices.update_nwk_addr(&ieee, nwk_addr);
                            } else {
                                // Unknown IEEE — create a new device entry
                                let mut dev = Device::new(ieee, nwk_addr);
                                if let Some(cfg) = device_configs.get(&ieee.as_hex()) {
                                    if let Some(ref name) = cfg.friendly_name {
                                        dev.friendly_name = name.clone();
                                    }
                                }
                                devices.add(dev);
                            }

                            // Trigger interview if not done yet
                            if devices.get_by_ieee(&ieee).map_or(false, |d| !d.interview_complete) {
                                coord.request_active_eps(nwk_addr).await.ok();
                            }
                        }

                        Some(CoordinatorEvent::ActiveEpRsp { nwk_addr, endpoints }) => {
                            debug!("Active EPs for 0x{nwk_addr:04X}: {endpoints:?}");
                            for ep in endpoints {
                                coord.request_simple_desc(nwk_addr, ep).await.ok();
                            }
                        }

                        Some(CoordinatorEvent::SimpleDescRsp {
                            nwk_addr, endpoint, profile_id, device_id,
                            input_clusters, output_clusters
                        }) => {
                            debug!("SimpleDesc 0x{nwk_addr:04X} ep={endpoint} clusters={input_clusters:?}");
                            let ep_desc = EndpointDesc {
                                endpoint,
                                profile_id,
                                device_id,
                                input_clusters: input_clusters.clone(),
                                output_clusters,
                            };

                            let mut interview_just_completed = false;
                            if let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) {
                                dev.endpoints.retain(|e| e.endpoint != endpoint);
                                dev.endpoints.push(ep_desc);
                                if !dev.interview_complete && !dev.endpoints.is_empty() {
                                    dev.interview_complete = true;
                                    interview_just_completed = true;
                                    info!("Interview complete for {}", dev.display_name());
                                }
                            }

                            // Request basic cluster attributes (manufacturer, model)
                            if input_clusters.contains(&0x0000) {
                                let payload = zigbee2mqtt_rs::zigbee::zcl::frame::read_attributes_payload(
                                    &[0x0004, 0x0005, 0x0007, 0x4000],
                                );
                                trans_id = trans_id.wrapping_add(1);
                                coord.send_zcl(nwk_addr, endpoint, 0x0000, trans_id, payload).await.ok();
                            }

                            Self::publish_device_list_static(&devices, &mqtt).await;

                            // Publish HA discovery after interview
                            if ha_enabled && interview_just_completed {
                                if let Some(dev) = devices.get_by_nwk(nwk_addr) {
                                    homeassistant::publish_discovery(&mqtt, &dev, &base_topic, &coordinator_ieee).await;
                                }
                            }
                        }

                        Some(CoordinatorEvent::Message {
                            src_addr, src_ep, cluster_id, link_quality, data
                        }) => {
                            debug!("AF msg from 0x{src_addr:04X} ep={src_ep} cluster=0x{cluster_id:04X} lqi={link_quality}");

                            // Unknown NWK? Request IEEE address to link it.
                            if devices.get_by_nwk(src_addr).is_none() {
                                debug!("Unknown NWK 0x{src_addr:04X}, requesting IEEE address");
                                coord.request_ieee_addr(src_addr).await.ok();
                            }

                            if cluster_id == 0x0000 {
                                Self::handle_basic_cluster_response(&devices, src_addr, &data);
                            }

                            match zcl::parse_message(cluster_id, &data) {
                                Ok(Some(zcl_msg)) => {
                                    if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
                                        dev.merge_state(zcl_msg.values.clone());
                                        dev.state.insert("linkquality".into(), json!(link_quality));
                                        dev.state.insert("last_seen".into(), json!(now_iso8601()));
                                        let state = serde_json::Value::Object(dev.state.clone());
                                        let name = dev.friendly_name.clone();
                                        drop(dev);
                                        mqtt.publish_device_state(&name, &state).await.ok();
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => warn!("ZCL parse error: {e}"),
                            }
                        }
                    }
                }

                // ── MQTT commands ─────────────────────────────────────────────
                cmd = mqtt_rx.recv() => {
                    match cmd {
                        None => break,
                        Some(MqttCommand::PermitJoin { duration }) => {
                            info!("Permit join: {duration}s");
                            coord.permit_join(duration).await.ok();
                            mqtt.publish_bridge_log("info", &format!("Permit join: {duration}s")).await.ok();
                        }
                        Some(MqttCommand::SetDevice { friendly_name, payload }) => {
                            Self::handle_set(
                                &devices, &coord, &mut trans_id,
                                &friendly_name, &payload,
                            ).await;
                        }
                        Some(MqttCommand::GetDevice { friendly_name, .. }) => {
                            if let Some(dev) = devices.find_by_name(&friendly_name) {
                                let state = serde_json::Value::Object(dev.state);
                                mqtt.publish_device_state(&friendly_name, &state).await.ok();
                            }
                        }
                    }
                }
            }
        }

        mqtt.publish_bridge_state(false).await.ok();
        Ok(())
    }

    /// Pre-seed the device registry from config so devices are findable by
    /// friendly name even before they re-announce on the network.
    fn apply_device_configs(&self) {
        for (ieee_str, cfg) in &self.cfg.devices {
            if let Some(ieee) = parse_ieee_addr(ieee_str) {
                let name = cfg
                    .friendly_name
                    .clone()
                    .unwrap_or_else(|| ieee.as_hex());
                let disabled = cfg.disabled.unwrap_or(false);

                if let Some(mut dev) = self.devices.get_mut_by_ieee(&ieee) {
                    dev.friendly_name = name;
                    dev.disabled = disabled;
                } else {
                    // Pre-seed: create a stub device with NWK=0 (updated on join)
                    let mut dev = Device::new(ieee, 0);
                    dev.friendly_name = name;
                    dev.disabled = disabled;
                    self.devices.add(dev);
                }
            }
        }
    }

    async fn publish_bridge_info(&self, mqtt: &MqttBridge, coord: &CoordinatorHandle) {
        let coord_ieee = coord
            .info
            .ieee_addr
            .map(|a| IeeeAddr(a).as_hex())
            .unwrap_or_default();

        let info = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "coordinator": {
                "ieee_address": coord_ieee,
                "type": "z-Stack",
                "meta": {
                    "revision": coord.info.transport_rev,
                    "version": coord.info.version,
                }
            },
            "log_level": self.cfg.advanced.log_level,
            "permit_join": self.cfg.permit_join,
            "config": {},
        });

        mqtt.publish_bridge_info(&info).await.ok();
    }

    async fn handle_set(
        devices: &DeviceRegistry,
        coord: &CoordinatorHandle,
        trans_id: &mut u8,
        name: &str,
        payload: &serde_json::Value,
    ) {
        let dev = match devices.find_by_name(name) {
            Some(d) => d,
            None => {
                warn!("Set command for unknown device: {name}");
                return;
            }
        };

        if dev.nwk_addr == 0 || !dev.interview_complete {
            warn!(
                "Device {name} not yet available on network (nwk=0x{:04X}, interviewed={})",
                dev.nwk_addr, dev.interview_complete
            );
            return;
        }

        let nwk_addr = dev.nwk_addr;
        let endpoints = &dev.endpoints;

        // Handle state (on/off)
        if let Some(state_val) = payload.get("state") {
            let state_str = state_val.as_str().unwrap_or("");
            if let Some(ep) = find_ep_with_cluster(endpoints, 0x0006) {
                if let Some(zcl_payload) = on_off::set_state_payload(*trans_id, state_str) {
                    *trans_id = trans_id.wrapping_add(1);
                    coord
                        .send_zcl(nwk_addr, ep, 0x0006, *trans_id, zcl_payload)
                        .await
                        .ok();
                }
            }
        }

        // Handle brightness
        if let Some(brightness) = payload.get("brightness").and_then(|v| v.as_u64()) {
            if let Some(ep) = find_ep_with_cluster(endpoints, 0x0008) {
                let lvl = brightness.min(254) as u8;
                let transition = transition_time(payload);
                let zcl_payload = level::move_to_level_payload(*trans_id, lvl, transition);
                *trans_id = trans_id.wrapping_add(1);
                coord
                    .send_zcl(nwk_addr, ep, 0x0008, *trans_id, zcl_payload)
                    .await
                    .ok();
            }
        }

        // Handle color_temp
        if let Some(ct) = payload.get("color_temp").and_then(|v| v.as_u64()) {
            if let Some(ep) = find_ep_with_cluster(endpoints, 0x0300) {
                let transition = transition_time(payload);
                let zcl_payload =
                    color::move_to_color_temp_payload(*trans_id, ct as u16, transition);
                *trans_id = trans_id.wrapping_add(1);
                coord
                    .send_zcl(nwk_addr, ep, 0x0300, *trans_id, zcl_payload)
                    .await
                    .ok();
            }
        }

        // Handle color object: {"color": {"x": 0.3, "y": 0.3}} or {"color": {"hue": 180, "saturation": 100}}
        if let Some(color_obj) = payload.get("color").and_then(|v| v.as_object()) {
            if let Some(ep) = find_ep_with_cluster(endpoints, 0x0300) {
                let transition = transition_time(payload);

                if let (Some(x), Some(y)) = (
                    color_obj.get("x").and_then(|v| v.as_f64()),
                    color_obj.get("y").and_then(|v| v.as_f64()),
                ) {
                    let zcl_payload = color::move_to_color_xy_payload(*trans_id, x, y, transition);
                    *trans_id = trans_id.wrapping_add(1);
                    coord
                        .send_zcl(nwk_addr, ep, 0x0300, *trans_id, zcl_payload)
                        .await
                        .ok();
                } else if let (Some(h), Some(s)) = (
                    color_obj.get("hue").and_then(|v| v.as_f64()),
                    color_obj.get("saturation").and_then(|v| v.as_f64()),
                ) {
                    // Convert from z2m 0-360/0-100 to ZCL 0-254
                    let zcl_hue = ((h / 360.0) * 254.0).round() as u8;
                    let zcl_sat = ((s / 100.0) * 254.0).round() as u8;
                    let zcl_payload =
                        color::move_to_hue_sat_payload(*trans_id, zcl_hue, zcl_sat, transition);
                    *trans_id = trans_id.wrapping_add(1);
                    coord
                        .send_zcl(nwk_addr, ep, 0x0300, *trans_id, zcl_payload)
                        .await
                        .ok();
                }
            }
        }
    }

    /// Handle basic cluster (0x0000) Read Attributes Response to update device metadata.
    fn handle_basic_cluster_response(
        devices: &DeviceRegistry,
        src_addr: u16,
        data: &[u8],
    ) {
        if let Ok(Some(zcl_msg)) = zcl::parse_message(0x0000, data) {
            if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
                for (key, value) in &zcl_msg.values {
                    match key.as_str() {
                        "manufacturer" => {
                            if let Some(s) = value.as_str() {
                                dev.manufacturer = Some(s.to_string());
                            }
                        }
                        "model" => {
                            if let Some(s) = value.as_str() {
                                dev.model = Some(s.to_string());
                            }
                        }
                        "power_source" => {
                            if let Some(s) = value.as_str() {
                                dev.power_source = Some(s.to_string());
                            }
                        }
                        "sw_build_id" => {
                            if let Some(s) = value.as_str() {
                                dev.sw_build_id = Some(s.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn publish_device_list(&self, mqtt: &MqttBridge) {
        Self::publish_device_list_static(&self.devices, mqtt).await;
    }

    async fn publish_device_list_static(devices: &DeviceRegistry, mqtt: &MqttBridge) {
        let list: Vec<_> = devices
            .all_devices()
            .iter()
            .map(|d| d.to_z2m_device_json())
            .collect();
        mqtt.publish_bridge_devices(&json!(list)).await.ok();
    }
}

fn find_ep_with_cluster(endpoints: &[EndpointDesc], cluster_id: u16) -> Option<u8> {
    endpoints
        .iter()
        .find(|e| e.input_clusters.contains(&cluster_id))
        .map(|e| e.endpoint)
}

fn transition_time(payload: &serde_json::Value) -> u16 {
    payload
        .get("transition")
        .and_then(|v| v.as_f64())
        .map(|s| (s * 10.0) as u16)
        .unwrap_or(0)
}

fn now_iso8601() -> String {
    // Simple UTC timestamp without external crate
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Convert to components
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since epoch to Y-M-D (simplified leap year handling)
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}+00:00")
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Parse an IEEE address string like "0xAABBCCDDEEFF0011" to IeeeAddr.
fn parse_ieee_addr(s: &str) -> Option<IeeeAddr> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ieee_addr_valid() {
        let addr = parse_ieee_addr("0x00158D0001020304").unwrap();
        assert_eq!(addr.as_hex(), "0x00158d0001020304");
    }

    #[test]
    fn parse_ieee_addr_lowercase() {
        let addr = parse_ieee_addr("0xec1bbdfffeaa66db").unwrap();
        assert_eq!(addr.as_hex().to_lowercase(), "0xec1bbdfffeaa66db");
    }

    #[test]
    fn parse_ieee_addr_invalid() {
        assert!(parse_ieee_addr("0x1234").is_none());
        assert!(parse_ieee_addr("not_hex").is_none());
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        // Should be like "2024-01-15T12:34:56+00:00"
        assert!(ts.contains('T'));
        assert!(ts.ends_with("+00:00"));
        assert_eq!(ts.len(), 25);
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-01-15 is 19737 days since epoch
        let (y, m, d) = days_to_ymd(19737);
        assert_eq!((y, m, d), (2024, 1, 15));
    }

    #[test]
    fn transition_time_from_payload() {
        let p = json!({"state": "ON", "transition": 1.5});
        assert_eq!(transition_time(&p), 15);
    }

    #[test]
    fn transition_time_default() {
        let p = json!({"state": "ON"});
        assert_eq!(transition_time(&p), 0);
    }

    #[test]
    fn find_ep_with_existing_cluster() {
        let eps = vec![
            EndpointDesc {
                endpoint: 1,
                profile_id: 0x0104,
                device_id: 0,
                input_clusters: vec![0x0000, 0x0006],
                output_clusters: vec![],
            },
            EndpointDesc {
                endpoint: 2,
                profile_id: 0x0104,
                device_id: 0,
                input_clusters: vec![0x0402],
                output_clusters: vec![],
            },
        ];
        assert_eq!(find_ep_with_cluster(&eps, 0x0006), Some(1));
        assert_eq!(find_ep_with_cluster(&eps, 0x0402), Some(2));
        assert_eq!(find_ep_with_cluster(&eps, 0x9999), None);
    }
}
