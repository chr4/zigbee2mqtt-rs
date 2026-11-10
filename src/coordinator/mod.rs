pub mod znp;

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::{AdapterType, Config};
use crate::error::Result;
use crate::zigbee::IeeeAddr;
use znp::ZnpCoordinator;

pub use znp::{Delivery, ZnpHandle};

#[derive(Debug, Clone)]
pub enum CoordinatorEvent {
    DeviceJoined {
        ieee_addr: IeeeAddr,
        nwk_addr: u16,
    },
    DeviceLeft {
        ieee_addr: IeeeAddr,
    },
    /// IEEE↔NWK address resolved (from ZDO_IEEE_ADDR_RSP, ZDO_NWK_ADDR_RSP or
    /// TC_DEV_IND).
    AddressResolved {
        ieee_addr: IeeeAddr,
        nwk_addr: u16,
    },
    Message {
        src_addr: u16,
        src_ep: u8,
        /// Our endpoint the frame was addressed to.
        dst_ep: u8,
        /// Sent as a broadcast/groupcast rather than to us directly (such
        /// frames never get a ZCL Default Response).
        was_broadcast: bool,
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
    /// A device joined or re-joined through the trust center (TC_DEV_IND):
    /// a (re)paired device, which may have lost its bindings, reporting
    /// configuration and IAS enrollment.
    TrustCenterJoin {
        ieee_addr: IeeeAddr,
        nwk_addr: u16,
    },
    /// Result of a ZDO_BIND_REQ (see `ZnpHandle::request_bind`).
    BindRsp {
        nwk_addr: u16,
        status: u8,
    },
}

#[derive(Debug, Clone)]
pub struct CoordinatorInfo {
    pub ieee_addr: Option<IeeeAddr>,
    pub version: String,
    pub transport_rev: u8,
}

pub struct CoordinatorHandle {
    /// Send side. Shared by the bridge's spawned tasks; never needs locking.
    pub znp: Arc<ZnpHandle>,
    pub events: mpsc::Receiver<CoordinatorEvent>,
    pub info: CoordinatorInfo,
}

pub async fn open_coordinator(cfg: &Config) -> Result<CoordinatorHandle> {
    match cfg.serial.adapter {
        AdapterType::Znp | AdapterType::Auto => {
            let coord =
                ZnpCoordinator::open(&cfg.serial.port, cfg.serial.baudrate, cfg.serial.rtscts)?;
            coord.start(cfg).await
        }
        AdapterType::Ezsp => Err(crate::error::Error::Config(
            "EZSP adapter not yet supported".into(),
        )),
    }
}
