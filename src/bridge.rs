/// The main bridge -- ties coordinator, MQTT, device registry, and ZCL together.
///
/// One task (the main loop in `Bridge::run`) owns all device *state*: it
/// handles coordinator events and MQTT commands, merges state and publishes
/// it. It never awaits a coordinator round-trip -- anything that talks to a
/// device runs in that device's job lane (see `jobs`) or a spawned task, and
/// reports back through `Internal` events -- so incoming Zigbee traffic is
/// always drained promptly.
mod commands;
mod jobs;

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::config::{Config, DeviceConfig, OutputMode};
use crate::coordinator::{open_coordinator, CoordinatorEvent, CoordinatorHandle, CoordinatorInfo};
use crate::database;
use crate::devices::{Device, DeviceRegistry};
use crate::error::{Error, Result};
use crate::homeassistant;
use crate::mqtt::attribute_output::flatten_attribute_output;
use crate::mqtt::{MqttBridge, MqttCommand};
use crate::zigbee::zcl;
use crate::zigbee::zcl::clusters::ias_zone;
use crate::zigbee::zcl::frame::{default_response_for, global, FrameType, ZclFrameHeader};
use crate::zigbee::{EndpointDesc, IeeeAddr};

use jobs::{auto_configurable, Ctx, Internal, Job, Lanes};

/// How often the main loop runs housekeeping (permit-join refresh/expiry,
/// stalled-interview completion, rate-limiter pruning).
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(10);

/// How often the coordinator is pinged when nothing else is going on: a
/// CC2531 can hang while idle, and would otherwise only be noticed (and the
/// bridge restarted) once some command timed out.
const COORDINATOR_PING_INTERVAL: Duration = Duration::from_secs(60);

/// Minimum gap between full republishes after MQTT publishes were dropped.
const LOST_PUBLISH_REPUBLISH_INTERVAL: Duration = Duration::from_secs(300);

/// An interview waiting on SimpleDescRsps is completed with whatever
/// endpoints arrived once this long has passed since the last ActiveEpRsp,
/// so one lost descriptor can't leave a device uninterviewed forever.
const INTERVIEW_TIMEOUT: Duration = Duration::from_secs(90);

/// Interview retries: a sleepy device may miss the ZDO request entirely.
const INTERVIEW_ATTEMPTS: u32 = 3;
const INTERVIEW_RETRY_DELAY: Duration = Duration::from_secs(20);

/// Minimum gap between ZDO_IEEE_ADDR_REQs for the same unknown NWK address,
/// so a chatty device we can't identify yet doesn't flood the coordinator.
const IEEE_LOOKUP_INTERVAL: Duration = Duration::from_secs(30);

/// After a device announces itself (typically: it was power-cycled, and a
/// bulb then comes back ON by default) its state is re-read, like
/// zigbee2mqtt does, once it had a moment to settle.
const REFRESH_DELAY: Duration = Duration::from_secs(2);
/// At most one such re-read per device in this period.
const REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// How often to try reading an IAS device's unknown ZoneType when it talks.
const ZONE_TYPE_READ_INTERVAL: Duration = Duration::from_secs(600);

/// Automatic setup (binding + reporting) of devices nobody configured yet:
/// at most this many attempts per run, this far apart.
const MAX_CONFIGURE_ATTEMPTS: u32 = 3;
const CONFIGURE_RETRY_INTERVAL: Duration = Duration::from_secs(1800);

/// Maximum permit-join duration the coordinator accepts in one request.
const PERMIT_JOIN_MAX: u8 = 254;

/// With `permit_join: true` in config, permit join is re-sent once this
/// little of the previous 254 s window is left. Mirrors zigbee-herdsman
/// re-sending every ~200 s (unverified -- from memory, not checked against
/// the herdsman source).
const PERMIT_JOIN_REFRESH_MARGIN: Duration = Duration::from_secs(54);

/// Keys BasicCluster::process_reports emits, mirroring the dedicated Device
/// fields handle_basic_cluster_response() populates from the same data.
const BASIC_CLUSTER_METADATA_KEYS: [&str; 4] =
    ["manufacturer", "model", "power_source", "sw_build_id"];

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
        let db_coordinator_ieee = self.import_database();

        // 2. Apply device configs (friendly names override database)
        self.apply_device_configs();

        // 2b. Restore state.json and keep database.db/state.json up to date
        let store = Arc::new(crate::persistence::Store::new(&self.config_path));
        store.load_state(&self.devices);
        let saver = store.spawn_saver(Arc::clone(&self.devices));

        // 3. Log device summary
        let all = self.devices.all_devices();
        info!(
            "Device registry: {} devices ({} interviewed)",
            all.len(),
            all.iter().filter(|d| d.interview_complete).count()
        );
        for dev in &all {
            info!(
                "  {} ({}) nwk=0x{:04X} eps={} interviewed={} configured={} model={:?}",
                dev.friendly_name,
                dev.ieee_addr,
                dev.nwk_addr,
                dev.endpoints.len(),
                dev.interview_complete,
                dev.configured,
                dev.model.as_deref().unwrap_or("-"),
            );
        }
        drop(all);

        // 4. Connect MQTT
        let (mqtt, mut mqtt_rx) = MqttBridge::connect(&self.cfg.mqtt).await?;
        let mqtt = Arc::new(mqtt);
        info!("MQTT connected (base_topic={})", self.cfg.mqtt.base_topic);

        // 5. Open coordinator
        let CoordinatorHandle {
            znp,
            events: mut coord_events,
            info: coord_info,
        } = open_coordinator(&self.cfg).await?;
        info!("Coordinator ready");

        let mut coord_info = coord_info;
        if coord_info.ieee_addr.is_none() {
            // UTIL_GET_DEVICE_INFO failed; the address can't have changed
            // since zigbee2mqtt wrote it down, and binding/IAS enrollment
            // need it.
            warn!("Coordinator IEEE address unknown, using the one from database.db: {db_coordinator_ieee:?}");
            coord_info.ieee_addr = db_coordinator_ieee;
        }
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(Ctx::new(
            Arc::clone(&self.devices),
            znp,
            coord_info.ieee_addr,
            internal_tx,
        ));
        let mut rt = Runtime::new(
            &self.cfg,
            ctx,
            Arc::clone(&mqtt),
            coord_info,
            Arc::clone(&store),
        );

        // 6. Permit join: enable if configured, otherwise explicitly disable --
        //    don't rely on whatever the coordinator's firmware defaults to.
        //    Not awaited: the result comes back as an Internal event.
        rt.permit_join.keep_enabled = self.cfg.permit_join;
        if self.cfg.permit_join {
            info!("Permit join enabled (kept open while permit_join: true)");
            rt.spawn_permit_join(PERMIT_JOIN_MAX, true);
        } else {
            rt.spawn_permit_join(0, false);
        }

        // 7. Publish retained MQTT state, then bring device state up to date.
        rt.republish_mqtt_state().await;
        rt.startup_jobs();
        info!("Startup complete, entering main loop");

        // 8. Main event loop
        let mut housekeeping = tokio::time::interval(HOUSEKEEPING_INTERVAL);
        housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        let result: Result<()> = loop {
            tokio::select! {
                event = coord_events.recv() => match event {
                    // The coordinator layer closes this channel on any fatal
                    // condition (serial error, unexpected reset, unresponsive
                    // adapter). Exit non-zero so the service manager restarts
                    // and re-initialises us.
                    None => {
                        error!("Coordinator event channel closed");
                        break Err(Error::Znp("coordinator connection lost".into()));
                    }
                    Some(event) => rt.on_coordinator_event(event).await,
                },
                cmd = mqtt_rx.recv() => match cmd {
                    None => {
                        error!("MQTT command channel closed");
                        break Err(Error::ChannelClosed);
                    }
                    Some(cmd) => rt.on_mqtt_command(cmd).await,
                },
                // Never closes: `rt.ctx` holds a sender.
                Some(msg) = internal_rx.recv() => rt.on_internal(msg).await,
                _ = housekeeping.tick() => rt.housekeeping().await,
                _ = &mut shutdown => {
                    info!("Shutdown signal received");
                    break Ok(());
                }
            }
        };

        if let Err(e) = mqtt.publish_bridge_state(false).await {
            warn!("Failed to publish offline state: {e}");
        }
        mqtt.disconnect().await;
        saver.abort();
        let devices = Arc::clone(&self.devices);
        let saved = tokio::task::spawn_blocking(move || store.save(&devices)).await;
        if let Err(e) = saved {
            warn!("Final save failed: {e}");
        }
        result
    }

    /// Import devices from an existing zigbee2mqtt database.db file.
    /// Returns the coordinator's IEEE address recorded there.
    fn import_database(&self) -> Option<IeeeAddr> {
        let db_path = database::find_database(&self.config_path)?;
        let (devices, coordinator_ieee) = database::load_database(&db_path, &self.cfg.devices);
        for dev in devices {
            self.devices.add(dev);
        }
        coordinator_ieee
    }

    /// Apply friendly names from config, overriding database values.
    /// Also pre-seeds stub entries for devices in config but not in database.
    fn apply_device_configs(&self) {
        for (ieee_str, cfg) in &self.cfg.devices {
            let Some(ieee) = IeeeAddr::from_hex(ieee_str) else {
                warn!("Ignoring config for invalid IEEE address {ieee_str:?}");
                continue;
            };
            let name = cfg.friendly_name.clone().unwrap_or_else(|| ieee.as_hex());
            let disabled = cfg.disabled.unwrap_or(false);

            let known = match self.devices.get_mut_by_ieee(&ieee) {
                Some(mut dev) => {
                    dev.disabled = disabled;
                    true
                }
                None => false,
            };
            if known {
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

/// Main-loop state.
struct Runtime {
    ctx: Arc<Ctx>,
    mqtt: Arc<MqttBridge>,
    store: Arc<crate::persistence::Store>,
    last_ping: Instant,
    info: CoordinatorInfo,
    coordinator_ieee_str: String,
    base_topic: String,
    ha_enabled: bool,
    output: OutputMode,
    log_level: String,
    device_configs: HashMap<String, DeviceConfig>,
    lanes: Lanes,
    permit_join: PermitJoin,
    interviews: InterviewTracker,
    ieee_lookups: RateLimiter<u16>,
    state_refreshes: RateLimiter<IeeeAddr>,
    zone_type_reads: RateLimiter<IeeeAddr>,
    configure_attempts: HashMap<IeeeAddr, (u32, Instant)>,
    setups_in_flight: HashSet<IeeeAddr>,
    /// Interviewed devices whose post-interview setup hasn't succeeded yet.
    pending_full_setup: HashSet<IeeeAddr>,
    /// Permit-join requests go to one worker task in order, so a disable is
    /// never overtaken by an earlier enable or refresh.
    permit_join_tx: mpsc::UnboundedSender<(u8, bool)>,
    last_lost_republish: Option<Instant>,
}

impl Runtime {
    fn new(
        cfg: &Config,
        ctx: Arc<Ctx>,
        mqtt: Arc<MqttBridge>,
        info: CoordinatorInfo,
        store: Arc<crate::persistence::Store>,
    ) -> Self {
        let (permit_join_tx, mut permit_join_rx) = mpsc::unbounded_channel::<(u8, bool)>();
        let znp = Arc::clone(&ctx.znp);
        let internal_tx = ctx.internal_tx.clone();
        tokio::spawn(async move {
            while let Some((duration, refresh)) = permit_join_rx.recv().await {
                let ok = match znp.permit_join(duration).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("Failed to set permit join ({duration}s): {e}");
                        false
                    }
                };
                let _ = internal_tx.send(Internal::PermitJoin {
                    duration,
                    ok,
                    refresh,
                });
            }
        });
        Self {
            store,
            last_ping: Instant::now(),
            coordinator_ieee_str: info.ieee_addr.map(|a| a.as_hex()).unwrap_or_default(),
            ctx,
            mqtt,
            info,
            base_topic: cfg.mqtt.base_topic.clone(),
            ha_enabled: cfg.homeassistant,
            output: cfg.advanced.output,
            log_level: cfg.advanced.log_level.clone(),
            device_configs: cfg.devices.clone(),
            lanes: Lanes::default(),
            permit_join: PermitJoin::default(),
            interviews: InterviewTracker::default(),
            ieee_lookups: RateLimiter::new(IEEE_LOOKUP_INTERVAL),
            state_refreshes: RateLimiter::new(REFRESH_MIN_INTERVAL),
            zone_type_reads: RateLimiter::new(ZONE_TYPE_READ_INTERVAL),
            configure_attempts: HashMap::new(),
            setups_in_flight: HashSet::new(),
            pending_full_setup: HashSet::new(),
            permit_join_tx,
            last_lost_republish: None,
        }
    }

    fn devices(&self) -> &DeviceRegistry {
        &self.ctx.devices
    }

    // ── Coordinator events ───────────────────────────────────────────────────

    async fn on_coordinator_event(&mut self, event: CoordinatorEvent) {
        match event {
            CoordinatorEvent::DeviceJoined { ieee_addr, nwk_addr } => {
                self.on_device_announce(ieee_addr, nwk_addr).await;
            }
            CoordinatorEvent::DeviceLeft { ieee_addr } => self.on_device_left(ieee_addr).await,
            CoordinatorEvent::AddressResolved { ieee_addr, nwk_addr } => {
                self.on_address_resolved(ieee_addr, nwk_addr).await;
            }
            CoordinatorEvent::TrustCenterJoin { ieee_addr, nwk_addr } => {
                self.on_address_resolved(ieee_addr, nwk_addr).await;
                // zigbee2mqtt reconfigures a device on every join: a device
                // that was reset and re-paired lost its bindings, reporting
                // and IAS enrollment. Redo the full post-interview setup.
                let interviewed = match self.devices().get_mut_by_ieee(&ieee_addr) {
                    Some(mut dev) if dev.interview_complete => {
                        dev.configured = false;
                        true
                    }
                    _ => false,
                };
                if interviewed {
                    info!("{ieee_addr} joined through the trust center, reconfiguring");
                    self.pending_full_setup.insert(ieee_addr);
                    self.configure_attempts.remove(&ieee_addr);
                    self.maybe_configure(ieee_addr);
                }
            }
            CoordinatorEvent::ActiveEpRsp {
                nwk_addr,
                endpoints,
            } => {
                debug!("Active EPs for 0x{nwk_addr:04X}: {endpoints:?}");
                let missing = self.interviews.expect(nwk_addr, &endpoints, Instant::now());
                self.spawn_simple_desc_requests(nwk_addr, missing);
            }
            CoordinatorEvent::BindRsp { nwk_addr, status } => {
                if status == 0 {
                    debug!("Bind succeeded for 0x{nwk_addr:04X}");
                } else {
                    warn!("Bind failed for 0x{nwk_addr:04X}: status=0x{status:02X}");
                }
            }
            CoordinatorEvent::SimpleDescRsp {
                nwk_addr,
                endpoint,
                profile_id,
                device_id,
                input_clusters,
                output_clusters,
            } => {
                debug!("SimpleDesc 0x{nwk_addr:04X} ep={endpoint} clusters={input_clusters:?}");
                let ep_desc = EndpointDesc {
                    endpoint,
                    profile_id,
                    device_id,
                    input_clusters,
                    output_clusters,
                };
                if let Some(mut dev) = self.devices().get_mut_by_nwk(nwk_addr) {
                    dev.endpoints.retain(|e| e.endpoint != endpoint);
                    dev.endpoints.push(ep_desc);
                    dev.endpoints.sort_by_key(|e| e.endpoint);
                }
                // Only complete the interview once every endpoint from
                // ActiveEpRsp has been described, so HA discovery isn't
                // published from a partial set.
                if self.interviews.record(nwk_addr, endpoint) {
                    self.complete_interview(nwk_addr).await;
                }
                self.publish_device_list().await;
            }
            CoordinatorEvent::Message {
                src_addr,
                src_ep,
                dst_ep,
                was_broadcast,
                cluster_id,
                link_quality,
                data,
            } => {
                debug!(
                    "AF msg from 0x{src_addr:04X} ep={src_ep}->{dst_ep} cluster=0x{cluster_id:04X} lqi={link_quality} bcast={was_broadcast} data={data:02X?}"
                );
                self.on_message(src_addr, src_ep, was_broadcast, cluster_id, link_quality, &data)
                    .await;
            }
        }
    }

    /// ZDO end device announce: a device joined, rejoined, or came back
    /// after a power cycle.
    async fn on_device_announce(&mut self, ieee: IeeeAddr, nwk_addr: u16) {
        info!("Device joined: {ieee} (0x{nwk_addr:04X})");
        let known = self.devices().get_by_ieee(&ieee).is_some();
        if known {
            self.devices().update_nwk_addr(&ieee, nwk_addr);
        } else {
            self.add_new_device(ieee, nwk_addr);
        }
        if let Err(e) = self
            .mqtt
            .publish_bridge_log("info", &format!("Device joined: {ieee}"))
            .await
        {
            warn!("Failed to publish bridge log: {e}");
        }

        let interviewed = self
            .devices()
            .get_by_ieee(&ieee)
            .is_some_and(|d| d.interview_complete);
        if interviewed {
            // A bulb switched off and on at the wall comes back in its
            // power-on state (usually ON) and announces itself; re-read so
            // MQTT doesn't keep showing the old state.
            self.schedule_refresh(ieee);
            self.maybe_configure(ieee);
        } else {
            self.spawn_interview_retry(ieee);
        }
        if !known {
            self.publish_device_list().await;
        }
    }

    async fn on_device_left(&mut self, ieee: IeeeAddr) {
        info!("Device left: {ieee}");
        if let Some(nwk_addr) = self.devices().get_by_ieee(&ieee).map(|d| d.nwk_addr) {
            self.interviews.forget(nwk_addr);
        }
        self.devices().remove_by_ieee(&ieee);
        self.store.forget_device(&ieee);
        self.lanes.close(&ieee);
        self.setups_in_flight.remove(&ieee);
        self.pending_full_setup.remove(&ieee);
        self.configure_attempts.remove(&ieee);
        if let Err(e) = self
            .mqtt
            .publish_bridge_log("info", &format!("Device left: {ieee}"))
            .await
        {
            warn!("Failed to publish bridge log: {e}");
        }
        self.publish_device_list().await;
    }

    async fn on_address_resolved(&mut self, ieee: IeeeAddr, nwk_addr: u16) {
        debug!("Address resolved: {ieee} -> 0x{nwk_addr:04X}");
        match self.devices().update_nwk_addr(&ieee, nwk_addr) {
            Some(old) if old != nwk_addr => {
                info!("{ieee} is now at NWK 0x{nwk_addr:04X} (was 0x{old:04X})");
            }
            Some(_) => {}
            None => {
                info!("New device {ieee} at NWK 0x{nwk_addr:04X}");
                self.add_new_device(ieee, nwk_addr);
                self.publish_device_list().await;
            }
        }
        if self
            .devices()
            .get_by_ieee(&ieee)
            .is_some_and(|d| !d.interview_complete)
        {
            self.spawn_interview_retry(ieee);
        }
    }

    fn add_new_device(&self, ieee: IeeeAddr, nwk_addr: u16) {
        let mut dev = Device::new(ieee, nwk_addr);
        if let Some(cfg) = self.device_configs.get(&ieee.as_hex()) {
            if let Some(ref name) = cfg.friendly_name {
                dev.friendly_name = name.clone();
            }
            dev.disabled = cfg.disabled.unwrap_or(false);
        }
        self.devices().add(dev);
    }

    async fn on_message(
        &mut self,
        src_addr: u16,
        src_ep: u8,
        was_broadcast: bool,
        cluster_id: u16,
        link_quality: u8,
        data: &[u8],
    ) {
        let now = Instant::now();
        let ieee = self.devices().ieee_for_nwk(src_addr);
        // Unknown NWK? Request IEEE address to link it.
        if ieee.is_none() && self.ieee_lookups.allow(src_addr, now) {
            debug!("Unknown NWK 0x{src_addr:04X}, requesting IEEE address");
            self.spawn_ieee_lookup(src_addr);
        }

        let header = match ZclFrameHeader::parse(data) {
            Ok((h, _)) => h,
            Err(e) => {
                debug!("Unparseable ZCL frame from 0x{src_addr:04X}: {e}");
                return;
            }
        };
        let enroll_request = cluster_id == 0x0500
            && header.frame_type == FrameType::ClusterSpecific
            && header.server_to_client
            && header.command_id == ias_zone::CMD_ZONE_ENROLL_REQ;

        // ZCL Default Response, for senders that asked for one (e.g. remote
        // button presses): devices that don't get it retry, drain their
        // battery and may consider the coordinator gone.
        if !was_broadcast && !enroll_request {
            if let Some(rsp) = default_response_for(&header) {
                let znp = Arc::clone(&self.ctx.znp);
                tokio::spawn(async move {
                    if let Err(e) = znp
                        .send_zcl_unconfirmed(src_addr, src_ep, cluster_id, rsp)
                        .await
                    {
                        debug!("Failed to send default response to 0x{src_addr:04X}: {e}");
                    }
                });
            }
        }
        self.log_command_failures(src_addr, cluster_id, &header, data);

        if cluster_id == 0x0000 {
            self.handle_basic_cluster_response(src_addr, data).await;
        }
        if cluster_id == 0x0500 {
            self.handle_ias_zone_type_response(src_addr, data).await;
            if enroll_request {
                info!("IAS zone enroll request from 0x{src_addr:04X}, enrolling");
                let ctx = Arc::clone(&self.ctx);
                let payload = ias_zone::enroll_response_payload(
                    ctx.next_seq(),
                    ias_zone::DEFAULT_ZONE_ID,
                );
                tokio::spawn(async move {
                    // Straight to the sender's address: it is awake now and
                    // may not be in the registry yet.
                    if let Err(e) = ctx.znp.send_zcl(src_addr, src_ep, 0x0500, payload).await {
                        warn!("Failed to send IAS zone enroll response to 0x{src_addr:04X}: {e}");
                    }
                });
            }
        }

        let Some(ieee) = ieee else {
            return;
        };
        self.follow_up_on_traffic(ieee, now);

        let (zone_type, held_arrow_direction) = match self.devices().get_by_ieee(&ieee) {
            Some(d) => (d.zone_type, d.arrow_hold_direction.clone()),
            None => return,
        };
        let mut zcl_msg = match zcl::parse_message(
            cluster_id,
            data,
            zone_type,
            held_arrow_direction.as_deref(),
        ) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                // Still proof of life.
                if let Some(mut dev) = self.devices().get_mut_by_ieee(&ieee) {
                    dev.state.insert("linkquality".into(), json!(link_quality));
                    dev.state.insert("last_seen".into(), json!(now_iso8601()));
                }
                return;
            }
            Err(e) => {
                warn!("ZCL parse error from 0x{src_addr:04X} cluster 0x{cluster_id:04X}: {e}");
                return;
            }
        };
        if cluster_id == 0x0000 {
            // Basic cluster attributes (manufacturer/model/power_source/
            // sw_build_id) are device metadata, not volatile state -- z2m
            // doesn't publish them on the device state topic.
            // handle_basic_cluster_response() above already routed them onto
            // dedicated Device fields; strip them here so they aren't also
            // merged into (and published as part of) device state.
            for key in BASIC_CLUSTER_METADATA_KEYS {
                zcl_msg.values.remove(key);
            }
        }

        let has_action = zcl_msg
            .values
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|a| !a.is_empty());
        let (name, state, after_action) = {
            let Some(mut dev) = self.devices().get_mut_by_ieee(&ieee) else {
                return;
            };
            if let Some(direction_update) = zcl_msg.arrow_hold_direction {
                dev.arrow_hold_direction = direction_update;
            }
            if zcl_msg.values.is_empty() {
                return;
            }
            dev.merge_state(zcl_msg.values);
            dev.state.insert("linkquality".into(), json!(link_quality));
            dev.state.insert("last_seen".into(), json!(now_iso8601()));
            let state = Value::Object(dev.state.clone());
            // `action`/`action_*` are momentary button-press events, not
            // retained device state -- matching zigbee2mqtt, don't let them
            // linger and pollute later unrelated publishes for this device
            // (they're still included in `state`, built just above, for this
            // one publish).
            strip_transient_action_fields(&mut dev.state);
            // zigbee2mqtt's legacy action sensor: publish an empty action
            // right after each one, so Home Assistant sees a state change
            // for every press (it ignores an update repeating the current
            // value, e.g. a second "toggle") and the retained message doesn't
            // replay the press to every new subscriber.
            let after_action = has_action.then(|| {
                let mut reset = dev.state.clone();
                reset.insert("action".into(), json!(""));
                Value::Object(reset)
            });
            (dev.friendly_name.clone(), state, after_action)
        };
        self.publish_state(&name, &state).await;
        if let Some(reset) = after_action {
            self.publish_state(&name, &reset).await;
        }
    }

    /// Things worth doing while a device is evidently awake and reachable.
    fn follow_up_on_traffic(&mut self, ieee: IeeeAddr, now: Instant) {
        let needs_zone_type = self.devices().get_by_ieee(&ieee).is_some_and(|d| {
            d.zone_type.is_none() && d.endpoint_with_input_cluster(0x0500).is_some()
        });
        if needs_zone_type && self.zone_type_reads.allow(ieee, now) {
            debug!("Reading unknown IAS ZoneType of {ieee}");
            self.lanes.submit(&self.ctx, ieee, Job::ReadZoneType);
        }
        self.maybe_configure(ieee);
    }

    /// Log Default Responses that report a failed command and failed
    /// attribute reporting configurations -- the only sign that a device
    /// refused something we sent.
    fn log_command_failures(
        &self,
        src_addr: u16,
        cluster_id: u16,
        header: &ZclFrameHeader,
        data: &[u8],
    ) {
        if header.frame_type != FrameType::Global {
            return;
        }
        let Ok((_, offset)) = ZclFrameHeader::parse(data) else {
            return;
        };
        let payload = &data[offset..];
        match header.command_id {
            global::DEFAULT_RESPONSE => {
                if let [command, status, ..] = *payload {
                    if status != 0 {
                        warn!(
                            "0x{src_addr:04X} rejected command 0x{command:02X} on cluster 0x{cluster_id:04X}: ZCL status 0x{status:02X}"
                        );
                    }
                }
            }
            // A single SUCCESS byte, or (status, direction, attr) records
            // for the attributes that failed.
            global::CONFIGURE_REPORTING_RSP
                if payload.len() > 1 || payload.first().is_some_and(|s| *s != 0) =>
            {
                debug!(
                    "0x{src_addr:04X} did not accept all reporting config for cluster 0x{cluster_id:04X}: {payload:02X?}"
                );
            }
            _ => {}
        }
    }

    // ── MQTT commands ────────────────────────────────────────────────────────

    async fn on_mqtt_command(&mut self, cmd: MqttCommand) {
        match cmd {
            MqttCommand::Connected => {
                info!("MQTT session restored, republishing retained state");
                self.republish_mqtt_state().await;
            }
            MqttCommand::PermitJoin { duration } => {
                info!("Permit join: {duration}s");
                if duration == 0 {
                    // An explicit disable also stops the config-driven
                    // keep-open refresh -- right away, so no refresh can be
                    // queued behind this request and reopen the network.
                    self.permit_join.keep_enabled = false;
                }
                self.spawn_permit_join(duration, false);
                if let Err(e) = self
                    .mqtt
                    .publish_bridge_log("info", &format!("Permit join: {duration}s"))
                    .await
                {
                    warn!("Failed to publish bridge log: {e}");
                }
            }
            MqttCommand::SetDevice {
                friendly_name,
                payload,
            } => {
                info!("MQTT set: {friendly_name} -> {payload}");
                match self.resolve_device(&friendly_name) {
                    Some(ieee) => self.lanes.submit(&self.ctx, ieee, Job::Set(payload)),
                    None => warn!("Set command for unknown device: {friendly_name}"),
                }
            }
            MqttCommand::GetDevice {
                friendly_name,
                payload,
            } => {
                debug!("MQTT get: {friendly_name} {payload}");
                match self.resolve_device(&friendly_name) {
                    Some(ieee) => self.lanes.submit(&self.ctx, ieee, Job::Get(payload)),
                    None => warn!("Get command for unknown device: {friendly_name}"),
                }
            }
        }
    }

    /// Device addressed by friendly name, or (like zigbee2mqtt) by IEEE
    /// address.
    fn resolve_device(&self, name: &str) -> Option<IeeeAddr> {
        if let Some(dev) = self.devices().find_by_name(name) {
            return Some(dev.ieee_addr);
        }
        IeeeAddr::from_hex(name).filter(|ieee| self.devices().get_by_ieee(ieee).is_some())
    }

    // ── Results from lanes and timers ────────────────────────────────────────

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::State { ieee, values } => {
                let published = {
                    let Some(mut dev) = self.devices().get_mut_by_ieee(&ieee) else {
                        return;
                    };
                    dev.merge_state(values);
                    (dev.friendly_name.clone(), Value::Object(dev.state.clone()))
                };
                self.publish_state(&published.0, &published.1).await;
            }
            Internal::PublishCached { ieee } => {
                let Some(dev) = self.devices().snapshot(&ieee) else {
                    return;
                };
                if !dev.state.is_empty() {
                    self.publish_state(&dev.friendly_name, &Value::Object(dev.state))
                        .await;
                }
            }
            Internal::Toggled { ieee } => {
                let published = {
                    let Some(mut dev) = self.devices().get_mut_by_ieee(&ieee) else {
                        return;
                    };
                    let flipped = match dev.state.get("state").and_then(Value::as_str) {
                        Some(s) if s.eq_ignore_ascii_case("ON") => "OFF",
                        Some(s) if s.eq_ignore_ascii_case("OFF") => "ON",
                        // Unknown: don't guess; a report (if configured) follows.
                        _ => return,
                    };
                    dev.state.insert("state".into(), json!(flipped));
                    (dev.friendly_name.clone(), Value::Object(dev.state.clone()))
                };
                self.publish_state(&published.0, &published.1).await;
            }
            Internal::SetupDone { ieee, full, ok } => {
                self.setups_in_flight.remove(&ieee);
                if ok && full {
                    self.pending_full_setup.remove(&ieee);
                }
                if ok {
                    if let Some(mut dev) = self.devices().get_mut_by_ieee(&ieee) {
                        dev.configured = true;
                    }
                    self.configure_attempts.remove(&ieee);
                }
            }
            Internal::PermitJoin {
                duration,
                ok,
                refresh,
            } => {
                if refresh {
                    self.permit_join.refresh_in_flight = false;
                }
                if !ok {
                    return;
                }
                let now = Instant::now();
                if refresh && !self.permit_join.keep_enabled {
                    // Disabled over MQTT while this refresh was in flight;
                    // that disable was queued after it, so the network is
                    // closed again.
                    return;
                }
                let was_enabled = self.permit_join.enabled(now);
                self.permit_join.on_sent(duration, now);
                if !refresh || !was_enabled {
                    self.publish_bridge_info().await;
                }
            }
            Internal::RefreshDue { ieee } => {
                self.lanes.submit(&self.ctx, ieee, Job::Refresh);
            }
        }
    }

    // ── Housekeeping ─────────────────────────────────────────────────────────

    async fn housekeeping(&mut self) {
        let now = Instant::now();

        if self.permit_join.needs_refresh(now) && !self.permit_join.refresh_in_flight {
            self.spawn_permit_join(PERMIT_JOIN_MAX, true);
        }
        if self.permit_join.take_expired(now) {
            info!("Permit join expired");
            self.publish_bridge_info().await;
        }

        let stalled = self.interviews.expire(now);
        for &nwk_addr in &stalled {
            warn!("Interview for 0x{nwk_addr:04X} timed out waiting for some endpoints, completing with those received");
            self.complete_interview(nwk_addr).await;
        }
        if !stalled.is_empty() {
            self.publish_device_list().await;
        }

        self.ieee_lookups.prune(now);
        self.state_refreshes.prune(now);
        self.zone_type_reads.prune(now);

        if now.duration_since(self.last_ping) >= COORDINATOR_PING_INTERVAL {
            self.last_ping = now;
            let znp = Arc::clone(&self.ctx.znp);
            tokio::spawn(async move {
                if let Err(e) = znp.ping().await {
                    warn!("Coordinator did not answer ping: {e}");
                }
            });
        }

        // At most every few minutes: a broker that is too slow to take our
        // publishes would otherwise get a full republish storm each tick.
        let may_republish = self
            .last_lost_republish
            .is_none_or(|t| now.duration_since(t) >= LOST_PUBLISH_REPUBLISH_INTERVAL);
        if may_republish && self.mqtt.take_lost_publishes() {
            warn!("Some MQTT publishes were dropped, republishing all state");
            self.last_lost_republish = Some(now);
            self.republish_mqtt_state().await;
        }
    }

    // ── Device follow-up work ────────────────────────────────────────────────

    /// Work queued once at startup: re-read the state of mains-powered
    /// lights (it may have changed while we were down), and configure
    /// reporting on devices nobody configured yet.
    fn startup_jobs(&mut self) {
        let now = Instant::now();
        for dev in self.devices().all_devices() {
            if dev.nwk_addr != 0 && !dev.interview_complete {
                // Interview never finished (e.g. bridge restarted halfway,
                // or imported that way): try again.
                self.spawn_interview_retry(dev.ieee_addr);
                continue;
            }
            if !dev.interview_complete || dev.nwk_addr == 0 || !dev.is_mains_powered() {
                continue;
            }
            if !commands::plan_reads(&dev, None).is_empty() {
                self.state_refreshes.allow(dev.ieee_addr, now);
                self.lanes.submit(&self.ctx, dev.ieee_addr, Job::Refresh);
            }
            self.maybe_configure(dev.ieee_addr);
        }
    }

    /// Re-read `ieee`'s state shortly (debounced, rate-limited).
    fn schedule_refresh(&mut self, ieee: IeeeAddr) {
        let has_state = self
            .devices()
            .snapshot(&ieee)
            .is_some_and(|d| !commands::plan_reads(&d, None).is_empty());
        if !has_state || !self.state_refreshes.allow(ieee, Instant::now()) {
            return;
        }
        let tx = self.ctx.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(REFRESH_DELAY).await;
            let _ = tx.send(Internal::RefreshDue { ieee });
        });
    }

    /// Set up an interviewed device that still needs it -- its post-
    /// interview setup failed (e.g. it fell asleep), or neither zigbee2mqtt
    /// nor we configured its bindings and reporting yet -- with a few,
    /// spaced-out attempts.
    fn maybe_configure(&mut self, ieee: IeeeAddr) {
        let Some(dev) = self.devices().snapshot(&ieee) else {
            return;
        };
        let full = self.pending_full_setup.contains(&ieee);
        let needs_config = !dev.configured && auto_configurable(&dev);
        if !dev.interview_complete
            || !(full || needs_config)
            || self.setups_in_flight.contains(&ieee)
        {
            return;
        }
        let now = Instant::now();
        let entry = self.configure_attempts.entry(ieee).or_insert((0, now));
        if entry.0 >= MAX_CONFIGURE_ATTEMPTS
            || (entry.0 > 0 && now.duration_since(entry.1) < CONFIGURE_RETRY_INTERVAL)
        {
            return;
        }
        *entry = (entry.0 + 1, now);
        self.setups_in_flight.insert(ieee);
        jobs::spawn_setup(&self.ctx, ieee, full);
    }

    /// Mark the device at `nwk_addr` interviewed (if it isn't already and has
    /// at least one described endpoint), publish its HA discovery and set it
    /// up.
    async fn complete_interview(&mut self, nwk_addr: u16) {
        let dev = {
            let Some(mut dev) = self.devices().get_mut_by_nwk(nwk_addr) else {
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
        if self.ha_enabled {
            homeassistant::publish_discovery(
                &self.mqtt,
                &dev,
                &self.base_topic,
                &self.coordinator_ieee_str,
            )
            .await;
        }
        // Until it succeeds, retries (maybe_configure) redo the full setup:
        // IAS enrollment and the interview reads, not just reporting.
        self.pending_full_setup.insert(dev.ieee_addr);
        self.configure_attempts.remove(&dev.ieee_addr);
        self.setups_in_flight.insert(dev.ieee_addr);
        jobs::spawn_setup(&self.ctx, dev.ieee_addr, true);
    }

    /// Basic cluster (0x0000) attributes update device metadata; republish
    /// HA discovery (device name/model) when it changed after the interview.
    async fn handle_basic_cluster_response(&self, src_addr: u16, data: &[u8]) {
        let Ok(Some(zcl_msg)) = zcl::parse_message(0x0000, data, None, None) else {
            return;
        };
        let changed = {
            let Some(mut dev) = self.devices().get_mut_by_nwk(src_addr) else {
                return;
            };
            let mut changed = false;
            for (key, value) in &zcl_msg.values {
                let Some(s) = value.as_str() else {
                    continue;
                };
                let field = match key.as_str() {
                    "manufacturer" => &mut dev.manufacturer,
                    "model" => &mut dev.model,
                    "power_source" => &mut dev.power_source,
                    "sw_build_id" => &mut dev.sw_build_id,
                    _ => continue,
                };
                if field.as_deref() != Some(s) {
                    *field = Some(s.to_string());
                    changed = true;
                }
            }
            (changed && dev.interview_complete).then(|| dev.clone())
        };
        if let Some(dev) = changed {
            if self.ha_enabled {
                homeassistant::publish_discovery(
                    &self.mqtt,
                    &dev,
                    &self.base_topic,
                    &self.coordinator_ieee_str,
                )
                .await;
            }
        }
    }

    /// IAS Zone (0x0500) Read Attributes Response: learn ZoneType, which
    /// determines how zone status reports are classified (contact,
    /// occupancy, smoke, water_leak, ...), and fix up HA discovery if it
    /// changed.
    async fn handle_ias_zone_type_response(&self, src_addr: u16, data: &[u8]) {
        let Ok(Some(zone_type)) = zcl::extract_ias_zone_type(data) else {
            return;
        };
        let changed = {
            let Some(mut dev) = self.devices().get_mut_by_nwk(src_addr) else {
                return;
            };
            let previous = dev.zone_type.replace(zone_type);
            (previous != Some(zone_type)).then(|| (previous, dev.clone()))
        };
        let Some((previous, dev)) = changed else {
            return;
        };
        info!(
            "{} has IAS ZoneType 0x{zone_type:04X} (was {previous:?})",
            dev.friendly_name
        );
        if self.ha_enabled && dev.interview_complete {
            homeassistant::clear_stale_ias_entity(&self.mqtt, &dev, previous).await;
            homeassistant::publish_discovery(
                &self.mqtt,
                &dev,
                &self.base_topic,
                &self.coordinator_ieee_str,
            )
            .await;
        }
    }

    // ── Spawned coordinator requests ─────────────────────────────────────────

    fn spawn_permit_join(&mut self, duration: u8, refresh: bool) {
        if refresh {
            self.permit_join.refresh_in_flight = true;
        }
        let _ = self.permit_join_tx.send((duration, refresh));
    }

    /// Background retry loop for a device's ZDO interview.
    ///
    /// A sleepy battery-powered end device (e.g. a remote control) may be
    /// asleep when the initial ZDO_ACTIVE_EP_REQ goes out over the air and
    /// simply never see it -- the SREQ/SRSP exchange with the coordinator's
    /// own radio still succeeds (it only confirms the request was queued),
    /// so nothing here times out on its own. This re-sends the request a
    /// bounded number of times with a delay generous enough for such devices
    /// to wake up and respond, stopping as soon as the interview completed.
    /// At most one loop runs per device.
    fn spawn_interview_retry(&self, ieee: IeeeAddr) {
        {
            let mut running = self
                .ctx
                .interview_retries
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !running.insert(ieee) {
                return;
            }
        }
        let ctx = Arc::clone(&self.ctx);
        tokio::spawn(async move {
            let done = |ctx: &Ctx| {
                ctx.devices
                    .get_by_ieee(&ieee)
                    .is_none_or(|d| d.interview_complete)
            };
            for attempt in 1..=INTERVIEW_ATTEMPTS {
                if done(&ctx) {
                    break;
                }
                let nwk = ctx.devices.get_by_ieee(&ieee).map(|d| d.nwk_addr);
                match nwk {
                    Some(nwk) if nwk != 0 => {
                        if let Err(e) = ctx.znp.request_active_eps(nwk).await {
                            warn!(
                                "Interview attempt {attempt}/{INTERVIEW_ATTEMPTS} for {ieee}: \
                                 failed to request active endpoints: {e}"
                            );
                        }
                    }
                    _ => debug!("Interview of {ieee} waiting for its network address"),
                }
                tokio::time::sleep(INTERVIEW_RETRY_DELAY).await;
            }
            if !done(&ctx) {
                warn!(
                    "Interview for {ieee} did not complete after {INTERVIEW_ATTEMPTS} attempts -- \
                     device may be asleep or out of range. It will retry automatically on its \
                     next join/announce."
                );
            }
            ctx.interview_retries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&ieee);
        });
    }

    /// Request simple descriptors for `endpoints` off the main loop.
    fn spawn_simple_desc_requests(&self, nwk_addr: u16, endpoints: Vec<u8>) {
        if endpoints.is_empty() {
            return;
        }
        let znp = Arc::clone(&self.ctx.znp);
        tokio::spawn(async move {
            for ep in endpoints {
                if let Err(e) = znp.request_simple_desc(nwk_addr, ep).await {
                    warn!("Failed to request simple descriptor for 0x{nwk_addr:04X} ep={ep}: {e}");
                }
            }
        });
    }

    /// Resolve an unknown NWK address to its IEEE address off the main loop.
    /// The answer arrives as a `CoordinatorEvent::AddressResolved`.
    fn spawn_ieee_lookup(&self, nwk_addr: u16) {
        let znp = Arc::clone(&self.ctx.znp);
        tokio::spawn(async move {
            if let Err(e) = znp.request_ieee_addr(nwk_addr).await {
                warn!("Failed to request IEEE address for 0x{nwk_addr:04X}: {e}");
            }
        });
    }

    // ── MQTT publishing ──────────────────────────────────────────────────────

    async fn publish_bridge_info(&self) {
        let info = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "coordinator": {
                "ieee_address": self.coordinator_ieee_str,
                "type": "z-Stack",
                "meta": {
                    "revision": self.info.transport_rev,
                    "version": self.info.version,
                }
            },
            "log_level": self.log_level,
            "permit_join": self.permit_join.enabled(Instant::now()),
            "config": {},
        });
        if let Err(e) = self.mqtt.publish_bridge_info(&info).await {
            warn!("Failed to publish bridge info: {e}");
        }
    }

    async fn republish_mqtt_state(&self) {
        if let Err(e) = self.mqtt.publish_bridge_state(true).await {
            warn!("Failed to publish online state: {e}");
        }
        self.publish_bridge_info().await;

        for dev in self.devices().all_devices() {
            // Nothing known yet (e.g. just after a restart): publishing `{}`
            // retained would wipe the broker's last real state for HA.
            if !dev.state.is_empty() {
                self.publish_state(&dev.friendly_name, &Value::Object(dev.state.clone()))
                    .await;
            }
            if self.ha_enabled && dev.interview_complete {
                homeassistant::publish_discovery(
                    &self.mqtt,
                    &dev,
                    &self.base_topic,
                    &self.coordinator_ieee_str,
                )
                .await;
            }
        }
        self.publish_device_list().await;
    }

    async fn publish_device_list(&self) {
        let mut devices = self.devices().all_devices();
        devices.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        let list: Vec<Value> = devices.iter().map(Device::to_z2m_device_json).collect();
        if let Err(e) = self.mqtt.publish_bridge_devices(&json!(list)).await {
            warn!("Failed to publish device list: {e}");
        }
    }

    /// Publish a device's state per `advanced.output` (z2m's
    /// `Controller.publishEntityState`): the merged JSON message, each state
    /// key as its own flattened raw-value subtopic, or both. All state
    /// publishes go through here so every call site respects the configured
    /// output mode uniformly.
    async fn publish_state(&self, friendly_name: &str, state: &Value) {
        let (publish_json, attributes) = plan_state_publishes(self.output, state);
        if publish_json {
            if let Err(e) = self.mqtt.publish_device_state(friendly_name, state).await {
                warn!("Failed to publish device state: {e}");
            }
        }
        for (key_path, value) in attributes {
            if let Err(e) = self
                .mqtt
                .publish_device_attribute(friendly_name, &key_path, &value)
                .await
            {
                warn!("Failed to publish attribute {key_path} for {friendly_name}: {e}");
            }
        }
    }
}

/// Remove momentary button-press fields (`action` and `action_*`) from a
/// device's *retained* state. Real device state (brightness, battery,
/// contact, ...) should keep accumulating via `merge_state`, but an `action`
/// is a one-off event -- without this, a stale `action` (and its companion
/// `action_*` keys) from one button press would linger and get republished,
/// mixed in, on every later unrelated event for that device. Call this
/// *after* building the JSON to publish for the current message, so the
/// action still appears in that one publish.
fn strip_transient_action_fields(state: &mut Map<String, Value>) {
    state.retain(|k, _| k != "action" && !k.starts_with("action_"));
}

/// Decides *what* `publish_state` should publish for a given output mode,
/// split out from the actual MQTT I/O purely so this branching is
/// unit-testable without a live broker connection: returns whether the
/// merged-JSON message should be published, and the flattened
/// `(key_path, value)` attribute pairs (empty unless `output` is
/// `Attribute`/`AttributeAndJson`).
fn plan_state_publishes(output: OutputMode, state: &Value) -> (bool, Vec<(String, String)>) {
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

/// Allows one event per key per `interval`.
struct RateLimiter<K> {
    last: HashMap<K, Instant>,
    interval: Duration,
}

impl<K: Eq + Hash> RateLimiter<K> {
    fn new(interval: Duration) -> Self {
        Self {
            last: HashMap::new(),
            interval,
        }
    }

    fn allow(&mut self, key: K, now: Instant) -> bool {
        match self.last.get(&key) {
            Some(&t) if now.duration_since(t) < self.interval => false,
            _ => {
                self.last.insert(key, now);
                true
            }
        }
    }

    /// Forget keys whose interval has passed, so the map can't grow forever.
    fn prune(&mut self, now: Instant) {
        let interval = self.interval;
        self.last.retain(|_, t| now.duration_since(*t) < interval);
    }
}

/// Actual permit-join state, as last sent to the coordinator.
#[derive(Default)]
struct PermitJoin {
    /// When the current permit-join window closes; `None` when disabled.
    until: Option<Instant>,
    /// Keep re-opening the window before it closes (`permit_join: true`).
    keep_enabled: bool,
    /// A keep-open refresh request is on its way to the coordinator.
    refresh_in_flight: bool,
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
        let mut state = Map::new();
        state.insert("action".into(), json!("brightness_up_click"));
        state.insert("action_duration".into(), json!(1.5));
        state.insert("brightness".into(), json!(200));
        state.insert("battery".into(), json!(80));
        state.insert("state".into(), json!("ON"));

        strip_transient_action_fields(&mut state);

        assert!(!state.contains_key("action"));
        assert!(!state.contains_key("action_duration"));
        assert_eq!(state.get("brightness"), Some(&json!(200)));
        assert_eq!(state.get("battery"), Some(&json!(80)));
        assert_eq!(state.get("state"), Some(&json!("ON")));
    }

    #[test]
    fn output_json_publishes_only_merged_state() {
        let state = json!({"state": "ON", "brightness": 200});
        let (publish_json, attributes) = plan_state_publishes(OutputMode::Json, &state);
        assert!(publish_json);
        assert!(attributes.is_empty());
    }

    #[test]
    fn output_attribute_publishes_only_flattened_attributes() {
        let state = json!({"state": "ON", "brightness": 200});
        let (publish_json, attributes) = plan_state_publishes(OutputMode::Attribute, &state);
        assert!(!publish_json);
        assert_eq!(attributes.len(), 2);
        assert!(attributes.contains(&("state".to_string(), "ON".to_string())));
        assert!(attributes.contains(&("brightness".to_string(), "200".to_string())));
    }

    #[test]
    fn output_attribute_and_json_publishes_both() {
        let state = json!({"state": "ON", "brightness": 200});
        let (publish_json, attributes) =
            plan_state_publishes(OutputMode::AttributeAndJson, &state);
        assert!(publish_json);
        assert_eq!(attributes.len(), 2);
    }

    /// Reproduces the per-message sequence in `Runtime::on_message`
    /// (merge_state -> clone into `state` -> strip transient action fields
    /// from retained `dev.state` -> plan_state_publishes) for two
    /// consecutive button clicks reporting the *same* action value, in
    /// `AttributeAndJson` mode. Guards against the flattened `action`
    /// attribute topic silently going stale/missing on repeat presses.
    #[test]
    fn repeated_identical_action_is_flattened_every_click() {
        let mut dev = Device::new(IeeeAddr::from_hex("0x00158d0001020304").unwrap(), 0x1234);

        for _ in 0..2 {
            let mut values = Map::new();
            values.insert("action".into(), json!("toggle"));
            dev.merge_state(values);
            dev.state.insert("linkquality".into(), json!(200));

            let state = Value::Object(dev.state.clone());
            strip_transient_action_fields(&mut dev.state);

            let (publish_json, attributes) =
                plan_state_publishes(OutputMode::AttributeAndJson, &state);
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
    fn rate_limiter_is_per_key_and_prunes() {
        let now = Instant::now();
        let mut l = RateLimiter::new(IEEE_LOOKUP_INTERVAL);
        assert!(l.allow(0x1234u16, now));
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
