/// ZNP command constants and typed request/response structures.
///
/// Reference: Texas Instruments Z-Stack Monitor and Test API
/// (swra453a – Z-Stack ZNP Interface Specification)
use crate::coordinator::znp::frame::{Subsystem, ZnpFrame};

// ─── SYS subsystem ────────────────────────────────────────────────────────────

pub mod sys {
    pub const RESET_REQ: u8 = 0x00; // AREQ (host → device)
    pub const VERSION: u8 = 0x02; // SREQ/SRSP
    pub const OSAL_NV_READ: u8 = 0x08;
    pub const OSAL_NV_WRITE: u8 = 0x09;
    pub const OSAL_NV_INIT: u8 = 0x07;
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

#[derive(Debug, Clone)]
pub struct SysVersionRsp {
    pub transport_rev: u8,
    pub product_id: u8,
    pub major_rel: u8,
    pub minor_rel: u8,
    pub hw_rev: u8,
}

impl SysVersionRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 5 {
            return None;
        }
        Some(Self {
            transport_rev: data[0],
            product_id: data[1],
            major_rel: data[2],
            minor_rel: data[3],
            hw_rev: data[4],
        })
    }
}

// ─── NV RAM helpers ──────────────────────────────────────────────────────────

/// Well-known Z-Stack NV item IDs.
pub mod nv {
    pub const STARTUP_OPTION: u16 = 0x0003;
    pub const LOGICAL_TYPE: u16 = 0x0087;
    pub const PANID: u16 = 0x0083;
    pub const CHANLIST: u16 = 0x0084;
    pub const EXTENDED_PAN_ID: u16 = 0x002D;
    pub const PRECFGKEY: u16 = 0x0062;
    pub const PRECFGKEYS_ENABLE: u16 = 0x0063;
    pub const ZDO_DIRECT_CB: u16 = 0x008F;
}

pub fn sys_osal_nv_write(item_id: u16, data: &[u8]) -> ZnpFrame {
    let mut payload = Vec::with_capacity(4 + data.len());
    payload.extend_from_slice(&item_id.to_le_bytes());
    payload.push(0); // offset
    payload.push(data.len() as u8);
    payload.extend_from_slice(data);
    ZnpFrame::sreq(Subsystem::Sys, sys::OSAL_NV_WRITE, payload)
}

pub fn sys_osal_nv_read(item_id: u16, offset: u8) -> ZnpFrame {
    let mut payload = Vec::with_capacity(3);
    payload.extend_from_slice(&item_id.to_le_bytes());
    payload.push(offset);
    ZnpFrame::sreq(Subsystem::Sys, sys::OSAL_NV_READ, payload)
}

// ─── APP_CNF subsystem ────────────────────────────────────────────────────────

pub mod app_cnf {
    pub const BDB_START_COMMISSIONING: u8 = 0x05;
    pub const BDB_SET_CHANNEL: u8 = 0x08;
}

/// Set the BDB channel mask. `is_primary` selects primary (true) or secondary.
pub fn app_cnf_bdb_set_channel(channel_mask: u32, is_primary: bool) -> ZnpFrame {
    let mut data = Vec::with_capacity(5);
    data.push(if is_primary { 1 } else { 0 });
    data.extend_from_slice(&channel_mask.to_le_bytes());
    ZnpFrame::sreq(Subsystem::AppCnf, app_cnf::BDB_SET_CHANNEL, data)
}

pub fn app_cnf_bdb_start_commissioning(mode: u8) -> ZnpFrame {
    ZnpFrame::sreq(
        Subsystem::AppCnf,
        app_cnf::BDB_START_COMMISSIONING,
        vec![mode],
    )
}

// ─── UTIL subsystem ───────────────────────────────────────────────────────────

pub mod util {
    pub const GET_DEVICE_INFO: u8 = 0x00;
    pub const CALLBACK_SUB_CMD: u8 = 0x06;
}

pub fn util_get_device_info() -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::Util, util::GET_DEVICE_INFO, vec![])
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub ieee_addr: [u8; 8],
    pub short_addr: u16,
    pub device_type: u8,
    pub device_state: u8,
    pub assoc_devices: Vec<u16>,
}

impl DeviceInfo {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[0..8]);
        let short_addr = u16::from_le_bytes([data[8], data[9]]);
        let device_type = data[10];
        let device_state = data[11];
        let n_assoc = if data.len() > 12 { data[12] as usize } else { 0 };
        let mut assoc = Vec::with_capacity(n_assoc);
        for i in 0..n_assoc {
            let base = 13 + i * 2;
            if base + 1 < data.len() {
                assoc.push(u16::from_le_bytes([data[base], data[base + 1]]));
            }
        }
        Some(Self {
            ieee_addr: ieee,
            short_addr,
            device_type,
            device_state,
            assoc_devices: assoc,
        })
    }
}

// ─── AF subsystem ─────────────────────────────────────────────────────────────

pub mod af {
    pub const REGISTER: u8 = 0x00;
    pub const DATA_REQUEST: u8 = 0x01;
}

pub fn af_register(
    endpoint: u8,
    profile_id: u16,
    device_id: u16,
    input_clusters: &[u16],
    output_clusters: &[u16],
) -> ZnpFrame {
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

pub fn af_data_request(
    dst_addr: u16,
    dst_ep: u8,
    src_ep: u8,
    cluster_id: u16,
    trans_id: u8,
    payload: Vec<u8>,
) -> ZnpFrame {
    let mut data = Vec::new();
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.push(dst_ep);
    data.push(src_ep);
    data.extend_from_slice(&cluster_id.to_le_bytes());
    data.push(trans_id);
    data.push(0x30); // options: AF_DISCV_ROUTE | AF_EN_SECURITY
    data.push(0xFF); // radius: max
    data.push(payload.len() as u8);
    data.extend(payload);
    ZnpFrame::sreq(Subsystem::Af, af::DATA_REQUEST, data)
}

#[derive(Debug, Clone)]
pub struct AfIncomingMsg {
    pub group_id: u16,
    pub cluster_id: u16,
    pub src_addr: u16,
    pub src_ep: u8,
    pub dst_ep: u8,
    pub link_quality: u8,
    pub data: Vec<u8>,
}

impl AfIncomingMsg {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        if raw.len() < 17 {
            return None;
        }
        let group_id = u16::from_le_bytes([raw[0], raw[1]]);
        let cluster_id = u16::from_le_bytes([raw[2], raw[3]]);
        let src_addr = u16::from_le_bytes([raw[4], raw[5]]);
        let src_ep = raw[6];
        let dst_ep = raw[7];
        // raw[8] = was_broadcast, raw[9] = link_quality, raw[10] = security
        let link_quality = raw[9];
        // raw[11..15] = timestamp, raw[15] = trans_seq_num
        let len = raw[16] as usize;
        if raw.len() < 17 + len {
            return None;
        }
        let data = raw[17..17 + len].to_vec();
        Some(Self {
            group_id,
            cluster_id,
            src_addr,
            src_ep,
            dst_ep,
            link_quality,
            data,
        })
    }
}

// ─── ZDO subsystem ────────────────────────────────────────────────────────────

pub mod zdo {
    pub const STARTUP_FROM_APP: u8 = 0x40;
    pub const PERMIT_JOIN_REQ: u8 = 0x36;
    pub const ACTIVE_EP_REQ: u8 = 0x05;
    pub const SIMPLE_DESC_REQ: u8 = 0x04;
    pub const MSG_CB_REGISTER: u8 = 0x3E;
}

pub fn zdo_startup_from_app(start_delay_ms: u16) -> ZnpFrame {
    ZnpFrame::sreq(
        Subsystem::Zdo,
        zdo::STARTUP_FROM_APP,
        start_delay_ms.to_le_bytes().to_vec(),
    )
}

pub fn zdo_permit_join(dst_addr: u16, duration: u8) -> ZnpFrame {
    let mut data = Vec::new();
    data.push(0x02); // addr mode: NWK address
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

/// Register for a ZDO callback by cluster ID.
pub fn zdo_msg_cb_register(cluster_id: u16) -> ZnpFrame {
    ZnpFrame::sreq(
        Subsystem::Zdo,
        zdo::MSG_CB_REGISTER,
        cluster_id.to_le_bytes().to_vec(),
    )
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
        if data.len() < 12 {
            return None;
        }
        let src_addr = u16::from_le_bytes([data[0], data[1]]);
        let nwk_addr = u16::from_le_bytes([data[2], data[3]]);
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[4..12]);
        let capabilities = if data.len() > 12 { data[12] } else { 0 };
        Some(Self {
            src_addr,
            nwk_addr,
            ieee_addr: ieee,
            capabilities,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LeaveInd {
    pub src_addr: u16,
    pub ieee_addr: [u8; 8],
}

impl LeaveInd {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 10 {
            return None;
        }
        let src_addr = u16::from_le_bytes([data[0], data[1]]);
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[2..10]);
        Some(Self {
            src_addr,
            ieee_addr: ieee,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ActiveEpRsp {
    pub nwk_addr: u16,
    pub status: u8,
    pub endpoints: Vec<u8>,
}

impl ActiveEpRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 6 {
            return None;
        }
        let status = data[2];
        let nwk_addr = u16::from_le_bytes([data[3], data[4]]);
        let count = data[5] as usize;
        if data.len() < 6 + count {
            return None;
        }
        let endpoints = data[6..6 + count].to_vec();
        Some(Self {
            nwk_addr,
            status,
            endpoints,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SimpleDescRsp {
    pub nwk_addr: u16,
    pub status: u8,
    pub endpoint: u8,
    pub profile_id: u16,
    pub device_id: u16,
    pub input_clusters: Vec<u16>,
    pub output_clusters: Vec<u16>,
}

impl SimpleDescRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        let status = data[2];
        let nwk_addr = u16::from_le_bytes([data[3], data[4]]);
        // data[5] = descriptor len
        let endpoint = data[6];
        let profile_id = u16::from_le_bytes([data[7], data[8]]);
        let device_id = u16::from_le_bytes([data[9], data[10]]);
        // data[11] high nibble = device version
        let mut pos = 12;
        let n_in = *data.get(pos)? as usize;
        pos += 1;
        let mut input_clusters = Vec::with_capacity(n_in);
        for _ in 0..n_in {
            if pos + 1 >= data.len() {
                return None;
            }
            input_clusters.push(u16::from_le_bytes([data[pos], data[pos + 1]]));
            pos += 2;
        }
        let n_out = *data.get(pos)? as usize;
        pos += 1;
        let mut output_clusters = Vec::with_capacity(n_out);
        for _ in 0..n_out {
            if pos + 1 >= data.len() {
                return None;
            }
            output_clusters.push(u16::from_le_bytes([data[pos], data[pos + 1]]));
            pos += 2;
        }
        Some(Self {
            nwk_addr,
            status,
            endpoint,
            profile_id,
            device_id,
            input_clusters,
            output_clusters,
        })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::znp::frame::FrameType;

    #[test]
    fn sys_reset_req_uses_correct_cmd1() {
        let frame = sys_reset_req(ResetType::Soft);
        assert_eq!(frame.cmd1, 0x00, "SYS_RESET_REQ cmd1 must be 0x00");
        assert_eq!(frame.subsystem, Subsystem::Sys);
        assert_eq!(frame.frame_type, FrameType::AReq);
        assert_eq!(frame.data, vec![0x01]); // Soft reset
    }

    #[test]
    fn sys_reset_req_hard() {
        let frame = sys_reset_req(ResetType::Hard);
        assert_eq!(frame.data, vec![0x00]);
    }

    #[test]
    fn sys_version_is_sreq() {
        let frame = sys_version();
        assert_eq!(frame.frame_type, FrameType::SReq);
        assert_eq!(frame.cmd1, 0x02);
    }

    #[test]
    fn nv_write_format() {
        let frame = sys_osal_nv_write(nv::PANID, &0x1A62u16.to_le_bytes());
        assert_eq!(frame.cmd1, 0x09); // SYS_OSAL_NV_WRITE
        assert_eq!(frame.data[0..2], 0x0083u16.to_le_bytes()); // item ID
        assert_eq!(frame.data[2], 0); // offset
        assert_eq!(frame.data[3], 2); // length
        assert_eq!(frame.data[4..6], 0x1A62u16.to_le_bytes()); // value
    }

    #[test]
    fn bdb_set_channel_primary() {
        let frame = app_cnf_bdb_set_channel(1 << 11, true);
        assert_eq!(frame.data[0], 1, "isPrimary should be 1 for primary channel");
        assert_eq!(frame.data.len(), 5, "should be 1 byte isPrimary + 4 bytes mask");
        let mask = u32::from_le_bytes([frame.data[1], frame.data[2], frame.data[3], frame.data[4]]);
        assert_eq!(mask, 1 << 11);
    }

    #[test]
    fn bdb_set_channel_secondary() {
        let frame = app_cnf_bdb_set_channel(0, false);
        assert_eq!(frame.data[0], 0, "isPrimary should be 0 for secondary");
    }

    #[test]
    fn af_register_format() {
        let frame = af_register(1, 0x0104, 0x0005, &[0x0000, 0x0006], &[0x0006]);
        assert_eq!(frame.cmd1, 0x00);
        assert_eq!(frame.data[0], 1); // endpoint
        assert_eq!(u16::from_le_bytes([frame.data[1], frame.data[2]]), 0x0104); // profile
        assert_eq!(frame.data[7], 2); // 2 input clusters
        assert_eq!(frame.data[12], 1); // 1 output cluster
    }

    #[test]
    fn af_data_request_format() {
        let frame = af_data_request(0x1234, 1, 1, 0x0006, 5, vec![0x11, 0x05, 0x01]);
        assert_eq!(frame.cmd1, 0x01);
        assert_eq!(u16::from_le_bytes([frame.data[0], frame.data[1]]), 0x1234);
        assert_eq!(frame.data[2], 1); // dst_ep
        assert_eq!(frame.data[3], 1); // src_ep
        assert_eq!(u16::from_le_bytes([frame.data[4], frame.data[5]]), 0x0006); // cluster_id
        assert_eq!(frame.data[6], 5); // trans_id
        assert_eq!(frame.data[7], 0x30); // options
        assert_eq!(frame.data[8], 0xFF); // radius
        assert_eq!(frame.data[9], 3); // payload length
        assert_eq!(frame.data[10..13], [0x11, 0x05, 0x01]);
    }

    #[test]
    fn parse_af_incoming_msg() {
        let mut raw = vec![0; 17];
        // cluster_id at [2..4]
        raw[2] = 0x06;
        raw[3] = 0x00; // cluster 0x0006
        // src_addr at [4..6]
        raw[4] = 0x34;
        raw[5] = 0x12; // 0x1234
        raw[6] = 1; // src_ep
        raw[7] = 1; // dst_ep
        raw[9] = 120; // link_quality
        raw[16] = 3; // data len
        raw.extend_from_slice(&[0x11, 0x01, 0x01]); // data
        let msg = AfIncomingMsg::parse(&raw).unwrap();
        assert_eq!(msg.cluster_id, 0x0006);
        assert_eq!(msg.src_addr, 0x1234);
        assert_eq!(msg.link_quality, 120);
        assert_eq!(msg.data, vec![0x11, 0x01, 0x01]);
    }

    #[test]
    fn parse_end_device_annce_ind() {
        let data = vec![
            0x34, 0x12, // src_addr
            0x56, 0x78, // nwk_addr
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // ieee
            0x8E, // capabilities
        ];
        let ind = EndDeviceAnnceInd::parse(&data).unwrap();
        assert_eq!(ind.src_addr, 0x1234);
        assert_eq!(ind.nwk_addr, 0x7856);
        assert_eq!(ind.ieee_addr, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(ind.capabilities, 0x8E);
    }

    #[test]
    fn parse_active_ep_rsp() {
        let data = vec![
            0x00, 0x00, // src_addr
            0x00, // status
            0x34, 0x12, // nwk_addr
            0x02, // count
            0x01, 0x02, // endpoints
        ];
        let rsp = ActiveEpRsp::parse(&data).unwrap();
        assert_eq!(rsp.nwk_addr, 0x1234);
        assert_eq!(rsp.endpoints, vec![0x01, 0x02]);
    }

    #[test]
    fn parse_simple_desc_rsp() {
        #[rustfmt::skip]
        let data = vec![
            0x00, 0x00, // src_addr
            0x00,       // status
            0x34, 0x12, // nwk_addr
            0x0A,       // desc len
            0x01,       // endpoint
            0x04, 0x01, // profile_id = 0x0104
            0x02, 0x01, // device_id = 0x0102
            0x00,       // device_version
            0x02,       // 2 input clusters
            0x00, 0x00, // 0x0000
            0x06, 0x00, // 0x0006
            0x01,       // 1 output cluster
            0x06, 0x00, // 0x0006
        ];
        let rsp = SimpleDescRsp::parse(&data).unwrap();
        assert_eq!(rsp.nwk_addr, 0x1234);
        assert_eq!(rsp.endpoint, 1);
        assert_eq!(rsp.profile_id, 0x0104);
        assert_eq!(rsp.device_id, 0x0102);
        assert_eq!(rsp.input_clusters, vec![0x0000, 0x0006]);
        assert_eq!(rsp.output_clusters, vec![0x0006]);
    }

    #[test]
    fn permit_join_format() {
        let frame = zdo_permit_join(0xFFFC, 254);
        assert_eq!(frame.data[0], 0x02); // addr mode
        assert_eq!(u16::from_le_bytes([frame.data[1], frame.data[2]]), 0xFFFC);
        assert_eq!(frame.data[3], 254); // duration
    }

    #[test]
    fn sys_version_rsp_parse() {
        let data = vec![20, 1, 2, 7, 1];
        let rsp = SysVersionRsp::parse(&data).unwrap();
        assert_eq!(rsp.transport_rev, 20);
        assert_eq!(rsp.product_id, 1);
        assert_eq!(rsp.major_rel, 2);
        assert_eq!(rsp.minor_rel, 7);
        assert_eq!(rsp.hw_rev, 1);
    }

    #[test]
    fn sys_version_rsp_parse_too_short() {
        assert!(SysVersionRsp::parse(&[1, 2, 3]).is_none());
    }
}
