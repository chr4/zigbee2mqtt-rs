pub mod commands;
pub mod frame;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use commands::*;
use transport::{ZnpEvent, ZnpTransport};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::coordinator::{CoordinatorEvent, CoordinatorHandle};

pub struct ZnpCoordinator {
    transport: ZnpTransport,
    event_rx:  mpsc::Receiver<ZnpEvent>,
}

impl ZnpCoordinator {
    /// Open the serial port and do NOT yet initialise the network.
    pub fn open(port: &str, baud: u32) -> Result<Self> {
        let (transport, event_rx) = ZnpTransport::open(port, baud)?;
        Ok(Self { transport, event_rx })
    }

    /// Full coordinator startup sequence.
    pub async fn start(mut self, cfg: &Config) -> Result<CoordinatorHandle> {
        self.reset().await?;
        self.check_version().await?;
        self.configure_network(cfg).await?;
        self.register_endpoints().await?;
        self.start_network().await?;
        info!("ZNP coordinator ready");

        let (coord_event_tx, coord_event_rx) = mpsc::channel::<CoordinatorEvent>(64);
        let transport_clone = self.transport.clone();

        // Spawn the event-pumping task
        tokio::spawn(async move {
            event_pump(self.event_rx, coord_event_tx).await;
        });

        Ok(CoordinatorHandle {
            inner: Arc::new(Mutex::new(ZnpHandle { transport: transport_clone })),
            events: coord_event_rx,
        })
    }

    // ── Initialisation steps ─────────────────────────────────────────────────

    async fn reset(&self) -> Result<()> {
        info!("Resetting ZNP coordinator (soft reset)…");
        self.transport.send(sys_reset_req(ResetType::Soft)).await?;
        // Wait for SYS_RESET_IND (we'll get it as an event, but just pause here)
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(())
    }

    async fn check_version(&self) -> Result<()> {
        let rsp = self.transport.request(sys_version()).await?;
        if let Some(v) = SysVersionRsp::parse(&rsp.data) {
            info!(
                "ZNP version: transport_rev={} product_id={} {}.{}",
                v.transport_rev, v.product_id, v.major_rel, v.minor_rel
            );
        }
        Ok(())
    }

    async fn configure_network(&self, cfg: &Config) -> Result<()> {
        let channel = cfg.advanced.channel;
        // channel N → bitmask bit N
        let channel_mask: u32 = 1 << channel;
        let rsp = self.transport
            .request(app_cnf_bdb_set_channel(channel_mask, 0))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!("BDB set channel returned non-zero status: {:?}", rsp.data);
        }
        info!("Zigbee channel set to {channel}");
        Ok(())
    }

    async fn register_endpoints(&self) -> Result<()> {
        // Register endpoint 1 (HA profile) – receives all ZCL cluster traffic
        let input_clusters: Vec<u16>  = vec![0x0000, 0x0006, 0x0008, 0x0300,
                                              0x0400, 0x0402, 0x0405, 0x0406,
                                              0x0500];
        let output_clusters: Vec<u16> = vec![0x0006, 0x0008, 0x0300];

        let rsp = self.transport
            .request(af_register(1, 0x0104, 0x0005, &input_clusters, &output_clusters))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!("AF_REGISTER returned non-zero: {:?}", rsp.data);
        }
        Ok(())
    }

    async fn start_network(&self) -> Result<()> {
        let rsp = self.transport.request(zdo_startup_from_app(100)).await?;
        match rsp.data.first().copied() {
            Some(0) => info!("ZDO startup: new network formed"),
            Some(1) => info!("ZDO startup: rejoined existing network"),
            other   => warn!("ZDO startup returned status {:?}", other),
        }
        Ok(())
    }
}

// ── Event pump (AREQ → CoordinatorEvent) ─────────────────────────────────────

async fn event_pump(mut znp_rx: mpsc::Receiver<ZnpEvent>, out: mpsc::Sender<CoordinatorEvent>) {
    while let Some(ev) = znp_rx.recv().await {
        let coord_event = match ev {
            ZnpEvent::EndDeviceAnnceInd(data) => {
                EndDeviceAnnceInd::parse(&data).map(|d| CoordinatorEvent::DeviceJoined {
                    ieee_addr: d.ieee_addr,
                    nwk_addr:  d.nwk_addr,
                })
            }
            ZnpEvent::LeaveInd(data) => {
                LeaveInd::parse(&data).map(|d| CoordinatorEvent::DeviceLeft {
                    ieee_addr: d.ieee_addr,
                    nwk_addr:  d.src_addr,
                })
            }
            ZnpEvent::AfIncomingMsg(data) => {
                AfIncomingMsg::parse(&data).map(|m| CoordinatorEvent::Message {
                    src_addr:   m.src_addr,
                    src_ep:     m.src_ep,
                    cluster_id: m.cluster_id,
                    link_quality: m.link_quality,
                    data:       m.data,
                })
            }
            ZnpEvent::ActiveEpRsp(data) => {
                ActiveEpRsp::parse(&data).map(|r| CoordinatorEvent::ActiveEpRsp {
                    nwk_addr:  r.nwk_addr,
                    endpoints: r.endpoints,
                })
            }
            ZnpEvent::SimpleDescRsp(data) => {
                SimpleDescRsp::parse(&data).map(|r| CoordinatorEvent::SimpleDescRsp {
                    nwk_addr:        r.nwk_addr,
                    endpoint:        r.endpoint,
                    profile_id:      r.profile_id,
                    input_clusters:  r.input_clusters,
                    output_clusters: r.output_clusters,
                })
            }
            _ => None,
        };

        if let Some(e) = coord_event {
            if out.send(e).await.is_err() {
                break;
            }
        }
    }
}

// ── ZnpHandle (send-side) ─────────────────────────────────────────────────────

pub struct ZnpHandle {
    transport: ZnpTransport,
}

impl ZnpHandle {
    pub async fn permit_join(&self, duration: u8) -> Result<()> {
        // 0xFFFC = broadcast coordinator + routers
        let rsp = self.transport.request(zdo_permit_join(0xFFFC, duration)).await?;
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

    pub async fn send_zcl(&self, dst_addr: u16, dst_ep: u8, cluster_id: u16, trans_id: u8, payload: Vec<u8>) -> Result<()> {
        let rsp = self.transport
            .request(af_data_request(dst_addr, dst_ep, 1, cluster_id, trans_id, payload))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            warn!("AF_DATA_REQUEST status: {:?}", rsp.data);
        }
        Ok(())
    }
}
