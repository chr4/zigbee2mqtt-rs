pub mod commands;
pub mod frame;
pub mod transport;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Semaphore};
use tracing::{debug, error, info, warn};

use commands::*;
use frame::ZnpFrame;
use transport::{ZnpEvent, ZnpTransport};

use crate::config::Config;
use crate::coordinator::{CoordinatorEvent, CoordinatorHandle, CoordinatorInfo};
use crate::error::{Error, Result};
use crate::zigbee::IeeeAddr;

// How long to wait after ZDO_STARTUP_FROM_APP for the stack to report it's
// running as coordinator. Forming a new network includes an energy scan, so
// this is generous; it's not fatal if exceeded (see start_network()).
const NETWORK_START_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for the AF_DATA_CONFIRM of a queued AF_DATA_REQUEST.
/// With an APS ack requested, the confirm only comes once the device acked
/// (or the stack gave up after its APS retries), which for a sleepy end
/// device waiting to poll its parent can take many seconds.
const DATA_CONFIRM_TIMEOUT: Duration = Duration::from_secs(20);

/// AF_DATA_REQUESTs allowed to await their AF_DATA_CONFIRM at once. The
/// CC2531 (Z-Stack Home 1.2) has very few buffers and starts failing frames
/// with MAC channel-access / buffer-full errors when flooded; zigbee-herdsman
/// is recalled to limit it to 2 concurrent requests (unverified) and newer
/// Z-Stack 3.x adapters to many more.
const MAX_DATA_REQUESTS_ZSTACK12: usize = 2;
const MAX_DATA_REQUESTS_ZSTACK3: usize = 8;

/// How long a fire-and-forget frame keeps its in-flight slot at most while
/// its AF_DATA_CONFIRM is outstanding.
const UNCONFIRMED_SLOT_HOLD: Duration = Duration::from_secs(3);

/// NWK radius for route discovery: twice the default maximum network depth
/// (15), Z-Stack's usual default radius.
const ROUTE_DISCOVERY_RADIUS: u8 = 30;

pub struct ZnpCoordinator {
    transport: ZnpTransport,
    event_rx: mpsc::Receiver<ZnpEvent>,
    /// AREQs received during init that the event pump should still forward.
    early_events: Vec<ZnpEvent>,
}

impl ZnpCoordinator {
    pub fn open(port: &str, baud: u32, rtscts: bool) -> Result<Self> {
        let (transport, event_rx) = ZnpTransport::open(port, baud, rtscts)?;
        Ok(Self {
            transport,
            event_rx,
            early_events: Vec::new(),
        })
    }

    /// Full coordinator startup sequence.
    pub async fn start(mut self, cfg: &Config) -> Result<CoordinatorHandle> {
        let saw_reset_ind = self.reset().await?;
        let version = self.check_version().await?;
        // Unverified heuristic, not sourced from a TI spec we have on hand:
        // product_id 0 is assumed to mean Z-Stack Home 1.2 (CC2531) and >=1
        // Z-Stack 3.x (CC2652/CC1352). If this misdetects, BDB channel
        // configuration below is silently skipped or wrongly applied -- if
        // joins fail on a given adapter, check the "ZNP version:" log line
        // logged just above against that adapter's actual stack version.
        let is_zstack3 = version.product_id >= 1;
        self.write_nv_config(cfg).await?;
        if is_zstack3 {
            self.configure_channel_bdb(cfg).await?;
        }
        self.register_endpoints().await?;
        self.start_network().await?;
        let device_info = self.get_device_info().await;
        match device_info.as_ref().and_then(|d| d.device_state) {
            Some(DEVICE_STATE_STARTED_AS_COORDINATOR) | None => {}
            Some(state) => error!(
                "Coordinator device state is 0x{state:02X}, not started as coordinator \
                 (0x{DEVICE_STATE_STARTED_AS_COORDINATOR:02X}) -- joins and messages may not work"
            ),
        }
        info!("ZNP coordinator ready");

        // A SYS_RESET_IND still buffered from the startup reset (if it was
        // slower than reset()'s timeout) must not reach the event pump, which
        // treats a reset as fatal. Everything else is kept for the bridge.
        while let Ok(ev) = self.event_rx.try_recv() {
            self.early_events.push(ev);
        }
        // At most one may be the (slow) answer to our own reset, and only if
        // reset() didn't see it; any other means the adapter reset again
        // during init and lost what we just set up.
        let mut excused = !saw_reset_ind;
        for ev in std::mem::take(&mut self.early_events) {
            if let ZnpEvent::StateChangeInd(state) = ev {
            if state != DEVICE_STATE_STARTED_AS_COORDINATOR {
                error!("Coordinator network state changed to 0x{state:02X} (not running as coordinator)");
            } else {
                info!("Coordinator running as coordinator again");
            }
            continue;
        }
        if matches!(ev, ZnpEvent::ResetInd) {
                if !excused {
                    return Err(Error::Znp("coordinator reset again during initialisation".into()));
                }
                warn!("Ignoring late SYS_RESET_IND from startup reset");
                excused = false;
                continue;
            }
            self.early_events.push(ev);
        }

        let coord_info = CoordinatorInfo {
            ieee_addr: device_info.as_ref().map(|d| IeeeAddr(d.ieee_addr)),
            version: format!("{}.{}", version.major_rel, version.minor_rel),
            transport_rev: version.transport_rev,
        };

        let (coord_event_tx, coord_event_rx) = mpsc::channel::<CoordinatorEvent>(64);
        let max_data_requests = if is_zstack3 {
            MAX_DATA_REQUESTS_ZSTACK3
        } else {
            MAX_DATA_REQUESTS_ZSTACK12
        };
        let handle = ZnpHandle::new(self.transport.clone(), max_data_requests);

        tokio::spawn(async move {
            event_pump(self.early_events, self.event_rx, coord_event_tx).await;
        });

        Ok(CoordinatorHandle {
            znp: Arc::new(handle),
            events: coord_event_rx,
            info: coord_info,
        })
    }

    // ── Initialisation steps ─────────────────────────────────────────────────

    /// Soft-reset the adapter. Returns whether its SYS_RESET_IND arrived.
    async fn reset(&mut self) -> Result<bool> {
        info!("Resetting ZNP coordinator (soft reset)…");
        self.transport.send(sys_reset_req(ResetType::Soft)).await?;

        // Wait for SYS_RESET_IND from the device
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, self.event_rx.recv()).await {
                Ok(Some(ZnpEvent::ResetInd)) => {
                    info!("Received SYS_RESET_IND");
                    return Ok(true);
                }
                Ok(Some(_)) => continue, // ignore other events during reset
                Ok(None) => return Err(Error::ChannelClosed),
                Err(_) => {
                    warn!("No SYS_RESET_IND received within timeout, continuing anyway");
                    return Ok(false);
                }
            }
        }
    }

    async fn check_version(&self) -> Result<SysVersionRsp> {
        let rsp = self.transport.request(sys_version()).await?;
        match SysVersionRsp::parse(&rsp.data) {
            Some(v) => {
                info!(
                    "ZNP version: transport_rev={} product_id={} {}.{} hw_rev={}",
                    v.transport_rev, v.product_id, v.major_rel, v.minor_rel, v.hw_rev
                );
                Ok(v)
            }
            None => Err(Error::Znp("failed to parse SYS_VERSION response".into())),
        }
    }

    /// Write network configuration to NVRAM before starting the network.
    async fn write_nv_config(&self, cfg: &Config) -> Result<()> {
        info!("Writing network configuration to NVRAM");

        // Logical type: coordinator = 0x00
        self.nv_write(nv::LOGICAL_TYPE, &[0x00]).await?;

        // PAN ID
        self.nv_write(nv::PANID, &cfg.advanced.pan_id.to_le_bytes())
            .await?;

        // Extended PAN ID
        self.nv_write(nv::EXTENDED_PAN_ID, &cfg.advanced.ext_pan_id)
            .await?;

        // Channel list (bitmask)
        let channel_mask: u32 = 1 << cfg.advanced.channel;
        self.nv_write(nv::CHANLIST, &channel_mask.to_le_bytes())
            .await?;

        // Network key
        self.nv_write(nv::PRECFGKEY, &cfg.advanced.network_key)
            .await?;

        // PRECFGKEYS_ENABLE = FALSE: the trust center distributes the real
        // network key to joining devices. TRUE means "every device already
        // has the key", and Z-Stack then sends a dummy (all-zero) key -- so
        // devices joining, or doing a trust-center rejoin after losing their
        // parent, end up without a usable key and drop off the network.
        // zigpy-znp writes False ("Z2M requires this item to be False") and
        // zigbee-herdsman's default (networkKeyDistribute: false) is 0 too.
        self.nv_write(nv::PRECFGKEYS_ENABLE, &[0x00]).await?;

        // Enable ZDO direct callbacks (so we receive device announcements etc.)
        self.nv_write(nv::ZDO_DIRECT_CB, &[0x01]).await?;

        Ok(())
    }

    async fn nv_write(&self, item_id: u16, data: &[u8]) -> Result<()> {
        let rsp = match self
            .transport
            .request(sys_osal_nv_write(item_id, data))
            .await
        {
            Ok(rsp) => rsp,
            // Firmware rejected the command outright -- tolerated like a
            // non-zero status below; timeouts/transport loss stay fatal.
            Err(e @ Error::ZnpRejected { .. }) => {
                warn!("NV write 0x{item_id:04X} rejected: {e}");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if rsp.data.first().copied() != Some(0) {
            warn!(
                "NV write 0x{item_id:04X} returned non-zero status {:?}",
                rsp.data.first()
            );
        }
        Ok(())
    }

    async fn configure_channel_bdb(&self, cfg: &Config) -> Result<()> {
        let channel = cfg.advanced.channel;
        let channel_mask: u32 = 1 << channel;

        // Set primary channel via BDB (Z-Stack 3.0+). If the product_id
        // heuristic in start() misdetected a Z-Stack 1.2 adapter, the firmware
        // rejects APP_CNF outright; the NV channel list written earlier still
        // applies, so carry on.
        let rsp = match self
            .transport
            .request(app_cnf_bdb_set_channel(channel_mask, true))
            .await
        {
            Ok(rsp) => rsp,
            Err(e @ Error::ZnpRejected { .. }) => {
                warn!("BDB channel commands not supported by this firmware ({e}); using NV channel list");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if rsp.data.first().copied() != Some(0) {
            warn!(
                "BDB set primary channel returned non-zero status: {:?}",
                rsp.data
            );
        }

        // Clear secondary channel
        let rsp = self
            .transport
            .request(app_cnf_bdb_set_channel(0, false))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!(
                "BDB set secondary channel returned non-zero status: {:?}",
                rsp.data
            );
        }

        info!("Zigbee channel set to {channel}");
        Ok(())
    }

    async fn register_endpoints(&self) -> Result<()> {
        // Register endpoint 1 (HA profile) – receives ZCL cluster traffic
        let input_clusters: Vec<u16> = vec![
            0x0000, 0x0001, 0x0005, 0x0006, 0x0008, 0x0300, 0x0400, 0x0402, 0x0405, 0x0406, 0x0500,
            0x0B04,
        ];
        let output_clusters: Vec<u16> = vec![0x0006, 0x0008, 0x0300];

        let rsp = self
            .transport
            .request(af_register(
                1,
                0x0104,
                0x0005,
                &input_clusters,
                &output_clusters,
            ))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!("AF_REGISTER returned non-zero: {:?}", rsp.data);
        }
        Ok(())
    }

    async fn start_network(&mut self) -> Result<()> {
        let rsp = self.transport.request(zdo_startup_from_app(100)).await?;
        // StartupState per zigpy-znp: 0 = restored, 1 = new, 2 = not started
        match rsp.data.first().copied() {
            Some(0) => info!("ZDO startup: restored existing network"),
            Some(1) => info!("ZDO startup: forming new network"),
            Some(2) => warn!("ZDO startup: network not started"),
            other => warn!("ZDO startup returned status {:?}", other),
        }

        // The SRSP only acknowledges the request; wait for the stack to
        // actually come up as coordinator before sending network commands.
        // Not fatal on timeout: if the stack was already running (no reset
        // happened), no state change is reported at all.
        let deadline = tokio::time::Instant::now() + NETWORK_START_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.event_rx.recv()).await {
                Ok(Some(ZnpEvent::StateChangeInd(DEVICE_STATE_STARTED_AS_COORDINATOR))) => {
                    info!("Network up, running as coordinator");
                    return Ok(());
                }
                Ok(Some(ZnpEvent::StateChangeInd(state))) => {
                    debug!("ZDO state change: 0x{state:02X}");
                }
                Ok(Some(ev)) => self.early_events.push(ev),
                Ok(None) => return Err(Error::ChannelClosed),
                Err(_) => {
                    warn!(
                        "No coordinator state change within {}s, continuing anyway",
                        NETWORK_START_TIMEOUT.as_secs()
                    );
                    return Ok(());
                }
            }
        }
    }

    async fn get_device_info(&self) -> Option<DeviceInfo> {
        match self.transport.request(util_get_device_info()).await {
            Ok(rsp) => DeviceInfo::parse(&rsp.data),
            Err(e) => {
                warn!("Failed to get device info: {e}");
                None
            }
        }
    }
}

// ── Event pump (AREQ → CoordinatorEvent) ─────────────────────────────────────

/// Forwards translated events until the transport shuts down, the bridge
/// goes away, or the coordinator resets. Returning closes the
/// `CoordinatorHandle::events` channel, which the bridge treats as fatal.
async fn event_pump(
    early_events: Vec<ZnpEvent>,
    mut znp_rx: mpsc::Receiver<ZnpEvent>,
    out: mpsc::Sender<CoordinatorEvent>,
) {
    let mut early_events = early_events.into_iter();
    loop {
        let ev = match early_events.next() {
            Some(ev) => ev,
            None => match znp_rx.recv().await {
                Some(ev) => ev,
                None => {
                    error!("ZNP transport stopped, closing coordinator event channel");
                    return;
                }
            },
        };
        if let ZnpEvent::StateChangeInd(state) = ev {
            if state != DEVICE_STATE_STARTED_AS_COORDINATOR {
                error!("Coordinator network state changed to 0x{state:02X} (not running as coordinator)");
            } else {
                info!("Coordinator running as coordinator again");
            }
            continue;
        }
        if matches!(ev, ZnpEvent::ResetInd) {
            // A reset wipes AF endpoint registration and stops the network
            // until ZDO_STARTUP_FROM_APP is sent again; nothing would arrive
            // from devices any more. Re-running init is the supervisor's job.
            error!("Coordinator reset unexpectedly, endpoint registration/network state lost");
            return;
        }
        if let Some(e) = translate_event(ev) {
            if out.send(e).await.is_err() {
                return;
            }
        }
    }
}

fn translate_event(ev: ZnpEvent) -> Option<CoordinatorEvent> {
    match ev {
        ZnpEvent::EndDeviceAnnceInd(data) => {
            EndDeviceAnnceInd::parse(&data).map(|d| CoordinatorEvent::DeviceJoined {
                ieee_addr: IeeeAddr(d.ieee_addr),
                nwk_addr: d.nwk_addr,
            })
        }
        ZnpEvent::LeaveInd(data) => {
            let ind = LeaveInd::parse(&data)?;
            let ieee_addr = IeeeAddr(ind.ieee_addr);
            if ind.rejoin {
                // Leaving to rejoin (e.g. new parent): keep the device,
                // its EndDeviceAnnceInd will update the NWK address.
                info!("Device {ieee_addr} left with rejoin flag set, keeping it");
                return None;
            }
            Some(CoordinatorEvent::DeviceLeft { ieee_addr })
        }
        ZnpEvent::AfIncomingMsg(data) => {
            AfIncomingMsg::parse(&data).map(|m| CoordinatorEvent::Message {
                src_addr: m.src_addr,
                src_ep: m.src_ep,
                dst_ep: m.dst_ep,
                was_broadcast: m.was_broadcast,
                cluster_id: m.cluster_id,
                link_quality: m.link_quality,
                data: m.data,
            })
        }
        ZnpEvent::ActiveEpRsp(data) => {
            ActiveEpRsp::parse(&data).map(|r| CoordinatorEvent::ActiveEpRsp {
                nwk_addr: r.nwk_addr,
                endpoints: r.endpoints,
            })
        }
        ZnpEvent::SimpleDescRsp(data) => {
            SimpleDescRsp::parse(&data).map(|r| CoordinatorEvent::SimpleDescRsp {
                nwk_addr: r.nwk_addr,
                endpoint: r.endpoint,
                profile_id: r.profile_id,
                device_id: r.device_id,
                input_clusters: r.input_clusters,
                output_clusters: r.output_clusters,
            })
        }
        ZnpEvent::IeeeAddrRsp(data) | ZnpEvent::NwkAddrRsp(data) => {
            IeeeAddrRsp::parse(&data).map(|r| CoordinatorEvent::AddressResolved {
                ieee_addr: IeeeAddr(r.ieee_addr),
                nwk_addr: r.nwk_addr,
            })
        }
        ZnpEvent::BindRsp(data) => BindRsp::parse(&data).map(|r| CoordinatorEvent::BindRsp {
            nwk_addr: r.nwk_addr,
            status: r.status,
        }),
        ZnpEvent::TcDevInd(data) => {
            // TC_DEV_IND: SrcAddr(u16) + IEEEAddr(8) + ParentAddr(u16)
            if data.len() >= 10 {
                let nwk_addr = u16::from_le_bytes([data[0], data[1]]);
                let mut ieee = [0u8; 8];
                ieee.copy_from_slice(&data[2..10]);
                Some(CoordinatorEvent::TrustCenterJoin {
                    ieee_addr: IeeeAddr(ieee),
                    nwk_addr,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

// ── ZnpHandle (send-side) ─────────────────────────────────────────────────────

/// Outcome of an AF_DATA_REQUEST the coordinator accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// AF_DATA_CONFIRM reported success. Frames are sent with an APS ack
    /// requested, so this means the device itself acknowledged the frame.
    Confirmed,
    /// No AF_DATA_CONFIRM arrived in time: delivery is unknown.
    Unconfirmed,
}

/// Send-side handle to the coordinator. Cheap to share (`Arc`): the
/// transport actor serializes SREQs itself, so no lock is needed around it.
pub struct ZnpHandle {
    transport: ZnpTransport,
    /// AF transaction IDs (matching AF_DATA_CONFIRMs to requests), separate
    /// from the ZCL sequence numbers inside the payloads.
    af_trans_id: AtomicU8,
    /// Per AF transaction ID: `None` while reserved, else when it was last
    /// released. Reserved IDs are never handed out, and recently released
    /// ones are avoided -- Z-Stack may still deliver a late AF_DATA_CONFIRM
    /// for them, which must not reach a new waiter.
    ids_in_use: Arc<std::sync::Mutex<[Option<std::time::Instant>; 256]>>,
    /// Bounds AF_DATA_REQUESTs waiting for their AF_DATA_CONFIRM -- every
    /// one we send, Default Responses included.
    data_slots: Arc<Semaphore>,
}

/// An AF transaction ID reserved until dropped.
struct TransId {
    id: u8,
    in_use: Arc<std::sync::Mutex<[Option<std::time::Instant>; 256]>>,
}

impl Drop for TransId {
    fn drop(&mut self) {
        self.in_use.lock().unwrap_or_else(|e| e.into_inner())[usize::from(self.id)] =
            Some(std::time::Instant::now());
    }
}

/// How long a released AF transaction ID is avoided (if any other is free).
const TRANS_ID_QUARANTINE: Duration = Duration::from_secs(60);

impl ZnpHandle {
    fn new(transport: ZnpTransport, max_data_requests: usize) -> Self {
        Self {
            transport,
            af_trans_id: AtomicU8::new(0),
            ids_in_use: Arc::new(std::sync::Mutex::new([Some(std::time::Instant::now()); 256])),
            data_slots: Arc::new(Semaphore::new(max_data_requests)),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(transport: ZnpTransport, max_data_requests: usize) -> Self {
        Self::new(transport, max_data_requests)
    }

    /// Reserve the next AF transaction ID not awaiting a confirm. At most
    /// `data_slots` IDs are reserved at a time, so one is always free.
    fn reserve_trans_id(&self) -> TransId {
        let mut ids = self.ids_in_use.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        // Next free ID out of quarantine; failing that (e.g. a burst of
        // >256 frames a minute), the free one released longest ago.
        let mut oldest: Option<(u8, std::time::Instant)> = None;
        let mut chosen = None;
        for _ in 0..256 {
            let id = self.af_trans_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if let Some(released) = ids[usize::from(id)] {
                if now.duration_since(released) >= TRANS_ID_QUARANTINE {
                    chosen = Some(id);
                    break;
                }
                if oldest.is_none_or(|(_, t)| released < t) {
                    oldest = Some((id, released));
                }
            }
        }
        // At most `data_slots` IDs are reserved, so one is always free.
        let id = chosen
            .or(oldest.map(|(id, _)| id))
            .expect("more AF transaction IDs than in-flight requests");
        ids[usize::from(id)] = None;
        TransId {
            id,
            in_use: Arc::clone(&self.ids_in_use),
        }
    }

    /// Liveness check: the coordinator answers SYS_PING itself. A hung
    /// adapter makes this time out, and enough timeouts in a row shut the
    /// transport down (and the bridge with it, for a restart).
    pub async fn ping(&self) -> Result<()> {
        self.transport.request(sys_ping()).await.map(|_| ())
    }

    pub async fn permit_join(&self, duration: u8) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_permit_join(0xFFFC, duration))
            .await?;
        check_status(&rsp, "ZDO_MGMT_PERMIT_JOIN_REQ")
    }

    pub async fn request_active_eps(&self, nwk_addr: u16) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_active_ep_req(nwk_addr, nwk_addr))
            .await?;
        check_status(&rsp, "ZDO_ACTIVE_EP_REQ")
    }

    pub async fn request_simple_desc(&self, nwk_addr: u16, endpoint: u8) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_simple_desc_req(nwk_addr, nwk_addr, endpoint))
            .await?;
        check_status(&rsp, "ZDO_SIMPLE_DESC_REQ")
    }

    pub async fn request_ieee_addr(&self, nwk_addr: u16) -> Result<()> {
        let rsp = self.transport.request(zdo_ieee_addr_req(nwk_addr)).await?;
        check_status(&rsp, "ZDO_IEEE_ADDR_REQ")
    }

    /// Look up `ieee`'s current NWK address; the answer arrives as a
    /// `CoordinatorEvent::AddressResolved`.
    pub async fn request_nwk_addr(&self, ieee: [u8; 8]) -> Result<()> {
        let rsp = self.transport.request(zdo_nwk_addr_req(ieee)).await?;
        check_status(&rsp, "ZDO_NWK_ADDR_REQ")
    }

    /// Ask the stack to rediscover the route to `nwk_addr`.
    pub async fn discover_route(&self, nwk_addr: u16) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_ext_route_disc(nwk_addr, ROUTE_DISCOVERY_RADIUS))
            .await?;
        check_status(&rsp, "ZDO_EXT_ROUTE_DISC")
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn request_bind(
        &self,
        dst_addr: u16,
        src_ieee: [u8; 8],
        src_ep: u8,
        cluster_id: u16,
        dst_ieee: [u8; 8],
        dst_ep: u8,
    ) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_bind_req(
                dst_addr, src_ieee, src_ep, cluster_id, dst_ieee, dst_ep,
            ))
            .await?;
        check_status(&rsp, "ZDO_BIND_REQ")
    }

    /// Send a ZCL frame from our endpoint 1 and wait for the coordinator's
    /// AF_DATA_CONFIRM. `Err(Error::DeliveryFailed)` if the stack refused to
    /// queue the frame or reported it undeliverable; at most
    /// `data_slots` of these are in flight at once.
    pub async fn send_zcl(
        &self,
        dst_addr: u16,
        dst_ep: u8,
        cluster_id: u16,
        payload: Vec<u8>,
    ) -> Result<Delivery> {
        let _slot = self
            .data_slots
            .acquire()
            .await
            .map_err(|_| Error::ChannelClosed)?;
        let trans_id = self.reserve_trans_id();
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let rsp = self
            .transport
            .request_with_confirm(
                af_data_request(dst_addr, dst_ep, 1, cluster_id, trans_id.id, payload),
                trans_id.id,
                confirm_tx,
            )
            .await?;
        check_data_request(dst_addr, &rsp)?;
        match tokio::time::timeout(DATA_CONFIRM_TIMEOUT, confirm_rx).await {
            Ok(Ok(status::SUCCESS)) => Ok(Delivery::Confirmed),
            Ok(Ok(status)) => Err(Error::DeliveryFailed {
                nwk: dst_addr,
                status,
            }),
            // IDs are never reused while awaited, so a dropped waiter means
            // the transport shut down.
            Ok(Err(_)) => Err(Error::ChannelClosed),
            Err(_) => {
                debug!(
                    "No AF_DATA_CONFIRM for frame to 0x{dst_addr:04X} cluster 0x{cluster_id:04X} (trans {})",
                    trans_id.id
                );
                Ok(Delivery::Unconfirmed)
            }
        }
    }

    /// Send a ZCL frame without waiting for its AF_DATA_CONFIRM (e.g. a ZCL
    /// Default Response, where nothing depends on the outcome). Only fails if
    /// the stack refused to queue it. The frame still occupies an in-flight
    /// slot until its confirm arrives (or briefly times out), so bursts of
    /// these can't flood the adapter's buffers.
    pub async fn send_zcl_unconfirmed(
        &self,
        dst_addr: u16,
        dst_ep: u8,
        cluster_id: u16,
        payload: Vec<u8>,
    ) -> Result<()> {
        let slot = Arc::clone(&self.data_slots)
            .acquire_owned()
            .await
            .map_err(|_| Error::ChannelClosed)?;
        let trans_id = self.reserve_trans_id();
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let rsp = self
            .transport
            .request_with_confirm(
                af_data_request(dst_addr, dst_ep, 1, cluster_id, trans_id.id, payload),
                trans_id.id,
                confirm_tx,
            )
            .await?;
        check_data_request(dst_addr, &rsp)?;
        tokio::spawn(async move {
            let _ = tokio::time::timeout(UNCONFIRMED_SLOT_HOLD, confirm_rx).await;
            drop((slot, trans_id));
        });
        Ok(())
    }
}

/// A ZDO/SYS SRSP whose first byte is a status: non-zero means the request
/// was not carried out (e.g. the stack was out of buffers).
fn check_status(rsp: &ZnpFrame, what: &str) -> Result<()> {
    match rsp.data.first().copied() {
        Some(0) => Ok(()),
        status => Err(Error::Znp(format!("{what} failed with status {status:02X?}"))),
    }
}

/// AF_DATA_REQUEST's SRSP status: non-zero means the frame was never queued
/// for transmission (e.g. no buffers), so it must not count as sent.
fn check_data_request(dst_addr: u16, rsp: &ZnpFrame) -> Result<()> {
    match rsp.data.first().copied() {
        Some(status::SUCCESS) => Ok(()),
        Some(status) => Err(Error::DeliveryFailed {
            nwk: dst_addr,
            status,
        }),
        None => Err(Error::Znp(format!(
            "empty AF_DATA_REQUEST response for 0x{dst_addr:04X}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leave_ind(rejoin: bool) -> ZnpEvent {
        let mut data = vec![0x34, 0x12, 1, 2, 3, 4, 5, 6, 7, 8, 0x00, 0x00];
        data.push(rejoin as u8);
        ZnpEvent::LeaveInd(data)
    }

    async fn run_pump(early: Vec<ZnpEvent>, live: Vec<ZnpEvent>) -> Vec<CoordinatorEvent> {
        let (znp_tx, znp_rx) = mpsc::channel(16);
        let (out_tx, mut out_rx) = mpsc::channel(16);
        for ev in live {
            znp_tx.send(ev).await.unwrap();
        }
        let pump = tokio::spawn(event_pump(early, znp_rx, out_tx));
        let mut got = Vec::new();
        // Ends once the pump returns and drops `out_tx` -- keep `znp_tx`
        // alive so only a fatal event (not transport shutdown) can end it.
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await
        {
            got.push(ev);
        }
        drop(znp_tx);
        pump.await.unwrap();
        got
    }

    #[tokio::test]
    async fn reset_ind_closes_event_channel() {
        let (znp_tx, znp_rx) = mpsc::channel(4);
        let (out_tx, mut out_rx) = mpsc::channel(4);
        znp_tx.send(ZnpEvent::ResetInd).await.unwrap();
        tokio::spawn(event_pump(Vec::new(), znp_rx, out_tx));
        // Channel closes even though the transport side is still open.
        assert!(out_rx.recv().await.is_none());
        drop(znp_tx);
    }

    #[tokio::test]
    async fn transport_shutdown_closes_event_channel() {
        let (znp_tx, znp_rx) = mpsc::channel::<ZnpEvent>(4);
        let (out_tx, mut out_rx) = mpsc::channel(4);
        tokio::spawn(event_pump(Vec::new(), znp_rx, out_tx));
        drop(znp_tx);
        assert!(out_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn leave_with_rejoin_is_not_forwarded() {
        let got = run_pump(Vec::new(), vec![leave_ind(true), leave_ind(false)]).await;
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], CoordinatorEvent::DeviceLeft { .. }));
    }

    // ── ZnpHandle::send_zcl ───────────────────────────────────────────────

    use transport::fake::{link, FakeCoordinator};

    fn handle(max: usize) -> (Arc<ZnpHandle>, FakeCoordinator) {
        let (transport, _events, coord) = link();
        (Arc::new(ZnpHandle::for_test(transport, max)), coord)
    }

    fn spawn_send(znp: &Arc<ZnpHandle>) -> tokio::task::JoinHandle<Result<Delivery>> {
        let znp = Arc::clone(znp);
        tokio::spawn(async move { znp.send_zcl(0x1234, 1, 0x0006, vec![0x11, 1, 1]).await })
    }

    /// AF transaction ID of an AF_DATA_REQUEST frame.
    fn af_trans_id(frame: &ZnpFrame) -> u8 {
        frame.data[6]
    }

    #[tokio::test]
    async fn send_zcl_waits_for_successful_confirm() {
        let (znp, mut coord) = handle(2);
        let send = spawn_send(&znp);
        let req = coord.recv().await;
        coord.reply(&req, vec![0x00]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!send.is_finished(), "must wait for AF_DATA_CONFIRM");
        coord.confirm(af_trans_id(&req), 0x00).await;
        assert_eq!(send.await.unwrap().unwrap(), Delivery::Confirmed);
    }

    #[tokio::test]
    async fn send_zcl_reports_failed_confirm() {
        let (znp, mut coord) = handle(2);
        let send = spawn_send(&znp);
        let req = coord.recv().await;
        coord.reply(&req, vec![0x00]).await;
        coord.confirm(af_trans_id(&req), status::NWK_NO_ROUTE).await;
        assert!(matches!(
            send.await.unwrap(),
            Err(Error::DeliveryFailed { nwk: 0x1234, status: status::NWK_NO_ROUTE })
        ));
    }

    #[tokio::test]
    async fn send_zcl_rejected_srsp_fails_without_confirm() {
        let (znp, mut coord) = handle(2);
        let send = spawn_send(&znp);
        let req = coord.recv().await;
        coord.reply(&req, vec![status::BUFFER_FULL]).await;
        assert!(matches!(
            send.await.unwrap(),
            Err(Error::DeliveryFailed { status: status::BUFFER_FULL, .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn send_zcl_without_confirm_is_unconfirmed() {
        let (znp, mut coord) = handle(2);
        let send = spawn_send(&znp);
        let req = coord.recv().await;
        coord.reply(&req, vec![0x00]).await;
        assert_eq!(send.await.unwrap().unwrap(), Delivery::Unconfirmed);
    }

    #[tokio::test]
    async fn in_flight_data_requests_are_bounded() {
        let (znp, mut coord) = handle(1);
        let first = spawn_send(&znp);
        let req1 = coord.recv().await;
        coord.reply(&req1, vec![0x00]).await;
        let second = spawn_send(&znp);
        // The second frame must not be sent while the first awaits its confirm.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), coord.recv())
                .await
                .is_err()
        );
        coord.confirm(af_trans_id(&req1), 0x00).await;
        assert_eq!(first.await.unwrap().unwrap(), Delivery::Confirmed);
        let req2 = coord.recv().await;
        assert_ne!(af_trans_id(&req1), af_trans_id(&req2));
        coord.reply(&req2, vec![0x00]).await;
        coord.confirm(af_trans_id(&req2), 0x00).await;
        assert_eq!(second.await.unwrap().unwrap(), Delivery::Confirmed);
    }

    #[tokio::test]
    async fn unconfirmed_sends_count_against_the_in_flight_limit() {
        let (znp, mut coord) = handle(1);
        let dr = {
            let znp = Arc::clone(&znp);
            tokio::spawn(async move { znp.send_zcl_unconfirmed(0x1234, 1, 0x0006, vec![0x18, 1, 0x0B, 1, 0]).await })
        };
        let req1 = coord.recv().await;
        coord.reply(&req1, vec![0x00]).await;
        dr.await.unwrap().unwrap();
        let send = spawn_send(&znp);
        // Held back until the default response's confirm frees the slot.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), coord.recv())
                .await
                .is_err()
        );
        coord.confirm(af_trans_id(&req1), 0x00).await;
        let req2 = coord.recv().await;
        coord.reply(&req2, vec![0x00]).await;
        coord.confirm(af_trans_id(&req2), 0x00).await;
        assert_eq!(send.await.unwrap().unwrap(), Delivery::Confirmed);
    }

    #[tokio::test]
    async fn trans_ids_in_use_are_skipped() {
        let (transport, _events, _coord) = link();
        let znp = ZnpHandle::for_test(transport, 2);
        let held = znp.reserve_trans_id();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..600 {
            let id = znp.reserve_trans_id().id;
            assert_ne!(id, held.id);
            seen.insert(id);
        }
        // Released IDs are reused only once nothing fresher is left.
        assert_eq!(seen.len(), 255);
    }

    #[test]
    fn nwk_addr_rsp_resolves_address() {
        let mut data = vec![0x00];
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        data.extend_from_slice(&[0x78, 0x56, 0x00, 0x00]);
        match translate_event(ZnpEvent::NwkAddrRsp(data)) {
            Some(CoordinatorEvent::AddressResolved { nwk_addr, ieee_addr }) => {
                assert_eq!(nwk_addr, 0x5678);
                assert_eq!(ieee_addr, IeeeAddr([1, 2, 3, 4, 5, 6, 7, 8]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn early_events_are_forwarded_first() {
        let got = run_pump(vec![leave_ind(false)], vec![ZnpEvent::Other]).await;
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], CoordinatorEvent::DeviceLeft { .. }));
    }
}
