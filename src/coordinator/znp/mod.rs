pub mod commands;
pub mod frame;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use commands::*;
use transport::{ZnpEvent, ZnpTransport};

use crate::config::Config;
use crate::coordinator::{CoordinatorEvent, CoordinatorHandle, CoordinatorInfo};
use crate::error::{Error, Result};

pub struct ZnpCoordinator {
    transport: ZnpTransport,
    event_rx: mpsc::Receiver<ZnpEvent>,
}

impl ZnpCoordinator {
    pub fn open(port: &str, baud: u32) -> Result<Self> {
        let (transport, event_rx) = ZnpTransport::open(port, baud)?;
        Ok(Self {
            transport,
            event_rx,
        })
    }

    /// Full coordinator startup sequence.
    pub async fn start(mut self, cfg: &Config) -> Result<CoordinatorHandle> {
        self.reset().await?;
        let version = self.check_version().await?;
        let is_zstack3 = version.product_id >= 1;
        self.write_nv_config(cfg).await?;
        if is_zstack3 {
            self.configure_channel_bdb(cfg).await?;
        }
        self.register_endpoints().await?;
        self.start_network().await?;
        let device_info = self.get_device_info().await;
        info!("ZNP coordinator ready");

        let coord_info = CoordinatorInfo {
            ieee_addr: device_info.as_ref().map(|d| d.ieee_addr),
            version: format!("{}.{}", version.major_rel, version.minor_rel),
            transport_rev: version.transport_rev,
        };

        let (coord_event_tx, coord_event_rx) = mpsc::channel::<CoordinatorEvent>(64);
        let transport_clone = self.transport.clone();

        tokio::spawn(async move {
            event_pump(self.event_rx, coord_event_tx).await;
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
        self.transport
            .send(sys_reset_req(ResetType::Soft))
            .await?;

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
        let rsp = self
            .transport
            .request(sys_osal_nv_write(item_id, data))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            debug!(
                "NV write 0x{item_id:04X} returned status {:?}",
                rsp.data.first()
            );
        }
        Ok(())
    }

    async fn configure_channel_bdb(&self, cfg: &Config) -> Result<()> {
        let channel = cfg.advanced.channel;
        let channel_mask: u32 = 1 << channel;

        // Set primary channel via BDB (Z-Stack 3.0+)
        let rsp = self
            .transport
            .request(app_cnf_bdb_set_channel(channel_mask, true))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            debug!("BDB set primary channel returned: {:?}", rsp.data);
        }

        // Clear secondary channel
        let rsp = self
            .transport
            .request(app_cnf_bdb_set_channel(0, false))
            .await?;
        if rsp.data.first().copied() != Some(0) {
            debug!("BDB set secondary channel returned: {:?}", rsp.data);
        }

        info!("Zigbee channel set to {channel}");
        Ok(())
    }

    async fn register_endpoints(&self) -> Result<()> {
        // Register endpoint 1 (HA profile) – receives ZCL cluster traffic
        let input_clusters: Vec<u16> = vec![
            0x0000, 0x0001, 0x0006, 0x0008, 0x0300, 0x0400, 0x0402, 0x0405, 0x0406, 0x0500,
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

    async fn start_network(&self) -> Result<()> {
        let rsp = self
            .transport
            .request(zdo_startup_from_app(100))
            .await?;
        match rsp.data.first().copied() {
            Some(0) => info!("ZDO startup: new network formed"),
            Some(1) => info!("ZDO startup: rejoined existing network"),
            other => warn!("ZDO startup returned status {:?}", other),
        }
        // Allow the network to stabilize
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(())
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

async fn event_pump(mut znp_rx: mpsc::Receiver<ZnpEvent>, out: mpsc::Sender<CoordinatorEvent>) {
    while let Some(ev) = znp_rx.recv().await {
        let coord_event = match ev {
            ZnpEvent::EndDeviceAnnceInd(data) => {
                EndDeviceAnnceInd::parse(&data).map(|d| CoordinatorEvent::DeviceJoined {
                    ieee_addr: d.ieee_addr,
                    nwk_addr: d.nwk_addr,
                })
            }
            ZnpEvent::LeaveInd(data) => {
                LeaveInd::parse(&data).map(|d| CoordinatorEvent::DeviceLeft {
                    ieee_addr: d.ieee_addr,
                })
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
                    ieee_addr: r.ieee_addr,
                    nwk_addr: r.nwk_addr,
                })
            }
            ZnpEvent::TcDevInd(data) => {
                // TC_DEV_IND: SrcAddr(u16) + IEEEAddr(8) + ParentAddr(u16)
                if data.len() >= 10 {
                    let nwk_addr = u16::from_le_bytes([data[0], data[1]]);
                    let mut ieee = [0u8; 8];
                    ieee.copy_from_slice(&data[2..10]);
                    Some(CoordinatorEvent::AddressResolved {
                        ieee_addr: ieee,
                        nwk_addr,
                    })
                } else {
                    None
                }
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
        self.transport
            .request(zdo_ieee_addr_req(nwk_addr))
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
