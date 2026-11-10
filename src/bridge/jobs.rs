//! Per-device work that needs coordinator round-trips: `set`/`get`
//! commands, state refreshes and device setup (bindings, attribute
//! reporting, IAS enrollment, interview reads).
//!
//! Each device gets a *lane*: a task working through that device's jobs in
//! order, so commands to one device are never reordered, while different
//! devices -- and the bridge's main loop -- never wait on each other. Lanes
//! don't touch device state: whatever they learn or assume goes back to the
//! main loop as an `Internal` event, so state is merged and published from
//! one place only (no two tasks racing to publish different snapshots).
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, info, warn};

use super::commands::{get_keys, plan_reads, plan_set, Command, OnOffCommand};
use crate::coordinator::znp::commands::status;
use crate::coordinator::{Delivery, ZnpHandle};
use crate::devices::{Device, DeviceRegistry};
use crate::error::{Error, Result};
use crate::zigbee::zcl::clusters::ias_zone;
use crate::zigbee::zcl::frame::{
    configure_reporting_payload, read_attributes_payload, ReportingConfig,
};
use crate::zigbee::IeeeAddr;

/// Jobs queued per device before further ones are dropped (a device that
/// doesn't answer takes several seconds per command).
const LANE_CAPACITY: usize = 32;

/// Attempts for one frame when delivery fails in a recoverable way.
const MAX_SEND_ATTEMPTS: u32 = 3;
/// Pause after asking for route discovery before retrying (zigbee-herdsman
/// is recalled to wait a few seconds here too).
const ROUTE_SETTLE: Duration = Duration::from_secs(2);
/// Pause after a ZDO_NWK_ADDR_REQ, for the answer to update the registry.
const NWK_LOOKUP_SETTLE: Duration = Duration::from_secs(2);

/// Output clusters bound to the coordinator. Only command-emitting clusters
/// remote controls/switches use: binding *every* output cluster (OTA, Time,
/// Basic, ...) wastes the device's small binding table and makes it unicast
/// traffic we never handle.
const BINDABLE_OUTPUT_CLUSTERS: [u16; 4] = [0x0005, 0x0006, 0x0008, 0x0300];

const BOOLEAN: u8 = 0x10;
const BITMAP8: u8 = 0x18;
const UINT8: u8 = 0x20;
const UINT16: u8 = 0x21;
const INT16: u8 = 0x29;

const fn report(
    attr_id: u16,
    data_type: u8,
    min_interval: u16,
    max_interval: u16,
    reportable_change: Option<u32>,
) -> ReportingConfig {
    ReportingConfig {
        attr_id,
        data_type,
        min_interval,
        max_interval,
        reportable_change,
    }
}

/// Attribute reporting configured on each input cluster we publish state
/// for, bound to the coordinator first. Without this a device only tells us
/// about changes it didn't get from us (wall switch, a remote bound
/// directly to the bulb, power-on behaviour, sensor readings) if someone
/// configured it before. Intervals approximate zigbee-herdsman-converters'
/// `reporting.*` defaults (recalled, not verified): report on change, at
/// most every `min` and at least every `max` seconds. Colour temperature and
/// xy go in separate requests, as colour-temperature-only lights reject xy.
const REPORTING: &[(u16, &[ReportingConfig])] = &[
    (0x0006, &[report(0x0000, BOOLEAN, 0, 3600, None)]),
    (0x0008, &[report(0x0000, UINT8, 1, 3600, Some(1))]),
    (0x0300, &[report(0x0007, UINT16, 1, 3600, Some(1))]),
    (
        0x0300,
        &[
            report(0x0003, UINT16, 1, 3600, Some(1)),
            report(0x0004, UINT16, 1, 3600, Some(1)),
        ],
    ),
    (0x0402, &[report(0x0000, INT16, 10, 3600, Some(100))]),
    (0x0405, &[report(0x0000, UINT16, 10, 3600, Some(100))]),
    (0x0406, &[report(0x0000, BITMAP8, 0, 3600, None)]),
    (0x0001, &[report(0x0021, UINT8, 3600, 62000, Some(0))]),
];

/// Attributes read once after an interview:
/// - Basic: manufacturer, model, power source, sw build id.
/// - IAS Zone: ZoneType, so zone status reports can be classified
///   (contact/occupancy/smoke/water_leak/...).
/// - Power Configuration: battery voltage/percentage, which most devices
///   never report unsolicited without being asked.
const INTERVIEW_READS: [(u16, &[u16]); 3] = [
    (0x0000, &[0x0004, 0x0005, 0x0007, 0x4000]),
    (0x0500, &[ias_zone::ZONE_TYPE]),
    (0x0001, &[0x0020, 0x0021]),
];

/// Results from lanes (and timers) for the main loop.
#[derive(Debug)]
pub(super) enum Internal {
    /// Merge these values into the device's state and publish it (e.g. the
    /// optimistic state of a delivered `set`).
    State {
        ieee: IeeeAddr,
        values: Map<String, Value>,
    },
    /// Publish the cached state as is (`get` with nothing to read).
    PublishCached { ieee: IeeeAddr },
    /// A device setup finished (`full`: it was the post-interview one).
    /// `generation` identifies the setup, so a stale result (from before the
    /// device left and re-paired) can be told apart.
    SetupDone {
        ieee: IeeeAddr,
        full: bool,
        ok: bool,
        generation: u64,
    },
    /// A permit-join request finished.
    PermitJoin {
        duration: u8,
        ok: bool,
        /// Sent by housekeeping to keep `permit_join: true` open, rather
        /// than requested over MQTT.
        refresh: bool,
    },
    /// Time to re-read a device's state (debounced after an announce).
    RefreshDue { ieee: IeeeAddr },
}

#[derive(Debug)]
pub(super) enum Job {
    Set(Value),
    Get(Value),
    /// Re-read light state (after a power cycle, at startup).
    Refresh,
    ReadZoneType,
}

/// Shared by the main loop and all lanes.
pub(super) struct Ctx {
    pub devices: Arc<DeviceRegistry>,
    pub znp: Arc<ZnpHandle>,
    pub coordinator_ieee: Option<IeeeAddr>,
    pub internal_tx: mpsc::UnboundedSender<Internal>,
    zcl_seq: AtomicU8,
    /// One device setup at a time network-wide: each is a burst of binds and
    /// configure requests the CC2531 copes badly with in parallel.
    setup_slot: Semaphore,
    /// Devices with an interview retry loop running.
    pub interview_retries: Mutex<HashSet<IeeeAddr>>,
}

impl Ctx {
    pub fn new(
        devices: Arc<DeviceRegistry>,
        znp: Arc<ZnpHandle>,
        coordinator_ieee: Option<IeeeAddr>,
        internal_tx: mpsc::UnboundedSender<Internal>,
    ) -> Self {
        Self {
            devices,
            znp,
            coordinator_ieee,
            internal_tx,
            zcl_seq: AtomicU8::new(0),
            setup_slot: Semaphore::new(1),
            interview_retries: Mutex::new(HashSet::new()),
        }
    }

    /// Next ZCL transaction sequence number.
    pub fn next_seq(&self) -> u8 {
        self.zcl_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    fn current_nwk(&self, ieee: &IeeeAddr) -> Option<u16> {
        self.devices
            .get_by_ieee(ieee)
            .map(|d| d.nwk_addr)
            .filter(|&nwk| nwk != 0)
    }
}

/// Per-device job queues, owned by the main loop.
#[derive(Default)]
pub(super) struct Lanes {
    senders: HashMap<IeeeAddr, mpsc::Sender<Job>>,
}

impl Lanes {
    /// Queue `job` on `ieee`'s lane, starting the lane if needed.
    /// Jobs for devices no longer in the registry are dropped.
    pub fn submit(&mut self, ctx: &Arc<Ctx>, ieee: IeeeAddr, job: Job) {
        if ctx.devices.get_by_ieee(&ieee).is_none() {
            self.senders.remove(&ieee);
            return;
        }
        let job = match self.senders.get(&ieee) {
            Some(tx) => match tx.try_send(job) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(job)) => {
                    warn!("Too many queued commands for {ieee}, dropping {job:?}");
                    return;
                }
                // The lane task is gone (it panicked); start a new one.
                Err(mpsc::error::TrySendError::Closed(job)) => job,
            },
            None => job,
        };
        let (tx, rx) = mpsc::channel(LANE_CAPACITY);
        tx.try_send(job).expect("fresh channel has room");
        tokio::spawn(run_lane(Arc::clone(ctx), ieee, rx));
        self.senders.insert(ieee, tx);
    }

    /// Stop `ieee`'s lane once its queue is drained (device left).
    pub fn close(&mut self, ieee: &IeeeAddr) {
        self.senders.remove(ieee);
    }
}

async fn run_lane(ctx: Arc<Ctx>, ieee: IeeeAddr, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        // The device left meanwhile: its lane is done.
        let Some(dev) = ctx.devices.snapshot(&ieee) else {
            return;
        };
        match job {
            Job::Set(payload) => run_set(&ctx, &dev, &payload).await,
            Job::Get(payload) => run_get(&ctx, &dev, &payload).await,
            Job::Refresh => {
                // Stop at the first failure: a bulb switched off at the wall
                // would otherwise tie up in-flight slots retrying every read.
                for (ep, cluster, attrs) in plan_reads(&dev, None) {
                    if !read_attributes(&ctx, &dev, ep, cluster, &attrs).await {
                        break;
                    }
                }
            }
            Job::ReadZoneType => {
                if let Some(ep) = dev.endpoint_with_input_cluster(0x0500) {
                    read_attributes(&ctx, &dev, ep, 0x0500, &[ias_zone::ZONE_TYPE]).await;
                }
            }
        }
    }
}

async fn run_set(ctx: &Ctx, dev: &Device, payload: &Value) {
    let plan = plan_set(dev, payload);
    if plan.is_empty() {
        warn!(
            "Set for {}: nothing applicable in {payload} (device endpoints: {:?})",
            dev.friendly_name,
            dev.endpoints.iter().map(|e| e.endpoint).collect::<Vec<_>>()
        );
        return;
    }
    let mut optimistic = Map::new();
    for cmd in plan {
        let frame = cmd.command.payload(ctx.next_seq());
        // A Toggle whose ack got lost may still have been executed; sending
        // it again could toggle twice.
        let idempotent = !matches!(cmd.command, Command::OnOff(OnOffCommand::Toggle));
        match send_to_device(
            ctx,
            dev.ieee_addr,
            cmd.endpoint,
            cmd.cluster,
            frame,
            idempotent,
        )
        .await
        {
            Ok(delivery) => {
                if !idempotent {
                    // TOGGLE: the resulting state depends on what the device
                    // was in, which any cached or optimistic value may not
                    // reflect -- ask it (the answer is published like any
                    // report).
                    read_attributes(ctx, dev, cmd.endpoint, cmd.cluster, &[0x0000]).await;
                }
                if delivery == Delivery::Unconfirmed {
                    debug!(
                        "{} command to {} sent, delivery unconfirmed",
                        cmd.command.name(),
                        dev.friendly_name
                    );
                }
                optimistic.extend(cmd.optimistic);
            }
            Err(e) => warn!(
                "Failed to send {} command to {}: {e}",
                cmd.command.name(),
                dev.friendly_name
            ),
        }
    }
    // zigbee2mqtt publishes the new state once the device acknowledged the
    // command; Home Assistant's JSON light only changes its UI on that.
    if !optimistic.is_empty() {
        let _ = ctx.internal_tx.send(Internal::State {
            ieee: dev.ieee_addr,
            values: optimistic,
        });
    }
}

async fn run_get(ctx: &Ctx, dev: &Device, payload: &Value) {
    let keys = get_keys(payload);
    let reads = plan_reads(dev, keys.as_deref());
    if reads.is_empty() {
        // Nothing we know how to read (e.g. a sensor): answer from cache.
        let _ = ctx.internal_tx.send(Internal::PublishCached {
            ieee: dev.ieee_addr,
        });
        return;
    }
    // The answers arrive as Read Attributes Responses and are published
    // like any other report.
    for (ep, cluster, attrs) in reads {
        read_attributes(ctx, dev, ep, cluster, &attrs).await;
    }
}

async fn read_attributes(ctx: &Ctx, dev: &Device, ep: u8, cluster: u16, attrs: &[u16]) -> bool {
    let frame = read_attributes_payload(ctx.next_seq(), attrs);
    match send_to_device(ctx, dev.ieee_addr, ep, cluster, frame, true).await {
        Ok(_) => true,
        Err(e) => {
            warn!(
                "Failed to read cluster 0x{cluster:04X} of {}: {e}",
                dev.friendly_name
            );
            false
        }
    }
}

/// Set a device up in the background: one device at a time network-wide,
/// and outside its lane, so its (and every other device's) set/get commands
/// never queue behind a long setup. Reports `Internal::SetupDone`.
/// Returns a handle to abort it (e.g. when the device leaves).
pub(super) fn spawn_setup(
    ctx: &Arc<Ctx>,
    ieee: IeeeAddr,
    full: bool,
    generation: u64,
) -> tokio::task::AbortHandle {
    let ctx = Arc::clone(ctx);
    tokio::spawn(async move {
        let ok = {
            let _slot = ctx.setup_slot.acquire().await;
            match ctx.devices.snapshot(&ieee) {
                // Only an interviewed device can be set up (it may have
                // left and rejoined while this waited for the slot).
                Some(dev) if dev.interview_complete && !dev.endpoints.is_empty() => {
                    run_setup(&ctx, &dev, full).await
                }
                _ => false,
            }
        };
        let _ = ctx.internal_tx.send(Internal::SetupDone {
            ieee,
            full,
            ok,
            generation,
        });
    })
    .abort_handle()
}

/// Whether to configure bindings/reporting on `dev`. Xiaomi/Aqara (LUMI)
/// devices report on their own schedule, don't take kindly to standard
/// reporting configuration and zigbee2mqtt doesn't configure most of them
/// either.
pub(super) fn auto_configurable(dev: &Device) -> bool {
    let lumi = dev
        .manufacturer
        .as_deref()
        .is_some_and(|m| m.to_ascii_uppercase().starts_with("LUMI"))
        || dev
            .model
            .as_deref()
            .is_some_and(|m| m.to_ascii_lowercase().starts_with("lumi."));
    !lumi && !dev.endpoints.is_empty()
}

/// Delivery failed because the device can't be reached at all (rather than
/// e.g. refusing one attribute): no point trying its remaining setup steps.
fn unreachable(e: &Error) -> bool {
    matches!(
        e,
        Error::DeliveryFailed {
            status: status::NWK_NO_ROUTE
                | status::MAC_NO_ACK
                | status::APS_NO_ACK
                | status::MAC_TRANSACTION_EXPIRED,
            ..
        }
    )
}

/// After an interview (`full`): IAS enrollment and the interview reads
/// first -- a freshly joined sleepy device is only awake for a short while
/// -- then bindings and attribute reporting. Returns whether every step went
/// through; stops at the first sign the device is unreachable.
async fn run_setup(ctx: &Ctx, dev: &Device, full: bool) -> bool {
    let Some(coordinator) = ctx.coordinator_ieee else {
        warn!(
            "Cannot configure {}: coordinator IEEE address unknown",
            dev.friendly_name
        );
        return false;
    };
    info!("Configuring {}", dev.friendly_name);

    let mut dev = dev.clone();
    if full {
        for ep in dev
            .endpoints
            .iter()
            .filter(|e| e.input_clusters.contains(&0x0500))
        {
            // IAS Zone enrollment: point the device at us as its CIE, then
            // send an unsolicited Zone Enroll Response (devices that instead
            // send a Zone Enroll Request are answered from the main loop).
            let frame = ias_zone::write_cie_address_payload(ctx.next_seq(), coordinator.0);
            if let Err(e) =
                send_to_device(ctx, dev.ieee_addr, ep.endpoint, 0x0500, frame, true).await
            {
                warn!(
                    "Failed to write IAS CIE address to {}: {e}",
                    dev.friendly_name
                );
                return false;
            }
            if !send_enroll_response(ctx, dev.ieee_addr, ep.endpoint).await {
                return false;
            }
        }
        for (cluster, attrs) in INTERVIEW_READS {
            if let Some(ep) = dev.endpoint_with_input_cluster(cluster) {
                if !read_attributes(ctx, &dev, ep, cluster, attrs).await {
                    return false;
                }
            }
        }
        // Let the main loop record the Basic answers (manufacturer/model)
        // before deciding whether this is a device to configure.
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(fresh) = ctx.devices.snapshot(&dev.ieee_addr) {
            dev = fresh;
        }
        if dev.manufacturer.is_none() && dev.model.is_none() {
            // Basic answer not in yet: can't tell whether this is a device
            // we mustn't configure. Count as incomplete; a retry decides.
            debug!("Basic attributes of {} not known yet", dev.friendly_name);
            return false;
        }
    }
    if !auto_configurable(&dev) {
        info!(
            "Not configuring reporting on {} (LUMI device)",
            dev.friendly_name
        );
        return true;
    }

    let mut ok = true;
    for ep in &dev.endpoints {
        let endpoint = ep.endpoint;
        // Remotes: their button presses are commands on these *output*
        // clusters, which they only send to devices they're bound to.
        for &cluster in ep
            .output_clusters
            .iter()
            .filter(|c| BINDABLE_OUTPUT_CLUSTERS.contains(c))
        {
            ok &= bind(ctx, &dev, endpoint, cluster, coordinator).await;
        }

        let mut bound: Vec<u16> = Vec::new();
        for &(cluster, configs) in REPORTING {
            if !ep.input_clusters.contains(&cluster) {
                continue;
            }
            if !bound.contains(&cluster) {
                bound.push(cluster);
                if !bind(ctx, &dev, endpoint, cluster, coordinator).await {
                    ok = false;
                    continue;
                }
            }
            let frame = configure_reporting_payload(ctx.next_seq(), configs);
            if let Err(e) = send_to_device(ctx, dev.ieee_addr, endpoint, cluster, frame, true).await
            {
                warn!(
                    "Failed to configure reporting of cluster 0x{cluster:04X} on {}: {e}",
                    dev.friendly_name
                );
                if unreachable(&e) {
                    return false;
                }
                ok = false;
            }
        }
    }

    if ok {
        info!("Configured {}", dev.friendly_name);
    } else {
        warn!("Configuring {} did not fully succeed", dev.friendly_name);
    }
    ok
}

async fn bind(ctx: &Ctx, dev: &Device, endpoint: u8, cluster: u16, coordinator: IeeeAddr) -> bool {
    let Some(nwk) = ctx.current_nwk(&dev.ieee_addr) else {
        return false;
    };
    match ctx
        .znp
        .request_bind(nwk, dev.ieee_addr.0, endpoint, cluster, coordinator.0, 1)
        .await
    {
        Ok(()) => true,
        Err(e) => {
            warn!(
                "Failed to bind cluster 0x{cluster:04X} of {} ep {endpoint}: {e}",
                dev.friendly_name
            );
            false
        }
    }
}

/// Send an IAS Zone Enroll Response (zone id `DEFAULT_ZONE_ID`).
async fn send_enroll_response(ctx: &Ctx, ieee: IeeeAddr, endpoint: u8) -> bool {
    let frame = ias_zone::enroll_response_payload(ctx.next_seq(), ias_zone::DEFAULT_ZONE_ID);
    match send_to_device(ctx, ieee, endpoint, 0x0500, frame, true).await {
        Ok(_) => true,
        Err(e) => {
            warn!("Failed to send IAS zone enroll response to {ieee}: {e}");
            false
        }
    }
}

/// What to do before retrying a frame whose delivery failed with `status`
/// on attempt number `attempt` (1-based), following zigbee-herdsman's Z-Stack
/// adapter (recalled): congestion clears by itself, while a routing failure
/// first gets a route discovery and then a NWK address lookup, in case the
/// device rejoined with a new address we never heard about.
#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    GiveUp,
    Backoff(Duration),
    RediscoverRoute,
    LookUpNwkAddress,
}

fn recovery(status: u8, attempt: u32) -> Recovery {
    match status {
        status::MAC_CHANNEL_ACCESS_FAILURE
        | status::MEM_ERROR
        | status::BUFFER_FULL
        | status::MAC_TRANSACTION_OVERFLOW => {
            Recovery::Backoff(Duration::from_millis(500 * u64::from(attempt)))
        }
        status::NWK_NO_ROUTE | status::MAC_NO_ACK | status::APS_NO_ACK => match attempt {
            1 => Recovery::RediscoverRoute,
            2 => Recovery::LookUpNwkAddress,
            _ => Recovery::GiveUp,
        },
        // MAC_TRANSACTION_EXPIRED: a sleepy device didn't poll for it in
        // time; resending right away won't change that.
        _ => Recovery::GiveUp,
    }
}

/// Send a ZCL frame to `ieee`'s current address, waiting for the
/// coordinator's delivery confirmation and recovering from transient and
/// routing failures. A non-`idempotent` frame is not resent after a lost
/// acknowledgement (the device may have acted on it). Not used from the main
/// loop: this can take many seconds for an unreachable device.
pub(super) async fn send_to_device(
    ctx: &Ctx,
    ieee: IeeeAddr,
    endpoint: u8,
    cluster: u16,
    frame: Vec<u8>,
    idempotent: bool,
) -> Result<Delivery> {
    let mut attempt = 0u32;
    let mut looked_up = false;
    loop {
        let nwk = match ctx.current_nwk(&ieee) {
            Some(nwk) => nwk,
            None if !looked_up => {
                // Address unknown (never seen, or dropped as stale): ask.
                looked_up = true;
                debug!("NWK address of {ieee} unknown, looking it up");
                if let Err(e) = ctx.znp.request_nwk_addr(ieee.0).await {
                    debug!("NWK address lookup for {ieee} failed: {e}");
                }
                tokio::time::sleep(NWK_LOOKUP_SETTLE).await;
                continue;
            }
            None => return Err(Error::Znp(format!("network address of {ieee} unknown"))),
        };
        let result = ctx
            .znp
            .send_zcl(nwk, endpoint, cluster, frame.clone())
            .await;
        let status = match &result {
            Err(Error::DeliveryFailed { status, .. }) => *status,
            _ => return result,
        };
        attempt += 1;
        if attempt >= MAX_SEND_ATTEMPTS {
            return result;
        }
        match recovery(status, attempt) {
            Recovery::GiveUp => return result,
            Recovery::Backoff(delay) => {
                debug!("Delivery to {ieee} failed (0x{status:02X}), retrying in {delay:?}");
                tokio::time::sleep(delay).await;
            }
            // The device may have received it; only the ack went missing.
            _ if !idempotent && status == status::APS_NO_ACK => return result,
            Recovery::RediscoverRoute => {
                debug!(
                    "Delivery to {ieee} (0x{nwk:04X}) failed (0x{status:02X}), rediscovering route"
                );
                if let Err(e) = ctx.znp.discover_route(nwk).await {
                    debug!("Route discovery to 0x{nwk:04X} failed: {e}");
                }
                tokio::time::sleep(ROUTE_SETTLE).await;
            }
            Recovery::LookUpNwkAddress => {
                debug!("Delivery to {ieee} (0x{nwk:04X}) still failing (0x{status:02X}), looking up its NWK address");
                if let Err(e) = ctx.znp.request_nwk_addr(ieee.0).await {
                    debug!("NWK address lookup for {ieee} failed: {e}");
                }
                tokio::time::sleep(NWK_LOOKUP_SETTLE).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn congestion_backs_off_and_routing_failures_escalate() {
        assert_eq!(
            recovery(status::MAC_CHANNEL_ACCESS_FAILURE, 1),
            Recovery::Backoff(Duration::from_millis(500))
        );
        assert_eq!(recovery(status::NWK_NO_ROUTE, 1), Recovery::RediscoverRoute);
        assert_eq!(recovery(status::MAC_NO_ACK, 2), Recovery::LookUpNwkAddress);
        assert_eq!(recovery(status::APS_NO_ACK, 3), Recovery::GiveUp);
        assert_eq!(
            recovery(status::MAC_TRANSACTION_EXPIRED, 1),
            Recovery::GiveUp
        );
        assert_eq!(recovery(0x01, 1), Recovery::GiveUp);
    }

    #[test]
    fn lumi_devices_are_not_auto_configured() {
        let mut dev = Device::new(IeeeAddr([1; 8]), 0x1234);
        dev.endpoints.push(crate::zigbee::EndpointDesc {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0,
            input_clusters: vec![0x0006],
            output_clusters: vec![],
        });
        assert!(auto_configurable(&dev));
        dev.model = Some("lumi.sensor_magnet.aq2".into());
        assert!(!auto_configurable(&dev));
        dev.model = None;
        dev.manufacturer = Some("LUMI".into());
        assert!(!auto_configurable(&dev));
    }

    #[test]
    fn reporting_table_is_well_formed() {
        for (cluster, configs) in REPORTING {
            assert!(!configs.is_empty(), "cluster 0x{cluster:04X}");
            for c in *configs {
                assert!(c.min_interval <= c.max_interval);
                // Discrete types carry no reportable change, analog ones do.
                let discrete = matches!(c.data_type, BOOLEAN | BITMAP8);
                assert_eq!(
                    c.reportable_change.is_none(),
                    discrete,
                    "attr 0x{:04X}",
                    c.attr_id
                );
            }
        }
    }
}
