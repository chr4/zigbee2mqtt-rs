/// The main bridge – ties coordinator, MQTT, device registry, and ZCL together.
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::coordinator::{open_coordinator, CoordinatorEvent};
use crate::devices::{Device, DeviceRegistry};
use crate::error::Result;
use crate::mqtt::{MqttBridge, MqttCommand};
use crate::zigbee::zcl;
use crate::zigbee::zcl::clusters::on_off::set_state_payload;
use crate::zigbee::{EndpointDesc, IeeeAddr};

pub struct Bridge {
    cfg:      Config,
    devices:  Arc<DeviceRegistry>,
}

impl Bridge {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            devices: Arc::new(DeviceRegistry::new()),
        }
    }

    pub async fn run(self) -> Result<()> {
        // 1. Connect MQTT first so we can publish status ASAP
        let (mqtt, mut mqtt_rx) = MqttBridge::connect(&self.cfg.mqtt)?;
        mqtt.publish_bridge_state(true).await?;
        info!("MQTT bridge online");

        // 2. Open coordinator
        let mut coord = open_coordinator(&self.cfg).await?;
        info!("Coordinator ready");

        // 3. Permit join if configured
        if self.cfg.permit_join {
            coord.permit_join(254).await?;
            info!("Permit join enabled (254 s)");
        }

        // 4. Publish current device list
        self.publish_device_list(&mqtt).await;

        let devices = Arc::clone(&self.devices);
        let cfg     = self.cfg.clone();

        // 5. Main event loop
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
                                let dev = Device::new(ieee, nwk_addr);
                                devices.add(dev);
                            } else {
                                // Update NWK address if it changed
                                devices.update_nwk_addr(&ieee, nwk_addr);
                            }

                            mqtt.publish_bridge_log("info", &format!("Device joined: {ieee}")).await.ok();

                            // Start device interview
                            coord.request_active_eps(nwk_addr).await.ok();
                        }

                        Some(CoordinatorEvent::DeviceLeft { ieee_addr, nwk_addr }) => {
                            let ieee = IeeeAddr(ieee_addr);
                            info!("Device left: {ieee}");
                            devices.remove_by_ieee(&ieee);
                            mqtt.publish_bridge_log("info", &format!("Device left: {ieee}")).await.ok();
                            Self::publish_device_list_with(&devices, &mqtt).await;
                        }

                        Some(CoordinatorEvent::ActiveEpRsp { nwk_addr, endpoints }) => {
                            debug!("Active EPs for 0x{nwk_addr:04X}: {endpoints:?}");
                            for ep in endpoints {
                                coord.request_simple_desc(nwk_addr, ep).await.ok();
                            }
                        }

                        Some(CoordinatorEvent::SimpleDescRsp {
                            nwk_addr, endpoint, profile_id, input_clusters, output_clusters
                        }) => {
                            debug!("SimpleDesc 0x{nwk_addr:04X} ep={endpoint} clusters={input_clusters:?}");
                            let ep_desc = EndpointDesc {
                                endpoint,
                                profile_id,
                                device_id: 0,
                                input_clusters: input_clusters.clone(),
                                output_clusters,
                            };
                            if let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) {
                                dev.endpoints.retain(|e| e.endpoint != endpoint);
                                dev.endpoints.push(ep_desc);
                                if !dev.interview_complete && !dev.endpoints.is_empty() {
                                    dev.interview_complete = true;
                                    info!("Interview complete for {}", dev.display_name());
                                }
                            }
                            // Request basic cluster attributes (manufacturer, model)
                            if input_clusters.contains(&0x0000) {
                                let payload = crate::zigbee::zcl::frame::read_attributes_payload(
                                    &[0x0004, 0x0005, 0x0007],
                                );
                                trans_id = trans_id.wrapping_add(1);
                                coord.send_zcl(nwk_addr, endpoint, 0x0000, trans_id, payload).await.ok();
                            }

                            Self::publish_device_list_with(&devices, &mqtt).await;
                        }

                        Some(CoordinatorEvent::Message {
                            src_addr, src_ep, cluster_id, link_quality, data
                        }) => {
                            debug!("AF msg from 0x{src_addr:04X} ep={src_ep} cluster=0x{cluster_id:04X} lqi={link_quality}");
                            match zcl::parse_message(cluster_id, &data) {
                                Ok(Some(zcl_msg)) => {
                                    if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
                                        // Merge state and publish
                                        dev.merge_state(zcl_msg.values.clone());
                                        let state = serde_json::Value::Object(dev.state.clone());
                                        let name  = dev.friendly_name.clone();
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
                            // Return cached state by searching friendly names
                            let state_opt = devices.all_devices()
                                .into_iter()
                                .find(|d| d.friendly_name == friendly_name)
                                .map(|d| serde_json::Value::Object(d.state));
                            if let Some(state) = state_opt {
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

    async fn handle_set(
        devices:  &DeviceRegistry,
        coord:    &crate::coordinator::CoordinatorHandle,
        trans_id: &mut u8,
        name:     &str,
        payload:  &serde_json::Value,
    ) {
        // Find the device by friendly name
        let (nwk_addr, endpoints) = {
            // Search by friendly name
            let dev_opt = devices.all_devices().into_iter().find(|d| d.friendly_name == name);
            match dev_opt {
                None => {
                    warn!("Set command for unknown device: {name}");
                    return;
                }
                Some(dev) => (dev.nwk_addr, dev.endpoints.clone()),
            }
        };

        // Handle state (on/off)
        if let Some(state_val) = payload.get("state") {
            let state_str = state_val.as_str().unwrap_or("");
            // Find the endpoint with on/off cluster (0x0006)
            if let Some(ep) = endpoints.iter().find(|e| e.input_clusters.contains(&0x0006)) {
                if let Some(zcl_payload) = set_state_payload(*trans_id, state_str) {
                    *trans_id = trans_id.wrapping_add(1);
                    coord.send_zcl(nwk_addr, ep.endpoint, 0x0006, *trans_id, zcl_payload).await.ok();
                }
            }
        }

        // Handle brightness (level control)
        if let Some(brightness) = payload.get("brightness").and_then(|v| v.as_u64()) {
            if let Some(ep) = endpoints.iter().find(|e| e.input_clusters.contains(&0x0008)) {
                let level = brightness.min(254) as u8;
                let transition = payload.get("transition")
                    .and_then(|v| v.as_f64())
                    .map(|s| (s * 10.0) as u16)
                    .unwrap_or(0);
                let zcl_payload = crate::zigbee::zcl::clusters::level::move_to_level_payload(
                    *trans_id, level, transition,
                );
                *trans_id = trans_id.wrapping_add(1);
                coord.send_zcl(nwk_addr, ep.endpoint, 0x0008, *trans_id, zcl_payload).await.ok();
            }
        }
    }

    async fn publish_device_list(&self, mqtt: &MqttBridge) {
        Self::publish_device_list_with(&self.devices, mqtt).await;
    }

    async fn publish_device_list_with(devices: &DeviceRegistry, mqtt: &MqttBridge) {
        let list: Vec<_> = devices.all_devices().iter().map(|d| d.to_info_json()).collect();
        mqtt.publish_bridge_devices(&json!(list)).await.ok();
    }
}
