/// ZNP command constants and typed request/response structures.
///
/// Reference: Texas Instruments Z-Stack Monitor and Test API
/// (swra453a – Z-Stack ZNP Interface Specification)
use crate::coordinator::znp::frame::{Subsystem, ZnpFrame};

// ─── SYS subsystem ────────────────────────────────────────────────────────────

pub mod sys {
    pub const RESET_REQ:  u8 = 0x09; // AREQ
    pub const RESET_IND:  u8 = 0x80; // AREQ (incoming)
    pub const VERSION:    u8 = 0x02; // SREQ/SRSP
    pub const OSAL_NV_READ:   u8 = 0x08;
    pub const OSAL_NV_WRITE:  u8 = 0x09;
    pub const OSAL_NV_LENGTH: u8 = 0x13;
}

/// Reset type for SYS_RESET_REQ
#[derive(Debug, Clone, Copy)]
pub enum ResetType {
    Hard = 0,
    Soft = 1,
}

pub fn sys_reset_req(reset_type: ResetType) -> ZnpFrame {
    ZnpFrame::areq(Subsystem::Sys, sys::RESET_REQ, vec![reset_type as u8])
}

pub fn sys_version() -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::Sys, sys::VERSION, vec![])
}

#[derive(Debug)]
pub struct SysVersionRsp {
    pub transport_rev: u8,
    pub product_id:    u8,
    pub major_rel:     u8,
    pub minor_rel:     u8,
    pub hw_rev:        u8,
}

impl SysVersionRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 5 { return None; }
        Some(Self {
            transport_rev: data[0],
            product_id:    data[1],
            major_rel:     data[2],
            minor_rel:     data[3],
            hw_rev:        data[4],
        })
    }
}

#[derive(Debug)]
pub struct SysResetInd {
    pub reason:        u8,
    pub transport_rev: u8,
    pub product_id:    u8,
    pub major_rel:     u8,
    pub minor_rel:     u8,
    pub hw_rev:        u8,
}

impl SysResetInd {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 6 { return None; }
        Some(Self {
            reason:        data[0],
            transport_rev: data[1],
            product_id:    data[2],
            major_rel:     data[3],
            minor_rel:     data[4],
            hw_rev:        data[5],
        })
    }
}

// ─── APP_CNF subsystem ────────────────────────────────────────────────────────

pub mod app_cnf {
    pub const BDB_START_COMMISSIONING: u8 = 0x00;
    pub const BDB_SET_CHANNEL:         u8 = 0x08;
    pub const SET_DEFAULT_ENDDEVICE_TIMEOUT: u8 = 0x03;
}

pub fn app_cnf_bdb_set_channel(primary: u32, secondary: u32) -> ZnpFrame {
    let mut data = Vec::with_capacity(9);
    data.push(0); // isPrimary = true
    data.extend_from_slice(&primary.to_le_bytes());
    data.extend_from_slice(&secondary.to_le_bytes());
    ZnpFrame::sreq(Subsystem::AppCnf, app_cnf::BDB_SET_CHANNEL, data)
}

pub fn app_cnf_bdb_start_commissioning(mode: u8) -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::AppCnf, app_cnf::BDB_START_COMMISSIONING, vec![mode])
}

// ─── UTIL subsystem ───────────────────────────────────────────────────────────

pub mod util {
    pub const GET_DEVICE_INFO: u8 = 0x00;
    pub const LED_CONTROL:     u8 = 0x0E;
    pub const CALLBACK_SUB_CMD: u8 = 0x06;
}

pub fn util_get_device_info() -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::Util, util::GET_DEVICE_INFO, vec![])
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub ieee_addr:    [u8; 8],
    pub short_addr:   u16,
    pub device_type:  u8,
    pub device_state: u8,
    pub assoc_devices: Vec<u16>,
}

impl DeviceInfo {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 { return None; }
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[0..8]);
        let short_addr  = u16::from_le_bytes([data[8], data[9]]);
        let device_type  = data[10];
        let device_state = data[11];
        let n_assoc = if data.len() > 12 { data[12] as usize } else { 0 };
        let mut assoc = Vec::with_capacity(n_assoc);
        for i in 0..n_assoc {
            let base = 13 + i * 2;
            if base + 1 < data.len() {
                assoc.push(u16::from_le_bytes([data[base], data[base + 1]]));
            }
        }
        Some(Self { ieee_addr: ieee, short_addr, device_type, device_state, assoc_devices: assoc })
    }
}

// ─── AF subsystem ─────────────────────────────────────────────────────────────

pub mod af {
    pub const REGISTER:      u8 = 0x00;
    pub const DATA_REQUEST:  u8 = 0x01;
    pub const DATA_REQUEST_EXT: u8 = 0x02;
    pub const INCOMING_MSG:  u8 = 0x81; // AREQ
    pub const DATA_CONFIRM:  u8 = 0x05; // AREQ
}

pub fn af_register(endpoint: u8, profile_id: u16, device_id: u16, input_clusters: &[u16], output_clusters: &[u16]) -> ZnpFrame {
    let mut data = Vec::new();
    data.push(endpoint);
    data.extend_from_slice(&profile_id.to_le_bytes());
    data.extend_from_slice(&device_id.to_le_bytes());
    data.push(0); // device version
    data.push(0); // latency (no latency)
    data.push(input_clusters.len() as u8);
    for &c in input_clusters {
        data.extend_from_slice(&c.to_le_bytes());
    }
    data.push(output_clusters.len() as u8);
    for &c in output_clusters {
        data.extend_from_slice(&c.to_le_bytes());
    }
    ZnpFrame::sreq(Subsystem::Af, af::REGISTER, data)
}

pub fn af_data_request(dst_addr: u16, dst_ep: u8, src_ep: u8, cluster_id: u16, trans_id: u8, payload: Vec<u8>) -> ZnpFrame {
    let mut data = Vec::new();
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.push(dst_ep);
    data.push(src_ep);
    data.extend_from_slice(&cluster_id.to_le_bytes());
    data.push(trans_id);
    data.push(0x30); // options: AF_DISCV_ROUTE
    data.push(0xFF); // radius: 0xFF = max
    data.push(payload.len() as u8);
    data.extend(payload);
    ZnpFrame::sreq(Subsystem::Af, af::DATA_REQUEST, data)
}

#[derive(Debug, Clone)]
pub struct AfIncomingMsg {
    pub group_id:   u16,
    pub cluster_id: u16,
    pub src_addr:   u16,
    pub src_ep:     u8,
    pub dst_ep:     u8,
    pub link_quality: u8,
    pub security:   u8,
    pub timestamp:  u32,
    pub trans_seq_num: u8,
    pub data:       Vec<u8>,
}

impl AfIncomingMsg {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        if raw.len() < 17 { return None; }
        let group_id   = u16::from_le_bytes([raw[0], raw[1]]);
        let cluster_id = u16::from_le_bytes([raw[2], raw[3]]);
        let src_addr   = u16::from_le_bytes([raw[4], raw[5]]);
        let src_ep     = raw[6];
        let dst_ep     = raw[7];
        // raw[8..11] = was_broadcast u8, link_quality u8, security u8
        let link_quality = raw[9];
        let security   = raw[10];
        let timestamp  = u32::from_le_bytes([raw[11], raw[12], raw[13], raw[14]]);
        let trans_seq_num = raw[15];
        let len        = raw[16] as usize;
        if raw.len() < 17 + len { return None; }
        let data = raw[17..17 + len].to_vec();
        Some(Self { group_id, cluster_id, src_addr, src_ep, dst_ep, link_quality, security, timestamp, trans_seq_num, data })
    }
}

// ─── ZDO subsystem ────────────────────────────────────────────────────────────

pub mod zdo {
    pub const STARTUP_FROM_APP:   u8 = 0x40; // SREQ
    pub const PERMIT_JOIN_REQ:    u8 = 0x36; // SREQ
    pub const ACTIVE_EP_REQ:      u8 = 0x05; // SREQ
    pub const SIMPLE_DESC_REQ:    u8 = 0x04; // SREQ
    pub const NODE_DESC_REQ:      u8 = 0x02; // SREQ
    pub const END_DEVICE_ANNCE_IND: u8 = 0xC1; // AREQ
    pub const LEAVE_IND:          u8 = 0xC9; // AREQ
    pub const ACTIVE_EP_RSP:      u8 = 0x85; // AREQ (callback)
    pub const SIMPLE_DESC_RSP:    u8 = 0x84; // AREQ
    pub const NODE_DESC_RSP:      u8 = 0x82; // AREQ
    pub const STATE_CHANGE_IND:   u8 = 0xC0; // AREQ
    pub const TC_DEV_IND:         u8 = 0xCA; // AREQ (trust centre)
}

pub fn zdo_startup_from_app(start_delay_ms: u16) -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::Zdo, zdo::STARTUP_FROM_APP, start_delay_ms.to_le_bytes().to_vec())
}

pub fn zdo_permit_join(dst_addr: u16, duration: u8) -> ZnpFrame {
    let mut data = Vec::new();
    data.push(0x02); // addr mode: 0x02 = NWK
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.push(duration);
    data.push(0); // tc_significance
    ZnpFrame::sreq(Subsystem::Zdo, zdo::PERMIT_JOIN_REQ, data)
}

pub fn zdo_active_ep_req(dst_addr: u16, nwk_addr_of_interest: u16) -> ZnpFrame {
    let mut data = Vec::new();
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.extend_from_slice(&nwk_addr_of_interest.to_le_bytes());
    ZnpFrame::sreq(Subsystem::Zdo, zdo::ACTIVE_EP_REQ, data)
}

pub fn zdo_simple_desc_req(dst_addr: u16, nwk_addr_of_interest: u16, endpoint: u8) -> ZnpFrame {
    let mut data = Vec::new();
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.extend_from_slice(&nwk_addr_of_interest.to_le_bytes());
    data.push(endpoint);
    ZnpFrame::sreq(Subsystem::Zdo, zdo::SIMPLE_DESC_REQ, data)
}

#[derive(Debug, Clone)]
pub struct EndDeviceAnnceInd {
    pub src_addr: u16,
    pub nwk_addr: u16,
    pub ieee_addr: [u8; 8],
    pub capabilities: u8,
}

impl EndDeviceAnnceInd {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 { return None; }
        let src_addr = u16::from_le_bytes([data[0], data[1]]);
        let nwk_addr = u16::from_le_bytes([data[2], data[3]]);
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[4..12]);
        let capabilities = if data.len() > 12 { data[12] } else { 0 };
        Some(Self { src_addr, nwk_addr, ieee_addr: ieee, capabilities })
    }
}

#[derive(Debug, Clone)]
pub struct LeaveInd {
    pub src_addr:  u16,
    pub ieee_addr: [u8; 8],
    pub request:   bool,
    pub remove_children: bool,
    pub rejoin:    bool,
}

impl LeaveInd {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 11 { return None; }
        let src_addr = u16::from_le_bytes([data[0], data[1]]);
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[2..10]);
        let flags = data[10];
        Some(Self {
            src_addr,
            ieee_addr: ieee,
            request:         (flags & 0x40) != 0,
            remove_children: (flags & 0x20) != 0,
            rejoin:          (flags & 0x80) != 0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ActiveEpRsp {
    pub src_addr:  u16,
    pub status:    u8,
    pub nwk_addr:  u16,
    pub endpoints: Vec<u8>,
}

impl ActiveEpRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 6 { return None; }
        let src_addr = u16::from_le_bytes([data[0], data[1]]);
        let status   = data[2];
        let nwk_addr = u16::from_le_bytes([data[3], data[4]]);
        let count    = data[5] as usize;
        if data.len() < 6 + count { return None; }
        let endpoints = data[6..6 + count].to_vec();
        Some(Self { src_addr, status, nwk_addr, endpoints })
    }
}

#[derive(Debug, Clone)]
pub struct SimpleDescRsp {
    pub src_addr:       u16,
    pub status:         u8,
    pub nwk_addr:       u16,
    pub endpoint:       u8,
    pub profile_id:     u16,
    pub device_id:      u16,
    pub input_clusters: Vec<u16>,
    pub output_clusters: Vec<u16>,
}

impl SimpleDescRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 { return None; }
        let src_addr  = u16::from_le_bytes([data[0], data[1]]);
        let status    = data[2];
        let nwk_addr  = u16::from_le_bytes([data[3], data[4]]);
        // data[5] = descriptor len
        let endpoint   = data[6];
        let profile_id = u16::from_le_bytes([data[7], data[8]]);
        let device_id  = u16::from_le_bytes([data[9], data[10]]);
        let _device_ver = data[11] >> 4;
        let mut pos = 12;
        let n_in = *data.get(pos)? as usize;
        pos += 1;
        let mut input_clusters = Vec::with_capacity(n_in);
        for _ in 0..n_in {
            if pos + 1 >= data.len() { return None; }
            input_clusters.push(u16::from_le_bytes([data[pos], data[pos + 1]]));
            pos += 2;
        }
        let n_out = *data.get(pos)? as usize;
        pos += 1;
        let mut output_clusters = Vec::with_capacity(n_out);
        for _ in 0..n_out {
            if pos + 1 >= data.len() { return None; }
            output_clusters.push(u16::from_le_bytes([data[pos], data[pos + 1]]));
            pos += 2;
        }
        Some(Self { src_addr, status, nwk_addr, endpoint, profile_id, device_id, input_clusters, output_clusters })
    }
}
