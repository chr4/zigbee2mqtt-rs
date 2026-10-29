/// The main bridge -- ties coordinator, MQTT, device registry, and ZCL together.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use std::path::PathBuf;

use crate::config::{Config, OutputMode};
use crate::coordinator::znp::ZnpHandle;
use crate::coordinator::{open_coordinator, CoordinatorEvent, CoordinatorHandle};
use crate::database;
use crate::devices::{Device, DeviceRegistry};
use crate::error::{Error, Result};
use crate::homeassistant;
use crate::mqtt::attribute_output::flatten_attribute_output;
use crate::mqtt::{MqttBridge, MqttCommand};
use crate::zigbee::zcl;
use crate::zigbee::zcl::clusters::color;
use crate::zigbee::zcl::clusters::ias_zone;
use crate::zigbee::zcl::clusters::level;
use crate::zigbee::zcl::clusters::on_off;
use crate::zigbee::{EndpointDesc, IeeeAddr};

/// How often the main loop runs housekeeping (permit-join refresh/expiry,
/// stalled-interview completion, rate-limiter pruning).
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(10);

/// An interview waiting on SimpleDescRsps is completed with whatever
/// endpoints arrived once this long has passed since the last ActiveEpRsp,
/// so one lost descriptor can't leave a device uninterviewed forever.
const INTERVIEW_TIMEOUT: Duration = Duration::from_secs(90);

/// Minimum gap between ZDO_IEEE_ADDR_REQs for the same unknown NWK address,
/// so a chatty device we can't identify yet doesn't flood the coordinator.
const IEEE_LOOKUP_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum permit-join duration the coordinator accepts in one request.
const PERMIT_JOIN_MAX: u8 = 254;

/// With `permit_join: true` in config, permit join is re-sent once this
/// little of the previous 254 s window is left. Mirrors zigbee-herdsman
/// re-sending every ~200 s (unverified -- from memory, not checked against
/// the herdsman source).
const PERMIT_JOIN_REFRESH_MARGIN: Duration = Duration::from_secs(54);

/// Output clusters bound to the coordinator after interview. Only command-
/// emitting clusters remote controls/switches use: binding *every* output
/// cluster (OTA, Time, Basic, ...) wastes the device's small binding table
/// and makes it unicast traffic we never handle.
const BINDABLE_OUTPUT_CLUSTERS: [u16; 4] = [0x0005, 0x0006, 0x0008, 0x0300];

pub struct Bridge {
    cfg: Config,
    config_path: PathBuf,
    devices: Arc<DeviceRegistry>,
}

impl Bridge {
    pub fn new(cfg: Config, config_path: PathBuf) -> Self {
        Self {
            cfg,
            config_path,
            devices: Arc::new(DeviceRegistry::new()),
        }
    }

    pub async fn run(self) -> Result<()> {
        // 1. Import device database from existing zigbee2mqtt install
        self.import_database();

        // 2. Apply device configs (friendly names override database)
        self.apply_device_configs();

        // 2b. Restore state.json and keep database.db/state.json up to date
        let store = Arc::new(crate::persistence::Store::new(&self.config_path));
        store.load_state(&self.devices);
        let _saver = store.spawn_saver(Arc::clone(&self.devices));

        // 3. Log device summary
        let all = self.devices.all_devices();
        info!(
            "Device registry: {} devices ({} interviewed)",
            all.len(),
            all.iter().filter(|d| d.interview_complete).count()
        );
        for dev in &all {
            info!(
                "  {} ({}) nwk=0x{:04X} eps={} interviewed={} model={:?}",
                dev.friendly_name,
                dev.ieee_addr,
                dev.nwk_addr,
                dev.endpoints.len(),
                dev.interview_complete,
                dev.model.as_deref().unwrap_or("-"),
            );
        }
        drop(all);

        // 4. Connect MQTT
        let (mqtt, mut mqtt_rx) = MqttBridge::connect(&self.cfg.mqtt).await?;
        info!("MQTT connected (base_topic={})", self.cfg.mqtt.base_topic);

        // 5. Open coordinator
        let mut coord = open_coordinator(&self.cfg).await?;
        info!("Coordinator ready");

        // 6. Permit join: enable if configured, otherwise explicitly disable --
        //    don't rely on whatever the coordinator's firmware defaults to.
        let mut permit_join = PermitJoin::default();
        if self.cfg.permit_join {
            coord.permit_join(PERMIT_JOIN_MAX).await?;
            permit_join.on_sent(PERMIT_JOIN_MAX, Instant::now());
            permit_join.keep_enabled = true;
            info!("Permit join enabled (kept open while permit_join: true)");
        } else if let Err(e) = coord.permit_join(0).await {
            warn!("Failed to disable permit join: {e}");
        }

        // 7. Publish retained MQTT state
        self.republish_mqtt_state(&mqtt, &coord, permit_join.enabled(Instant::now()))
            .await;
        info!("Startup complete, entering main loop");

        let devices = Arc::clone(&self.devices);
        let base_topic = self.cfg.mqtt.base_topic.clone();
        let ha_enabled = self.cfg.homeassistant;
        let device_configs = self.cfg.devices.clone();
        let coordinator_ieee = coord.info.ieee_addr.map(|a| a.as_hex()).unwrap_or_default();

        // 8. Main event loop
        let trans_id = Arc::new(AtomicU8::new(0));
        let mut interviews = InterviewTracker::default();
        let mut ieee_lookups = LookupLimiter::default();
        let mut housekeeping = tokio::time::interval(HOUSEKEEPING_INTERVAL);
        housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        let result: Result<()> = loop {
            tokio::select! {
                // -- Coordinator events --
                event = coord.events.recv() => {
                    match event {
                        None => {
                            // The coordinator layer closes this channel on any
                            // fatal condition (serial error, unexpected reset,
                            // unresponsive adapter). Exit non-zero so the
                            // service manager restarts and re-initialises us.
                            error!("Coordinator event channel closed");
                            break Err(Error::Znp("coordinator connection lost".into()));
                        }
                        Some(CoordinatorEvent::DeviceJoined { ieee_addr, nwk_addr }) => {
                            let ieee = ieee_addr;
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

                            if let Err(e) = mqtt.publish_bridge_log("info", &format!("Device joined: {ieee}")).await {
                                warn!("Failed to publish bridge log: {e}");
                            }
                            Self::spawn_interview_retry(Arc::clone(&devices), Arc::clone(&coord.inner), ieee, nwk_addr);
                        }

                        Some(CoordinatorEvent::DeviceLeft { ieee_addr, .. }) => {
                            let ieee = ieee_addr;
                            info!("Device left: {ieee}");
                            if let Some(nwk_addr) = devices.get_by_ieee(&ieee).map(|d| d.nwk_addr) {
                                interviews.forget(nwk_addr);
                            }
                            devices.remove_by_ieee(&ieee);
                            if let Err(e) = mqtt.publish_bridge_log("info", &format!("Device left: {ieee}")).await {
                                warn!("Failed to publish bridge log: {e}");
                            }
                            Self::publish_device_list_static(&devices, &mqtt).await;
                        }

                        Some(CoordinatorEvent::AddressResolved { ieee_addr, nwk_addr }) => {
                            let ieee = ieee_addr;
                            debug!("Address resolved: {ieee} -> 0x{nwk_addr:04X}");

                            let known = devices.get_by_ieee(&ieee).is_some();
                            if known {
                                info!("Linking {ieee} to NWK 0x{nwk_addr:04X}");
                                devices.update_nwk_addr(&ieee, nwk_addr);
                            } else {
                                // Unknown IEEE -- create a new device entry
                                let mut dev = Device::new(ieee, nwk_addr);
                                if let Some(cfg) = device_configs.get(&ieee.as_hex()) {
                                    if let Some(ref name) = cfg.friendly_name {
                                        dev.friendly_name = name.clone();
                                    }
                                    dev.disabled = cfg.disabled.unwrap_or(false);
                                }
                                devices.add(dev);
                            }

                            // Trigger interview if not done yet
                            if devices.get_by_ieee(&ieee).is_some_and(|d| !d.interview_complete) {
                                Self::spawn_interview_retry(Arc::clone(&devices), Arc::clone(&coord.inner), ieee, nwk_addr);
                            }
                        }

                        Some(CoordinatorEvent::ActiveEpRsp { nwk_addr, endpoints }) => {
                            debug!("Active EPs for 0x{nwk_addr:04X}: {endpoints:?}");
                            let missing = interviews.expect(nwk_addr, &endpoints, Instant::now());
                            Self::spawn_simple_desc_requests(Arc::clone(&coord.inner), nwk_addr, missing);
                        }

                        Some(CoordinatorEvent::BindRsp { nwk_addr, status }) => {
                            if status == 0 {
                                debug!("Bind succeeded for 0x{nwk_addr:04X}");
                            } else {
                                warn!("Bind failed for 0x{nwk_addr:04X}: status=0x{status:02X}");
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
                                input_clusters,
                                output_clusters,
                            };

                            let mut src_ieee: Option<IeeeAddr> = None;
                            if let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) {
                                dev.endpoints.retain(|e| e.endpoint != endpoint);
                                dev.endpoints.push(ep_desc.clone());
                                src_ieee = Some(dev.ieee_addr);
                            }

                            // Only complete the interview once every endpoint
                            // from ActiveEpRsp has been described, so HA
                            // discovery isn't published from a partial set.
                            if interviews.record(nwk_addr, endpoint) {
                                Self::complete_interview(
                                    &devices, &mqtt, nwk_addr, ha_enabled, &base_topic, &coordinator_ieee,
                                ).await;
                            }

                            if let Some(src_ieee) = src_ieee {
                                Self::spawn_endpoint_setup(
                                    Arc::clone(&coord.inner),
                                    Arc::clone(&trans_id),
                                    nwk_addr,
                                    src_ieee,
                                    coord.info.ieee_addr,
                                    ep_desc,
                                );
                            }

                            Self::publish_device_list_static(&devices, &mqtt).await;
                        }

                        Some(CoordinatorEvent::Message {
                            src_addr, src_ep, cluster_id, link_quality, data
                        }) => {
                            debug!("AF msg from 0x{src_addr:04X} ep={src_ep} cluster=0x{cluster_id:04X} lqi={link_quality}");

                            // Unknown NWK? Request IEEE address to link it.
                            if devices.get_by_nwk(src_addr).is_none()
                                && ieee_lookups.allow(src_addr, Instant::now())
                            {
                                debug!("Unknown NWK 0x{src_addr:04X}, requesting IEEE address");
                                Self::spawn_ieee_lookup(Arc::clone(&coord.inner), src_addr);
                            }

                            if cluster_id == 0x0000 {
                                Self::handle_basic_cluster_response(&devices, src_addr, &data);
                            }
                            if cluster_id == 0x0500 {
                                Self::handle_ias_zone_type_response(&devices, src_addr, &data);
                                if zcl::is_ias_enroll_request(&data) {
                                    info!("IAS zone enroll request from 0x{src_addr:04X}, enrolling");
                                    let znp = Arc::clone(&coord.inner);
                                    let trans_id = Arc::clone(&trans_id);
                                    tokio::spawn(async move {
                                        Self::send_ias_enroll_response(&znp, &trans_id, src_addr, src_ep).await;
                                    });
                                }
                            }

                            let zone_type = devices.get_by_nwk(src_addr).and_then(|d| d.zone_type);
                            let held_arrow_direction = devices
                                .get_by_nwk(src_addr)
                                .and_then(|d| d.arrow_hold_direction.clone());
                            match zcl::parse_message(cluster_id, &data, zone_type, held_arrow_direction.as_deref()) {
                                Ok(Some(mut zcl_msg)) => {
                                    if cluster_id == 0x0000 {
                                        // Basic cluster attributes (manufacturer/model/
                                        // power_source/sw_build_id) are device metadata, not
                                        // volatile state -- z2m doesn't publish them on the
                                        // device state topic. handle_basic_cluster_response()
                                        // above already routed them onto dedicated Device
                                        // fields; strip them here so they aren't also merged
                                        // into (and published as part of) device state.
                                        for key in BASIC_CLUSTER_METADATA_KEYS {
                                            zcl_msg.values.remove(key);
                                        }
                                    }
                                    if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
                                        if let Some(direction_update) = zcl_msg.arrow_hold_direction {
                                            dev.arrow_hold_direction = direction_update;
                                        }
                                        dev.merge_state(zcl_msg.values.clone());
                                        dev.state.insert("linkquality".into(), json!(link_quality));
                                        dev.state.insert("last_seen".into(), json!(now_iso8601()));
                                        let state = serde_json::Value::Object(dev.state.clone());
                                        let name = dev.friendly_name.clone();
                                        // `action`/`action_*` are momentary button-press events,
                                        // not retained device state -- matching zigbee2mqtt,
                                        // don't let them linger and pollute later unrelated
                                        // publishes for this device (they're still included in
                                        // `state`, built just above, for this one publish).
                                        Self::strip_transient_action_fields(&mut dev.state);
                                        drop(dev);
                                        Self::publish_state(&mqtt, self.cfg.advanced.output, &name, &state).await;
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => warn!("ZCL parse error: {e}"),
                            }
                        }
                    }
                }

                // -- MQTT commands --
                cmd = mqtt_rx.recv() => {
                    match cmd {
                        None => {
                            error!("MQTT command channel closed");
                            break Err(Error::ChannelClosed);
                        }
                        Some(MqttCommand::Connected) => {
                            info!("MQTT session restored, republishing retained state");
                            self.republish_mqtt_state(&mqtt, &coord, permit_join.enabled(Instant::now()))
                                .await;
                        }
                        Some(MqttCommand::PermitJoin { duration }) => {
                            info!("Permit join: {duration}s");
                            match coord.permit_join(duration).await {
                                Ok(()) => {
                                    permit_join.on_sent(duration, Instant::now());
                                    // An explicit disable also stops the config-driven
                                    // keep-open refresh; otherwise it'd re-enable itself
                                    // within a minute.
                                    if duration == 0 {
                                        permit_join.keep_enabled = false;
                                    }
                                    self.publish_bridge_info(&mqtt, &coord, permit_join.enabled(Instant::now()))
                                        .await;
                                }
                                Err(e) => warn!("Failed to set permit join: {e}"),
                            }
                            if let Err(e) = mqtt.publish_bridge_log("info", &format!("Permit join: {duration}s")).await {
                                warn!("Failed to publish bridge log: {e}");
                            }
                        }
                        Some(MqttCommand::SetDevice { friendly_name, payload }) => {
                            info!("MQTT set: {friendly_name} -> {payload}");
                            Self::handle_set(
                                &devices, &coord, &mqtt, self.cfg.advanced.output, &trans_id,
                                &friendly_name, &payload,
                            ).await;
                        }
                        Some(MqttCommand::GetDevice { friendly_name, .. }) => {
                            debug!("MQTT get: {friendly_name}");
                            if let Some(dev) = devices.find_by_name(&friendly_name) {
                                let state = serde_json::Value::Object(dev.state);
                                Self::publish_state(&mqtt, self.cfg.advanced.output, &friendly_name, &state).await;
                            } else {
                                warn!("Get command for unknown device: {friendly_name}");
                            }
                        }
                    }
                }

                // -- Housekeeping --
                _ = housekeeping.tick() => {
                    let now = Instant::now();

                    if permit_join.needs_refresh(now) {
                        let was_enabled = permit_join.enabled(now);
                        match coord.permit_join(PERMIT_JOIN_MAX).await {
                            Ok(()) => {
                                permit_join.on_sent(PERMIT_JOIN_MAX, now);
                                if !was_enabled {
                                    self.publish_bridge_info(&mqtt, &coord, true).await;
                                }
                            }
                            Err(e) => warn!("Failed to refresh permit join: {e}"),
                        }
                    }
                    if permit_join.take_expired(now) {
                        info!("Permit join expired");
                        self.publish_bridge_info(&mqtt, &coord, false).await;
                    }

                    let stalled = interviews.expire(now);
                    for &nwk_addr in &stalled {
                        warn!("Interview for 0x{nwk_addr:04X} timed out waiting for some endpoints, completing with those received");
                        Self::complete_interview(
                            &devices, &mqtt, nwk_addr, ha_enabled, &base_topic, &coordinator_ieee,
                        ).await;
                    }
                    if !stalled.is_empty() {
                        Self::publish_device_list_static(&devices, &mqtt).await;
                    }

                    ieee_lookups.prune(now);
                }

                // -- Shutdown --
                _ = &mut shutdown => {
                    info!("Shutdown signal received");
                    break Ok(());
                }
            }
        };

        if let Err(e) = mqtt.publish_bridge_state(false).await {
            warn!("Failed to publish offline state: {e}");
        }
        store.save(&self.devices);
        result
    }

    /// Import devices from an existing zigbee2mqtt database.db file.
    fn import_database(&self) {
        if let Some(db_path) = database::find_database(&self.config_path) {
            let (devices, _coord_ieee) = database::load_database(&db_path, &self.cfg.devices);
            for dev in devices {
                self.devices.add(dev);
            }
        }
    }

    /// Apply friendly names from config, overriding database values.
    /// Also pre-seeds stub entries for devices in config but not in database.
    fn apply_device_configs(&self) {
        for (ieee_str, cfg) in &self.cfg.devices {
            if let Some(ieee) = IeeeAddr::from_hex(ieee_str) {
                let name = cfg.friendly_name.clone().unwrap_or_else(|| ieee.as_hex());
                let disabled = cfg.disabled.unwrap_or(false);

                if let Some(mut dev) = self.devices.get_mut_by_ieee(&ieee) {
                    dev.disabled = disabled;
                    drop(dev);
                    self.devices.rename(&ieee, name);
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

    async fn publish_bridge_info(
        &self,
        mqtt: &MqttBridge,
        coord: &CoordinatorHandle,
        permit_join: bool,
    ) {
        let coord_ieee = coord.info.ieee_addr.map(|a| a.as_hex()).unwrap_or_default();

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
            "permit_join": permit_join,
            "config": {},
        });

        if let Err(e) = mqtt.publish_bridge_info(&info).await {
            warn!("Failed to publish bridge info: {e}");
        }
    }

    async fn republish_mqtt_state(
        &self,
        mqtt: &MqttBridge,
        coord: &CoordinatorHandle,
        permit_join: bool,
    ) {
        if let Err(e) = mqtt.publish_bridge_state(true).await {
            warn!("Failed to publish online state: {e}");
        }

        self.publish_bridge_info(mqtt, coord, permit_join).await;

        let coordinator_ieee_str = coord.info.ieee_addr.map(|a| a.as_hex()).unwrap_or_default();

        for dev in self.devices.all_devices() {
            // Nothing known yet (e.g. just after a restart): publishing `{}`
            // retained would wipe the broker's last real state for HA.
            if !dev.state.is_empty() {
                let state = serde_json::Value::Object(dev.state.clone());
                Self::publish_state(mqtt, self.cfg.advanced.output, &dev.friendly_name, &state)
                    .await;
            }

            if self.cfg.homeassistant && dev.interview_complete {
                homeassistant::publish_discovery(
                    mqtt,
                    &dev,
                    &self.cfg.mqtt.base_topic,
                    &coordinator_ieee_str,
                )
                .await;
            }
        }

        self.publish_device_list(mqtt).await;
    }

    async fn handle_set(
        devices: &DeviceRegistry,
        coord: &CoordinatorHandle,
        mqtt: &MqttBridge,
        output: OutputMode,
        trans_id: &AtomicU8,
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
        let endpoints = dev.endpoints.clone();
        let current_state = dev.state.get("state").and_then(|v| v.as_str());

        // Build optimistic state from the command payload. Only commands the
        // coordinator actually accepted contribute to it.
        let mut optimistic = serde_json::Map::new();

        // Handle state (on/off)
        if let Some(state_val) = payload.get("state") {
            let state_str = state_val.as_str().unwrap_or("");
            if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0006) {
                let tid = next_trans_id(trans_id);
                if let Some(zcl_payload) = on_off::set_state_payload(tid, state_str) {
                    if Self::send_command(coord, nwk_addr, ep, 0x0006, tid, zcl_payload, "on/off")
                        .await
                    {
                        if let Some(s) = optimistic_on_off(state_str, current_state) {
                            optimistic.insert("state".into(), json!(s));
                        }
                    }
                }
            }
        }

        // Handle brightness
        if let Some(brightness) = payload.get("brightness").and_then(|v| v.as_u64()) {
            if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0008) {
                let lvl = brightness.min(254) as u8;
                let transition = transition_time(payload);
                let tid = next_trans_id(trans_id);
                let zcl_payload = level::move_to_level_payload(tid, lvl, transition);
                if Self::send_command(coord, nwk_addr, ep, 0x0008, tid, zcl_payload, "level").await
                {
                    optimistic.insert("brightness".into(), json!(lvl));
                    // Move-to-level-with-on/off: level 0 turns the light off,
                    // anything else turns it on.
                    if !optimistic.contains_key("state") {
                        optimistic
                            .insert("state".into(), json!(if lvl == 0 { "OFF" } else { "ON" }));
                    }
                }
            }
        }

        // Handle color_temp
        if let Some(ct) = payload.get("color_temp").and_then(|v| v.as_u64()) {
            if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0300) {
                let ct = ct.min(u64::from(u16::MAX)) as u16;
                let transition = transition_time(payload);
                let tid = next_trans_id(trans_id);
                let zcl_payload = color::move_to_color_temp_payload(tid, ct, transition);
                if Self::send_command(coord, nwk_addr, ep, 0x0300, tid, zcl_payload, "color temp")
                    .await
                {
                    optimistic.insert("color_temp".into(), json!(ct));
                    optimistic.insert("color_mode".into(), json!("color_temp"));
                }
            }
        }

        // Handle color object
        if let Some(color_obj) = payload.get("color").and_then(|v| v.as_object()) {
            if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0300) {
                let transition = transition_time(payload);

                if let (Some(x), Some(y)) = (
                    color_obj.get("x").and_then(|v| v.as_f64()),
                    color_obj.get("y").and_then(|v| v.as_f64()),
                ) {
                    let tid = next_trans_id(trans_id);
                    let zcl_payload = color::move_to_color_xy_payload(tid, x, y, transition);
                    if Self::send_command(coord, nwk_addr, ep, 0x0300, tid, zcl_payload, "color XY")
                        .await
                    {
                        optimistic.insert("color".into(), json!({"x": x, "y": y}));
                        optimistic.insert("color_mode".into(), json!("xy"));
                    }
                } else if let (Some(h), Some(s)) = (
                    color_obj.get("hue").and_then(|v| v.as_f64()),
                    color_obj.get("saturation").and_then(|v| v.as_f64()),
                ) {
                    let zcl_hue = ((h.rem_euclid(360.0) / 360.0) * 254.0).round() as u8;
                    let zcl_sat = ((s.clamp(0.0, 100.0) / 100.0) * 254.0).round() as u8;
                    let tid = next_trans_id(trans_id);
                    let zcl_payload =
                        color::move_to_hue_sat_payload(tid, zcl_hue, zcl_sat, transition);
                    if Self::send_command(coord, nwk_addr, ep, 0x0300, tid, zcl_payload, "color HS")
                        .await
                    {
                        optimistic.insert("color".into(), json!({"hue": h, "saturation": s}));
                        optimistic.insert("color_mode".into(), json!("hs"));
                    }
                }
            }
        }

        // Publish optimistic state immediately (z2m behavior). `last_seen` is
        // left alone: it tracks traffic *from* the device, not commands to it.
        if !optimistic.is_empty() {
            // Merge with existing device state
            if let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) {
                dev.merge_state(optimistic.clone());
            }
            // Publish the full merged state
            if let Some(dev) = devices.find_by_name(name) {
                let full_state = serde_json::Value::Object(dev.state);
                Self::publish_state(mqtt, output, name, &full_state).await;
            }
        }
    }

    /// Send one ZCL command for `handle_set`, using `trans_id` as both the ZCL
    /// sequence number (already baked into `payload`) and the AF transaction
    /// ID. Returns whether the coordinator accepted it.
    #[allow(clippy::too_many_arguments)]
    async fn send_command(
        coord: &CoordinatorHandle,
        nwk_addr: u16,
        ep: u8,
        cluster_id: u16,
        trans_id: u8,
        payload: Vec<u8>,
        what: &str,
    ) -> bool {
        match coord
            .send_zcl(nwk_addr, ep, cluster_id, trans_id, payload)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!("Failed to send {what} command: {e}");
                false
            }
        }
    }

    /// Remove momentary button-press fields (`action` and `action_*`) from a
    /// device's *retained* state. Real device state (brightness, battery,
    /// contact, ...) should keep accumulating via `merge_state`, but an
    /// `action` is a one-off event -- without this, a stale `action` (and
    /// its companion `action_*` keys) from one button press would linger and
    /// get republished, mixed in, on every later unrelated event for that
    /// device. Call this *after* building the JSON to publish for the
    /// current message, so the action still appears in that one publish.
    fn strip_transient_action_fields(state: &mut serde_json::Map<String, serde_json::Value>) {
        state.retain(|k, _| k != "action" && !k.starts_with("action_"));
    }

    /// Publish a device's state per `advanced.output` (z2m's
    /// `Controller.publishEntityState`): the merged JSON message, each state
    /// key as its own flattened raw-value subtopic, or both. All state
    /// publishes should go through this rather than calling
    /// `mqtt.publish_device_state` directly, so every call site respects the
    /// configured output mode uniformly.
    async fn publish_state(
        mqtt: &MqttBridge,
        output: OutputMode,
        friendly_name: &str,
        state: &serde_json::Value,
    ) {
        let (publish_json, attributes) = Self::plan_state_publishes(output, state);

        if publish_json {
            if let Err(e) = mqtt.publish_device_state(friendly_name, state).await {
                warn!("Failed to publish device state: {e}");
            }
        }
        for (key_path, value) in attributes {
            if let Err(e) = mqtt
                .publish_device_attribute(friendly_name, &key_path, &value)
                .await
            {
                warn!("Failed to publish attribute {key_path} for {friendly_name}: {e}");
            }
        }
    }

    /// Decides *what* `publish_state` should publish for a given output mode,
    /// split out from the actual MQTT I/O above purely so this branching is
    /// unit-testable without a live broker connection: returns whether the
    /// merged-JSON message should be published, and the flattened
    /// `(key_path, value)` attribute pairs (empty unless `output` is
    /// `Attribute`/`AttributeAndJson`).
    fn plan_state_publishes(
        output: OutputMode,
        state: &serde_json::Value,
    ) -> (bool, Vec<(String, String)>) {
        let publish_json = matches!(output, OutputMode::Json | OutputMode::AttributeAndJson);
        let attributes = if matches!(output, OutputMode::Attribute | OutputMode::AttributeAndJson) {
            state
                .as_object()
                .map(flatten_attribute_output)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        (publish_json, attributes)
    }

    /// Handle basic cluster (0x0000) Read Attributes Response to update device metadata.
    fn handle_basic_cluster_response(devices: &DeviceRegistry, src_addr: u16, data: &[u8]) {
        if let Ok(Some(zcl_msg)) = zcl::parse_message(0x0000, data, None, None) {
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

    /// Handle IAS Zone (0x0500) Read Attributes Response to learn ZoneType,
    /// which determines how zone status reports are classified (contact,
    /// occupancy, smoke, water_leak, ...). No-op for any other ZCL command.
    fn handle_ias_zone_type_response(devices: &DeviceRegistry, src_addr: u16, data: &[u8]) {
        if let Ok(Some(zone_type)) = zcl::extract_ias_zone_type(data) {
            if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
                dev.zone_type = Some(zone_type);
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
        if let Err(e) = mqtt.publish_bridge_devices(&json!(list)).await {
            warn!("Failed to publish device list: {e}");
        }
    }

    /// Spawn a background retry loop for a device's ZDO interview.
    ///
    /// A sleepy battery-powered end device (e.g. a remote control) may be
    /// asleep when the initial ZDO_ACTIVE_EP_REQ goes out over the air and
    /// simply never see it -- the SREQ/SRSP exchange with the coordinator's
    /// own radio still succeeds (it only confirms the request was queued),
    /// so nothing here times out on its own. Without a retry, the device's
    /// `interview_complete` flag would then never flip true. This polls
    /// `interview_complete` and re-sends the request a bounded number of
    /// times with a delay generous enough for such devices to wake up and
    /// respond, stopping as soon as the real ActiveEpRsp/SimpleDescRsp event
    /// handling (in the main loop) marks the interview complete.
    fn spawn_interview_retry(
        devices: Arc<DeviceRegistry>,
        znp: Arc<Mutex<ZnpHandle>>,
        ieee: IeeeAddr,
        nwk_addr: u16,
    ) {
        const MAX_ATTEMPTS: u32 = 3;
        const RETRY_DELAY: Duration = Duration::from_secs(20);

        tokio::spawn(async move {
            for attempt in 1..=MAX_ATTEMPTS {
                if devices
                    .get_by_ieee(&ieee)
                    .is_some_and(|d| d.interview_complete)
                {
                    return;
                }
                if let Err(e) = znp.lock().await.request_active_eps(nwk_addr).await {
                    warn!(
                        "Interview attempt {attempt}/{MAX_ATTEMPTS} for {ieee}: \
                         failed to request active endpoints: {e}"
                    );
                }
                tokio::time::sleep(RETRY_DELAY).await;
                if devices
                    .get_by_ieee(&ieee)
                    .is_some_and(|d| d.interview_complete)
                {
                    return;
                }
            }
            warn!(
                "Interview for {ieee} did not complete after {MAX_ATTEMPTS} attempts -- \
                 device may be asleep or out of range. It will retry automatically on its \
                 next join/announce."
            );
        });
    }

    /// Request simple descriptors for `endpoints` off the main loop: each is
    /// a coordinator round-trip (up to the 10 s SRSP timeout if the adapter
    /// is struggling), and the main loop must keep draining events meanwhile.
    fn spawn_simple_desc_requests(znp: Arc<Mutex<ZnpHandle>>, nwk_addr: u16, endpoints: Vec<u8>) {
        if endpoints.is_empty() {
            return;
        }
        tokio::spawn(async move {
            for ep in endpoints {
                if let Err(e) = znp.lock().await.request_simple_desc(nwk_addr, ep).await {
                    warn!("Failed to request simple descriptor for 0x{nwk_addr:04X} ep={ep}: {e}");
                }
            }
        });
    }

    /// Resolve an unknown NWK address to its IEEE address off the main loop.
    /// The answer arrives as a `CoordinatorEvent::AddressResolved`.
    fn spawn_ieee_lookup(znp: Arc<Mutex<ZnpHandle>>, nwk_addr: u16) {
        tokio::spawn(async move {
            if let Err(e) = znp.lock().await.request_ieee_addr(nwk_addr).await {
                warn!("Failed to request IEEE address for 0x{nwk_addr:04X}: {e}");
            }
        });
    }

    /// Per-endpoint setup after its simple descriptor arrived, off the main
    /// loop: bind command-emitting output clusters to the coordinator, and
    /// read the attributes needed to describe the device.
    ///
    /// Binding matters for devices whose relevant clusters are *output*
    /// clusters (e.g. remote controls -- button presses are genOnOff/
    /// genLevelCtrl/genScenes commands): without a binding to unicast to,
    /// such devices join fine but produce no Zigbee traffic at all when used.
    fn spawn_endpoint_setup(
        znp: Arc<Mutex<ZnpHandle>>,
        trans_id: Arc<AtomicU8>,
        nwk_addr: u16,
        src_ieee: IeeeAddr,
        coordinator_ieee: Option<IeeeAddr>,
        ep: EndpointDesc,
    ) {
        tokio::spawn(async move {
            let endpoint = ep.endpoint;
            let bindable: Vec<u16> = ep
                .output_clusters
                .iter()
                .copied()
                .filter(|c| BINDABLE_OUTPUT_CLUSTERS.contains(c))
                .collect();
            match coordinator_ieee {
                Some(dst_ieee) => {
                    for cluster_id in bindable {
                        let result = znp
                            .lock()
                            .await
                            .request_bind(nwk_addr, src_ieee.0, endpoint, cluster_id, dst_ieee.0, 1)
                            .await;
                        if let Err(e) = result {
                            warn!("Failed to request bind for 0x{nwk_addr:04X} ep={endpoint} cluster=0x{cluster_id:04X}: {e}");
                        }
                    }
                }
                None if !bindable.is_empty() => {
                    warn!("Cannot bind 0x{nwk_addr:04X} ep={endpoint}: coordinator IEEE address unknown");
                }
                None => {}
            }

            // IAS Zone enrollment: point the device at us as its CIE, then
            // send an unsolicited Zone Enroll Response (devices that instead
            // send a Zone Enroll Request are answered from the main loop).
            if ep.input_clusters.contains(&0x0500) {
                match coordinator_ieee {
                    Some(cie) => {
                        let tid = next_trans_id(&trans_id);
                        let payload = ias_zone::write_cie_address_payload(tid, cie.0);
                        let result = znp
                            .lock()
                            .await
                            .send_zcl(nwk_addr, endpoint, 0x0500, tid, payload)
                            .await;
                        if let Err(e) = result {
                            warn!("Failed to write IAS CIE address to 0x{nwk_addr:04X}: {e}");
                        }
                        Self::send_ias_enroll_response(&znp, &trans_id, nwk_addr, endpoint).await;
                    }
                    None => warn!("Cannot enroll IAS zone 0x{nwk_addr:04X}: coordinator IEEE address unknown"),
                }
            }

            // Basic: manufacturer, model, power source, sw build id.
            // IAS Zone: ZoneType, so zone status reports can be classified
            //   (contact/occupancy/smoke/water_leak/...) instead of always
            //   being assumed to be a door/contact sensor.
            // Power Configuration: battery voltage/percentage, which most
            //   devices never report unsolicited without being asked.
            let reads: [(u16, &[u16]); 3] = [
                (0x0000, &[0x0004, 0x0005, 0x0007, 0x4000]),
                (0x0500, &[0x0001]),
                (0x0001, &[0x0020, 0x0021]),
            ];
            for (cluster_id, attrs) in reads {
                if !ep.input_clusters.contains(&cluster_id) {
                    continue;
                }
                let tid = next_trans_id(&trans_id);
                let payload = crate::zigbee::zcl::frame::read_attributes_payload(tid, attrs);
                let result = znp
                    .lock()
                    .await
                    .send_zcl(nwk_addr, endpoint, cluster_id, tid, payload)
                    .await;
                if let Err(e) = result {
                    warn!("Failed to send ZCL read attributes (cluster 0x{cluster_id:04X}) to 0x{nwk_addr:04X}: {e}");
                }
            }
        });
    }

    async fn send_ias_enroll_response(
        znp: &Mutex<ZnpHandle>,
        trans_id: &AtomicU8,
        nwk_addr: u16,
        endpoint: u8,
    ) {
        let tid = next_trans_id(trans_id);
        let payload = ias_zone::enroll_response_payload(tid, ias_zone::DEFAULT_ZONE_ID);
        let result = znp
            .lock()
            .await
            .send_zcl(nwk_addr, endpoint, 0x0500, tid, payload)
            .await;
        if let Err(e) = result {
            warn!("Failed to send IAS zone enroll response to 0x{nwk_addr:04X}: {e}");
        }
    }

    /// Mark the device at `nwk_addr` interviewed (if it isn't already and has
    /// at least one described endpoint) and publish its HA discovery.
    async fn complete_interview(
        devices: &DeviceRegistry,
        mqtt: &MqttBridge,
        nwk_addr: u16,
        ha_enabled: bool,
        base_topic: &str,
        coordinator_ieee: &str,
    ) {
        let dev = {
            let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) else {
                return;
            };
            if dev.interview_complete || dev.endpoints.is_empty() {
                return;
            }
            dev.interview_complete = true;
            info!(
                "Interview complete for {} ({} endpoints)",
                dev.display_name(),
                dev.endpoints.len()
            );
            dev.clone()
        };
        if ha_enabled {
            homeassistant::publish_discovery(mqtt, &dev, base_topic, coordinator_ieee).await;
        }
    }
}

/// Allocate the next ZCL sequence number / AF transaction ID. Shared between
/// the main loop and spawned setup tasks, hence atomic.
fn next_trans_id(counter: &AtomicU8) -> u8 {
    counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

/// The `state` to publish optimistically for an on/off `command`. TOGGLE
/// inverts the last known state; if that's unknown, publish nothing rather
/// than guess (the device's own report will follow).
fn optimistic_on_off(command: &str, current: Option<&str>) -> Option<&'static str> {
    match command.to_uppercase().as_str() {
        "ON" | "TRUE" => Some("ON"),
        "OFF" | "FALSE" => Some("OFF"),
        "TOGGLE" => match current.map(str::to_uppercase).as_deref() {
            Some("ON") => Some("OFF"),
            Some("OFF") => Some("ON"),
            _ => None,
        },
        _ => None,
    }
}

/// Interviews waiting on SimpleDescRsps, keyed by NWK address.
#[derive(Default)]
struct InterviewTracker {
    pending: HashMap<u16, PendingInterview>,
}

struct PendingInterview {
    expected: HashSet<u8>,
    received: HashSet<u8>,
    /// Time of the latest ActiveEpRsp; the timeout counts from here.
    started: Instant,
}

impl InterviewTracker {
    /// Record an ActiveEpRsp. Returns the endpoints whose simple descriptor
    /// still needs requesting (a retried ActiveEpRsp only re-requests the
    /// ones not yet described).
    fn expect(&mut self, nwk: u16, endpoints: &[u8], now: Instant) -> Vec<u8> {
        if endpoints.is_empty() {
            self.pending.remove(&nwk);
            return Vec::new();
        }
        let entry = self.pending.entry(nwk).or_insert_with(|| PendingInterview {
            expected: HashSet::new(),
            received: HashSet::new(),
            started: now,
        });
        entry.expected = endpoints.iter().copied().collect();
        let expected = &entry.expected;
        entry.received.retain(|ep| expected.contains(ep));
        entry.started = now;
        let mut missing = Vec::new();
        for &ep in endpoints {
            if !entry.received.contains(&ep) && !missing.contains(&ep) {
                missing.push(ep);
            }
        }
        missing
    }

    /// Record a SimpleDescRsp. Returns true once every expected endpoint has
    /// been described -- or if `nwk` isn't tracked (its ActiveEpRsp was
    /// never seen), in which case one endpoint is the best we know of.
    fn record(&mut self, nwk: u16, endpoint: u8) -> bool {
        let Some(p) = self.pending.get_mut(&nwk) else {
            return true;
        };
        p.received.insert(endpoint);
        if p.received.is_superset(&p.expected) {
            self.pending.remove(&nwk);
            true
        } else {
            false
        }
    }

    /// Drop interviews that have waited longer than `INTERVIEW_TIMEOUT`.
    /// Returns those that got at least one endpoint, to be completed with
    /// what arrived.
    fn expire(&mut self, now: Instant) -> Vec<u16> {
        let mut stalled = Vec::new();
        self.pending.retain(|&nwk, p| {
            if now.duration_since(p.started) < INTERVIEW_TIMEOUT {
                return true;
            }
            if !p.received.is_empty() {
                stalled.push(nwk);
            }
            false
        });
        stalled
    }

    fn forget(&mut self, nwk: u16) {
        self.pending.remove(&nwk);
    }
}

/// Per-NWK rate limiter for unknown-address IEEE lookups.
#[derive(Default)]
struct LookupLimiter {
    last: HashMap<u16, Instant>,
}

impl LookupLimiter {
    fn allow(&mut self, nwk: u16, now: Instant) -> bool {
        match self.last.get(&nwk) {
            Some(&t) if now.duration_since(t) < IEEE_LOOKUP_INTERVAL => false,
            _ => {
                self.last.insert(nwk, now);
                true
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        self.last
            .retain(|_, t| now.duration_since(*t) < IEEE_LOOKUP_INTERVAL);
    }
}

/// Actual permit-join state, as last sent to the coordinator.
#[derive(Default)]
struct PermitJoin {
    /// When the current permit-join window closes; `None` when disabled.
    until: Option<Instant>,
    /// Keep re-opening the window before it closes (`permit_join: true`).
    keep_enabled: bool,
}

impl PermitJoin {
    fn enabled(&self, now: Instant) -> bool {
        self.until.is_some_and(|u| now < u)
    }

    /// Record a successfully sent permit-join request of `duration` seconds.
    fn on_sent(&mut self, duration: u8, now: Instant) {
        self.until = (duration > 0).then(|| now + Duration::from_secs(u64::from(duration)));
    }

    fn needs_refresh(&self, now: Instant) -> bool {
        self.keep_enabled
            && self
                .until
                .is_none_or(|u| u.saturating_duration_since(now) <= PERMIT_JOIN_REFRESH_MARGIN)
    }

    /// Clear an elapsed window; returns true exactly once when it closes.
    fn take_expired(&mut self, now: Instant) -> bool {
        if self.until.is_some_and(|u| now >= u) {
            self.until = None;
            true
        } else {
            false
        }
    }
}

/// Resolves on SIGINT or SIGTERM, so the bridge can publish its offline state
/// on a normal service stop.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                warn!("Failed to install SIGTERM handler: {e}");
                ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    ctrl_c().await;
}

/// Ctrl-C, but never resolving if the handler can't be installed -- an error
/// there must not look like a shutdown request.
async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!("Failed to install Ctrl-C handler: {e}");
        std::future::pending::<()>().await;
    }
}

/// Keys BasicCluster::process_reports emits, mirroring the dedicated Device
/// fields handle_basic_cluster_response() populates from the same data.
const BASIC_CLUSTER_METADATA_KEYS: [&str; 4] =
    ["manufacturer", "model", "power_source", "sw_build_id"];

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
        .map(|s| (s.max(0.0) * 10.0).min(65535.0) as u16)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ieee_addr_valid() {
        let addr = IeeeAddr::from_hex("0x00158D0001020304").unwrap();
        assert_eq!(addr.as_hex(), "0x00158d0001020304");
    }

    #[test]
    fn parse_ieee_addr_lowercase() {
        let addr = IeeeAddr::from_hex("0xec1bbdfffeaa66db").unwrap();
        assert_eq!(addr.as_hex().to_lowercase(), "0xec1bbdfffeaa66db");
    }

    #[test]
    fn parse_ieee_addr_invalid() {
        assert!(IeeeAddr::from_hex("0x1234").is_none());
        assert!(IeeeAddr::from_hex("not_hex").is_none());
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
    fn strip_transient_action_fields_removes_only_action_keys() {
        let mut state = serde_json::Map::new();
        state.insert("action".into(), json!("brightness_up_click"));
        state.insert("action_duration".into(), json!(1.5));
        state.insert("brightness".into(), json!(200));
        state.insert("battery".into(), json!(80));
        state.insert("state".into(), json!("ON"));

        Bridge::strip_transient_action_fields(&mut state);

        assert!(!state.contains_key("action"));
        assert!(!state.contains_key("action_duration"));
        assert_eq!(state.get("brightness"), Some(&json!(200)));
        assert_eq!(state.get("battery"), Some(&json!(80)));
        assert_eq!(state.get("state"), Some(&json!("ON")));
    }

    #[test]
    fn output_json_publishes_only_merged_state() {
        let state = json!({"state": "ON", "brightness": 200});
        let (publish_json, attributes) = Bridge::plan_state_publishes(OutputMode::Json, &state);
        assert!(publish_json);
        assert!(attributes.is_empty());
    }

    #[test]
    fn output_attribute_publishes_only_flattened_attributes() {
        let state = json!({"state": "ON", "brightness": 200});
        let (publish_json, attributes) =
            Bridge::plan_state_publishes(OutputMode::Attribute, &state);
        assert!(!publish_json);
        assert_eq!(attributes.len(), 2);
        assert!(attributes.contains(&("state".to_string(), "ON".to_string())));
        assert!(attributes.contains(&("brightness".to_string(), "200".to_string())));
    }

    #[test]
    fn output_attribute_and_json_publishes_both() {
        let state = json!({"state": "ON", "brightness": 200});
        let (publish_json, attributes) =
            Bridge::plan_state_publishes(OutputMode::AttributeAndJson, &state);
        assert!(publish_json);
        assert_eq!(attributes.len(), 2);
    }

    /// Reproduces the exact per-message sequence in the `CoordinatorEvent::
    /// Message` handler (merge_state -> clone into `state` -> strip transient
    /// action fields from retained `dev.state` -> plan_state_publishes) for
    /// two consecutive button clicks reporting the *same* action value, in
    /// `AttributeAndJson` mode. Guards against the flattened `action`
    /// attribute topic silently going stale/missing on repeat presses.
    #[test]
    fn repeated_identical_action_is_flattened_every_click() {
        let mut dev = Device::new(IeeeAddr::from_hex("0x00158d0001020304").unwrap(), 0x1234);

        for _ in 0..2 {
            let mut values = serde_json::Map::new();
            values.insert("action".into(), json!("toggle"));
            dev.merge_state(values);
            dev.state.insert("linkquality".into(), json!(200));

            let state = serde_json::Value::Object(dev.state.clone());
            Bridge::strip_transient_action_fields(&mut dev.state);

            let (publish_json, attributes) =
                Bridge::plan_state_publishes(OutputMode::AttributeAndJson, &state);
            assert!(publish_json);
            assert!(
                attributes.contains(&("action".to_string(), "toggle".to_string())),
                "action attribute missing from flattened publish: {attributes:?}"
            );
            assert!(!dev.state.contains_key("action"));
        }
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

    #[test]
    fn optimistic_on_off_explicit_states() {
        assert_eq!(optimistic_on_off("on", None), Some("ON"));
        assert_eq!(optimistic_on_off("OFF", Some("ON")), Some("OFF"));
        assert_eq!(optimistic_on_off("true", None), Some("ON"));
        assert_eq!(optimistic_on_off("blah", Some("ON")), None);
    }

    #[test]
    fn optimistic_on_off_toggle_inverts_known_state() {
        assert_eq!(optimistic_on_off("TOGGLE", Some("ON")), Some("OFF"));
        assert_eq!(optimistic_on_off("toggle", Some("OFF")), Some("ON"));
        assert_eq!(optimistic_on_off("TOGGLE", None), None);
    }

    #[test]
    fn next_trans_id_wraps_and_is_unique_per_call() {
        let counter = AtomicU8::new(254);
        assert_eq!(next_trans_id(&counter), 255);
        assert_eq!(next_trans_id(&counter), 0);
        assert_eq!(next_trans_id(&counter), 1);
    }

    #[test]
    fn interview_completes_only_after_all_endpoints() {
        let now = Instant::now();
        let mut t = InterviewTracker::default();
        assert_eq!(t.expect(0x1234, &[1, 2], now), vec![1, 2]);
        assert!(!t.record(0x1234, 1));
        assert!(t.record(0x1234, 2));
        // Done -> no longer tracked.
        assert!(t.expire(now + INTERVIEW_TIMEOUT).is_empty());
    }

    #[test]
    fn interview_retry_only_requests_missing_endpoints() {
        let now = Instant::now();
        let mut t = InterviewTracker::default();
        t.expect(0x1234, &[1, 2, 3], now);
        assert!(!t.record(0x1234, 2));
        assert_eq!(t.expect(0x1234, &[1, 2, 3], now), vec![1, 3]);
    }

    #[test]
    fn untracked_interview_completes_on_first_endpoint() {
        let mut t = InterviewTracker::default();
        assert!(t.record(0x4321, 1));
    }

    #[test]
    fn stalled_interview_expires_with_partial_endpoints() {
        let now = Instant::now();
        let mut t = InterviewTracker::default();
        t.expect(0x1111, &[1, 2], now);
        t.record(0x1111, 1);
        t.expect(0x2222, &[1], now); // nothing received -> dropped, not completed
        assert!(t.expire(now + Duration::from_secs(1)).is_empty());
        assert_eq!(t.expire(now + INTERVIEW_TIMEOUT), vec![0x1111]);
        assert!(t.pending.is_empty());
    }

    #[test]
    fn empty_active_ep_rsp_requests_nothing() {
        let mut t = InterviewTracker::default();
        assert!(t.expect(0x1234, &[], Instant::now()).is_empty());
    }

    #[test]
    fn ieee_lookups_are_rate_limited_per_nwk() {
        let now = Instant::now();
        let mut l = LookupLimiter::default();
        assert!(l.allow(0x1234, now));
        assert!(!l.allow(0x1234, now + Duration::from_secs(5)));
        assert!(l.allow(0x5678, now + Duration::from_secs(5)));
        assert!(l.allow(0x1234, now + IEEE_LOOKUP_INTERVAL));
        l.prune(now + IEEE_LOOKUP_INTERVAL * 3);
        assert!(l.last.is_empty());
    }

    #[test]
    fn permit_join_timed_window_expires_once() {
        let now = Instant::now();
        let mut p = PermitJoin::default();
        assert!(!p.enabled(now));
        p.on_sent(60, now);
        assert!(p.enabled(now + Duration::from_secs(59)));
        assert!(!p.needs_refresh(now + Duration::from_secs(59)));
        assert!(!p.take_expired(now + Duration::from_secs(59)));
        assert!(p.take_expired(now + Duration::from_secs(60)));
        assert!(!p.take_expired(now + Duration::from_secs(61)));
        assert!(!p.enabled(now + Duration::from_secs(61)));
    }

    #[test]
    fn permit_join_kept_open_refreshes_before_closing() {
        let now = Instant::now();
        let mut p = PermitJoin {
            keep_enabled: true,
            ..Default::default()
        };
        assert!(p.needs_refresh(now));
        p.on_sent(PERMIT_JOIN_MAX, now);
        assert!(!p.needs_refresh(now + Duration::from_secs(190)));
        assert!(p.needs_refresh(now + Duration::from_secs(200)));
    }

    #[test]
    fn permit_join_zero_disables() {
        let now = Instant::now();
        let mut p = PermitJoin::default();
        p.on_sent(PERMIT_JOIN_MAX, now);
        p.on_sent(0, now);
        assert!(!p.enabled(now));
        assert!(!p.take_expired(now));
    }
}
