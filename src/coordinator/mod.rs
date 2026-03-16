pub mod znp;

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::config::{AdapterType, Config};
use crate::error::Result;
use znp::{ZnpCoordinator, ZnpHandle};

#[derive(Debug, Clone)]
pub enum CoordinatorEvent {
    DeviceJoined {
        ieee_addr: [u8; 8],
        nwk_addr: u16,
    },
    DeviceLeft {
        ieee_addr: [u8; 8],
    },
    /// IEEE↔NWK address resolved (from ZDO_IEEE_ADDR_RSP or TC_DEV_IND).
    AddressResolved {
        ieee_addr: [u8; 8],
        nwk_addr: u16,
    },
    Message {
        src_addr: u16,
        src_ep: u8,
        cluster_id: u16,
        link_quality: u8,
        data: Vec<u8>,
    },
    ActiveEpRsp {
        nwk_addr: u16,
        endpoints: Vec<u8>,
    },
    SimpleDescRsp {
        nwk_addr: u16,
        endpoint: u8,
        profile_id: u16,
        device_id: u16,
        input_clusters: Vec<u16>,
        output_clusters: Vec<u16>,
    },
}

#[derive(Debug, Clone)]
pub struct CoordinatorInfo {
    pub ieee_addr: Option<[u8; 8]>,
    pub version: String,
    pub transport_rev: u8,
}

pub struct CoordinatorHandle {
    pub inner: Arc<Mutex<ZnpHandle>>,
    pub events: mpsc::Receiver<CoordinatorEvent>,
    pub info: CoordinatorInfo,
}

impl CoordinatorHandle {
    pub async fn permit_join(&self, duration: u8) -> Result<()> {
        self.inner.lock().await.permit_join(duration).await
    }

    pub async fn send_zcl(
        &self,
        dst_addr: u16,
        dst_ep: u8,
        cluster_id: u16,
        trans_id: u8,
        payload: Vec<u8>,
    ) -> Result<()> {
        self.inner
            .lock()
            .await
            .send_zcl(dst_addr, dst_ep, cluster_id, trans_id, payload)
            .await
    }

    pub async fn request_active_eps(&self, nwk_addr: u16) -> Result<()> {
        self.inner.lock().await.request_active_eps(nwk_addr).await
    }

    pub async fn request_simple_desc(&self, nwk_addr: u16, endpoint: u8) -> Result<()> {
        self.inner
            .lock()
            .await
            .request_simple_desc(nwk_addr, endpoint)
            .await
    }

    pub async fn request_ieee_addr(&self, nwk_addr: u16) -> Result<()> {
        self.inner
            .lock()
            .await
            .request_ieee_addr(nwk_addr)
            .await
    }
}

pub async fn open_coordinator(cfg: &Config) -> Result<CoordinatorHandle> {
    match cfg.serial.adapter {
        AdapterType::Znp | AdapterType::Auto => {
            let coord = ZnpCoordinator::open(&cfg.serial.port, cfg.serial.baudrate)?;
            coord.start(cfg).await
        }
        AdapterType::Ezsp => {
            unimplemented!("EZSP adapter support is not yet implemented");
        }
    }
}
