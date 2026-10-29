pub mod commands;
pub mod frame;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use commands::*;
use transport::{ZnpEvent, ZnpTransport};

use crate::config::Config;
use crate::coordinator::{CoordinatorEvent, CoordinatorHandle, CoordinatorInfo};
use crate::error::{Error, Result};
use crate::zigbee::IeeeAddr;

// How long to wait after ZDO_STARTUP_FROM_APP for the stack to report it's
// running as coordinator. Forming a new network includes an energy scan, so
// this is generous; it's not fatal if exceeded (see start_network()).
const NETWORK_START_TIMEOUT: Duration = Duration::from_secs(20);

pub struct ZnpCoordinator {
    transport: ZnpTransport,
    event_rx: mpsc::Receiver<ZnpEvent>,
    /// AREQs received during init that the event pump should still forward.
    early_events: Vec<ZnpEvent>,
}

impl ZnpCoordinator {
    pub fn open(port: &str, baud: u32) -> Result<Self> {
        let (transport, event_rx) = ZnpTransport::open(port, baud)?;
        Ok(Self {
            transport,
            event_rx,
            early_events: Vec::new(),
        })
    }

    /// Full coordinator startup sequence.
    pub async fn start(mut self, cfg: &Config) -> Result<CoordinatorHandle> {
        self.reset().await?;
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
            Some(state) => warn!(
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
        self.early_events.retain(|ev| {
            let late_reset = matches!(ev, ZnpEvent::ResetInd);
            if late_reset {
                warn!("Ignoring late SYS_RESET_IND from startup reset");
            }
            !late_reset
        });

        let coord_info = CoordinatorInfo {
            ieee_addr: device_info.as_ref().map(|d| IeeeAddr(d.ieee_addr)),
            version: format!("{}.{}", version.major_rel, version.minor_rel),
            transport_rev: version.transport_rev,
        };

        let (coord_event_tx, coord_event_rx) = mpsc::channel::<CoordinatorEvent>(64);
        let transport_clone = self.transport.clone();

        tokio::spawn(async move {
            event_pump(self.early_events, self.event_rx, coord_event_tx).await;
        });

        Ok(CoordinatorHandle {
            inner: Arc::new(Mutex::new(ZnpHandle {
                transport: transport_clone,
            })),
            events: coord_event_rx,
            info: coord_info,
        })
    }

    // ── Initialisation steps ─────────────────────────────────────────────────

    async fn reset(&mut self) -> Result<()> {
        info!("Resetting ZNP coordinator (soft reset)…");
        self.transport.send(sys_reset_req(ResetType::Soft)).await?;

        // Wait for SYS_RESET_IND from the device
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, self.event_rx.recv()).await {
                Ok(Some(ZnpEvent::ResetInd)) => {
                    info!("Received SYS_RESET_IND");
                    return Ok(());
                }
                Ok(Some(_)) => continue, // ignore other events during reset
                Ok(None) => return Err(Error::ChannelClosed),
                Err(_) => {
                    warn!("No SYS_RESET_IND received within timeout, continuing anyway");
                    return Ok(());
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

        // Enable pre-configured key distribution
        self.nv_write(nv::PRECFGKEYS_ENABLE, &[0x01]).await?;

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
        ZnpEvent::IeeeAddrRsp(data) => {
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
                Some(CoordinatorEvent::AddressResolved {
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

pub struct ZnpHandle {
    transport: ZnpTransport,
}

impl ZnpHandle {
    pub async fn permit_join(&self, duration: u8) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_permit_join(0xFFFC, duration))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!("PERMIT_JOIN rsp: {:?}", rsp.data);
        }
        Ok(())
    }

    pub async fn request_active_eps(&self, nwk_addr: u16) -> Result<()> {
        self.transport
            .request(zdo_active_ep_req(nwk_addr, nwk_addr))
            .await?;
        Ok(())
    }

    pub async fn request_simple_desc(&self, nwk_addr: u16, endpoint: u8) -> Result<()> {
        self.transport
            .request(zdo_simple_desc_req(nwk_addr, nwk_addr, endpoint))
            .await?;
        Ok(())
    }

    pub async fn request_ieee_addr(&self, nwk_addr: u16) -> Result<()> {
        self.transport.request(zdo_ieee_addr_req(nwk_addr)).await?;
        Ok(())
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
        self.transport
            .request(zdo_bind_req(
                dst_addr, src_ieee, src_ep, cluster_id, dst_ieee, dst_ep,
            ))
            .await?;
        Ok(())
    }

    pub async fn send_zcl(
        &self,
        dst_addr: u16,
        dst_ep: u8,
        cluster_id: u16,
        trans_id: u8,
        payload: Vec<u8>,
    ) -> Result<()> {
        let rsp = self
            .transport
            .request(af_data_request(
                dst_addr, dst_ep, 1, cluster_id, trans_id, payload,
            ))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!("AF_DATA_REQUEST status: {:?}", rsp.data);
        }
        Ok(())
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

    #[tokio::test]
    async fn early_events_are_forwarded_first() {
        let got = run_pump(vec![leave_ind(false)], vec![ZnpEvent::Other]).await;
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], CoordinatorEvent::DeviceLeft { .. }));
    }
}
