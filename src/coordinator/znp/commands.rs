/// ZNP command constants and typed request/response structures.
///
/// Reference: Texas Instruments Z-Stack Monitor and Test API
/// (swra453a – Z-Stack ZNP Interface Specification)
use crate::coordinator::znp::frame::{Subsystem, ZnpFrame};

// ─── SYS subsystem ────────────────────────────────────────────────────────────

mod sys {
    pub const RESET_REQ: u8 = 0x00;
    pub const PING: u8 = 0x01;
    pub const VERSION: u8 = 0x02;
    pub const OSAL_NV_WRITE: u8 = 0x09;
}

#[derive(Debug, Clone, Copy)]
pub enum ResetType {
    Soft = 1,
}

pub fn sys_reset_req(reset_type: ResetType) -> ZnpFrame {
    ZnpFrame::areq(Subsystem::Sys, sys::RESET_REQ, vec![reset_type as u8])
}

/// SYS_PING: answered by the coordinator itself; used as a liveness check.
pub fn sys_ping() -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::Sys, sys::PING, vec![])
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

pub mod nv {
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

// ─── APP_CNF subsystem ────────────────────────────────────────────────────────

mod app_cnf {
    pub const BDB_SET_CHANNEL: u8 = 0x08;
}

pub fn app_cnf_bdb_set_channel(channel_mask: u32, is_primary: bool) -> ZnpFrame {
    let mut data = Vec::with_capacity(5);
    data.push(u8::from(is_primary));
    data.extend_from_slice(&channel_mask.to_le_bytes());
    ZnpFrame::sreq(Subsystem::AppCnf, app_cnf::BDB_SET_CHANNEL, data)
}

// ─── UTIL subsystem ───────────────────────────────────────────────────────────

mod util {
    pub const GET_DEVICE_INFO: u8 = 0x00;
}

pub fn util_get_device_info() -> ZnpFrame {
    ZnpFrame::sreq(Subsystem::Util, util::GET_DEVICE_INFO, vec![])
}

/// ZDO device state reported once the stack is up as coordinator
/// (zigpy-znp `DeviceState.StartedAsCoordinator`).
pub const DEVICE_STATE_STARTED_AS_COORDINATOR: u8 = 0x09;

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub ieee_addr: [u8; 8],
    pub device_state: Option<u8>,
}

impl DeviceInfo {
    /// Layout per zigpy-znp's `UTIL.GetDeviceInfo` response: Status(u8)
    /// IEEE(8) NWK(u16) DeviceType(u8) DeviceState(u8) AssociatedDevices.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 9 || data[0] != 0 {
            return None;
        }
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[1..9]);
        Some(Self {
            ieee_addr: ieee,
            device_state: data.get(12).copied(),
        })
    }
}

// ─── AF subsystem ─────────────────────────────────────────────────────────────

mod af {
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
    data.push(0); // latency
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
    // Options: APS ack requested, so AF_DATA_CONFIRM reports end-to-end
    // delivery. NOT 0x20: that bit is SUPPRESS_ROUTE_DISC_NETWORK (zigpy-znp
    // `TransmitOptions`), which stops the stack from finding a new route to
    // a device whose old one broke -- it then stays unreachable.
    data.push(0x10);
    data.push(0xFF); // radius: max
    data.push(payload.len() as u8);
    data.extend(payload);
    ZnpFrame::sreq(Subsystem::Af, af::DATA_REQUEST, data)
}

/// AF_INCOMING_MSG: GroupID(u16) ClusterID(u16) SrcAddr(u16) SrcEndpoint(u8)
/// DstEndpoint(u8) WasBroadcast(u8) LinkQuality(u8) SecurityUse(u8)
/// TimeStamp(u32) TransSeqNumber(u8) Len(u8) Data.
#[derive(Debug, Clone)]
pub struct AfIncomingMsg {
    pub cluster_id: u16,
    pub src_addr: u16,
    pub src_ep: u8,
    pub dst_ep: u8,
    pub was_broadcast: bool,
    pub link_quality: u8,
    pub data: Vec<u8>,
}

impl AfIncomingMsg {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        if raw.len() < 17 {
            return None;
        }
        let cluster_id = u16::from_le_bytes([raw[2], raw[3]]);
        let src_addr = u16::from_le_bytes([raw[4], raw[5]]);
        let src_ep = raw[6];
        let dst_ep = raw[7];
        let was_broadcast = raw[8] != 0;
        let link_quality = raw[9];
        let len = raw[16] as usize;
        if raw.len() < 17 + len {
            return None;
        }
        let data = raw[17..17 + len].to_vec();
        Some(Self {
            cluster_id,
            src_addr,
            src_ep,
            dst_ep,
            was_broadcast,
            link_quality,
            data,
        })
    }
}

/// Z-Stack `ZStatus` values that can come back in an AF_DATA_CONFIRM (or a
/// failed AF_DATA_REQUEST SRSP), per Z-Stack's ZComDef.h / zigpy-znp's
/// `Status` enum (recalled, not checked against the header here).
pub mod status {
    pub const SUCCESS: u8 = 0x00;
    pub const MEM_ERROR: u8 = 0x10;
    pub const BUFFER_FULL: u8 = 0x11;
    pub const APS_NO_ACK: u8 = 0xB7;
    pub const NWK_NO_ROUTE: u8 = 0xCD;
    pub const MAC_CHANNEL_ACCESS_FAILURE: u8 = 0xE1;
    pub const MAC_NO_ACK: u8 = 0xE9;
    pub const MAC_TRANSACTION_EXPIRED: u8 = 0xF0;
    pub const MAC_TRANSACTION_OVERFLOW: u8 = 0xF1;
}

/// Human-readable name for a Z-Stack delivery status.
pub fn status_name(code: u8) -> &'static str {
    match code {
        status::SUCCESS => "success",
        0x01 => "failure",
        0x02 => "invalid parameter",
        status::MEM_ERROR => "out of memory",
        status::BUFFER_FULL => "buffer full",
        0xB1 => "APS failure",
        status::APS_NO_ACK => "no APS ack",
        0xC2 => "invalid NWK request",
        0xC8 => "unknown NWK device",
        0xCC => "no NWK ack",
        status::NWK_NO_ROUTE => "no route",
        status::MAC_CHANNEL_ACCESS_FAILURE => "MAC channel access failure",
        status::MAC_NO_ACK => "no MAC ack",
        status::MAC_TRANSACTION_EXPIRED => "MAC transaction expired (device asleep)",
        status::MAC_TRANSACTION_OVERFLOW => "MAC transaction queue full",
        _ => "unknown status",
    }
}

// ─── ZDO subsystem ────────────────────────────────────────────────────────────

mod zdo {
    pub const NWK_ADDR_REQ: u8 = 0x00;
    pub const IEEE_ADDR_REQ: u8 = 0x01;
    pub const SIMPLE_DESC_REQ: u8 = 0x04;
    pub const ACTIVE_EP_REQ: u8 = 0x05;
    pub const BIND_REQ: u8 = 0x21;
    pub const PERMIT_JOIN_REQ: u8 = 0x36;
    pub const STARTUP_FROM_APP: u8 = 0x40;
    pub const EXT_ROUTE_DISC: u8 = 0x45;
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
    // Broadcast destinations (0xFFF8..=0xFFFF) need AddrMode Broadcast (0x0F),
    // as zigpy-znp sends; plain NWK mode (0x02) is for unicast.
    data.push(if dst_addr >= 0xFFF8 { 0x0F } else { 0x02 });
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

/// Request IEEE address for a given NWK address.
pub fn zdo_ieee_addr_req(nwk_addr: u16) -> ZnpFrame {
    let mut data = Vec::new();
    data.extend_from_slice(&nwk_addr.to_le_bytes());
    data.push(0x00); // ReqType: single device response
    data.push(0x00); // StartIndex
    ZnpFrame::sreq(Subsystem::Zdo, zdo::IEEE_ADDR_REQ, data)
}

/// Look up the current NWK address of `ieee` (ZDO_NWK_ADDR_REQ, single device
/// response). The answer arrives as a ZDO_NWK_ADDR_RSP AREQ, laid out like
/// ZDO_IEEE_ADDR_RSP. Used to recover when a device changed its NWK address
/// without us seeing its announcement.
pub fn zdo_nwk_addr_req(ieee: [u8; 8]) -> ZnpFrame {
    let mut data = Vec::with_capacity(10);
    data.extend_from_slice(&ieee);
    data.push(0x00); // ReqType: single device response
    data.push(0x00); // StartIndex
    ZnpFrame::sreq(Subsystem::Zdo, zdo::NWK_ADDR_REQ, data)
}

/// Ask the stack to (re)discover a route to `dst_addr` (ZDO_EXT_ROUTE_DISC:
/// DstAddr, Options, Radius), as zigbee-herdsman does before retrying a
/// failed delivery.
pub fn zdo_ext_route_disc(dst_addr: u16, radius: u8) -> ZnpFrame {
    let mut data = Vec::with_capacity(4);
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.push(0x00); // options
    data.push(radius);
    ZnpFrame::sreq(Subsystem::Zdo, zdo::EXT_ROUTE_DISC, data)
}

/// Bind `cluster_id` on `src_ieee`'s `src_ep` to `dst_ieee`'s `dst_ep`
/// (normally the coordinator's own IEEE address + endpoint). Devices whose
/// relevant clusters are *output* clusters (e.g. remote controls, whose
/// button presses are genOnOff/genLevelCtrl/genScenes commands) only ever
/// unicast those commands to a device they're bound to -- without this,
/// they have nowhere to send commands and produce no Zigbee traffic at all.
///
/// `dst_addr` (the MT-layer "DstAddr") is the address the *request itself*
/// is routed to, i.e. the NWK address of the device whose binding table is
/// being modified -- that's `src_ieee`'s own NWK address, not the bind
/// target's.
pub fn zdo_bind_req(
    dst_addr: u16,
    src_ieee: [u8; 8],
    src_ep: u8,
    cluster_id: u16,
    dst_ieee: [u8; 8],
    dst_ep: u8,
) -> ZnpFrame {
    let mut data = Vec::with_capacity(2 + 8 + 1 + 2 + 1 + 8 + 1);
    data.extend_from_slice(&dst_addr.to_le_bytes());
    data.extend_from_slice(&src_ieee);
    data.push(src_ep);
    data.extend_from_slice(&cluster_id.to_le_bytes());
    data.push(0x03); // DstAddrMode: 64-bit IEEE addressing
    data.extend_from_slice(&dst_ieee);
    data.push(dst_ep);
    ZnpFrame::sreq(Subsystem::Zdo, zdo::BIND_REQ, data)
}

/// ZDO_IEEE_ADDR_RSP (AREQ cmd1=0x81), and ZDO_NWK_ADDR_RSP (0x80) which has
/// the same layout: Status(u8) IEEE(8) NWK(u16) StartIndex(u8)
/// NumAssocDev(u8) AssocDevList.
#[derive(Debug, Clone)]
pub struct IeeeAddrRsp {
    pub ieee_addr: [u8; 8],
    pub nwk_addr: u16,
}

impl IeeeAddrRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 11 {
            return None;
        }
        let status = data[0];
        if status != 0 {
            return None;
        }
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[1..9]);
        let nwk_addr = u16::from_le_bytes([data[9], data[10]]);
        Some(Self {
            ieee_addr: ieee,
            nwk_addr,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EndDeviceAnnceInd {
    pub nwk_addr: u16,
    pub ieee_addr: [u8; 8],
}

impl EndDeviceAnnceInd {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        let nwk_addr = u16::from_le_bytes([data[2], data[3]]);
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[4..12]);
        Some(Self {
            nwk_addr,
            ieee_addr: ieee,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LeaveInd {
    pub ieee_addr: [u8; 8],
    /// The device intends to rejoin (e.g. moving to a new parent), so it
    /// shouldn't be forgotten.
    pub rejoin: bool,
}

impl LeaveInd {
    /// Layout per zigpy-znp's `ZDO.LeaveInd`: NWK(u16) IEEE(8) Request(u8)
    /// Remove(u8) Rejoin(u8). A frame truncated before Rejoin is treated as a
    /// plain leave.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 10 {
            return None;
        }
        let mut ieee = [0u8; 8];
        ieee.copy_from_slice(&data[2..10]);
        let rejoin = data.get(12).is_some_and(|&b| b != 0);
        Some(Self {
            ieee_addr: ieee,
            rejoin,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ActiveEpRsp {
    pub nwk_addr: u16,
    pub endpoints: Vec<u8>,
}

impl ActiveEpRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        // SrcAddr(u16) Status(u8) NWKAddrOfInterest(u16) Count(u8) list.
        // An error reply (device not found/timeout) carries no usable list.
        if data.len() < 6 || data[2] != 0 {
            return None;
        }
        let nwk_addr = u16::from_le_bytes([data[3], data[4]]);
        let count = data[5] as usize;
        if data.len() < 6 + count {
            return None;
        }
        let endpoints = data[6..6 + count].to_vec();
        Some(Self {
            nwk_addr,
            endpoints,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SimpleDescRsp {
    pub nwk_addr: u16,
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
        let nwk_addr = u16::from_le_bytes([data[3], data[4]]);
        let endpoint = data[6];
        let profile_id = u16::from_le_bytes([data[7], data[8]]);
        let device_id = u16::from_le_bytes([data[9], data[10]]);
        let mut pos = 12;
        let n_in = *data.get(pos)? as usize;
        pos += 1;
        let mut input_clusters = Vec::with_capacity(n_in);
        for _ in 0..n_in {
            if pos + 2 > data.len() {
                return None;
            }
            input_clusters.push(u16::from_le_bytes([data[pos], data[pos + 1]]));
            pos += 2;
        }
        let n_out = *data.get(pos)? as usize;
        pos += 1;
        let mut output_clusters = Vec::with_capacity(n_out);
        for _ in 0..n_out {
            if pos + 2 > data.len() {
                return None;
            }
            output_clusters.push(u16::from_le_bytes([data[pos], data[pos + 1]]));
            pos += 2;
        }
        Some(Self {
            nwk_addr,
            endpoint,
            profile_id,
            device_id,
            input_clusters,
            output_clusters,
        })
    }
}

/// ZDO_BIND_RSP (AREQ cmd1=0xA1)
#[derive(Debug, Clone)]
pub struct BindRsp {
    pub nwk_addr: u16,
    pub status: u8,
}

impl BindRsp {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 3 {
            return None;
        }
        let nwk_addr = u16::from_le_bytes([data[0], data[1]]);
        let status = data[2];
        Some(Self { nwk_addr, status })
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
        assert_eq!(frame.cmd1, 0x00);
        assert_eq!(frame.subsystem, Subsystem::Sys);
        assert_eq!(frame.frame_type, FrameType::AReq);
        assert_eq!(frame.data, vec![0x01]);
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
        assert_eq!(frame.cmd1, 0x09);
        assert_eq!(frame.data[0..2], 0x0083u16.to_le_bytes());
        assert_eq!(frame.data[2], 0); // offset
        assert_eq!(frame.data[3], 2); // length
        assert_eq!(frame.data[4..6], 0x1A62u16.to_le_bytes());
    }

    #[test]
    fn bdb_set_channel_primary() {
        let frame = app_cnf_bdb_set_channel(1 << 11, true);
        assert_eq!(frame.data[0], 1);
        assert_eq!(frame.data.len(), 5);
        let mask = u32::from_le_bytes([frame.data[1], frame.data[2], frame.data[3], frame.data[4]]);
        assert_eq!(mask, 1 << 11);
    }

    #[test]
    fn bdb_set_channel_secondary() {
        let frame = app_cnf_bdb_set_channel(0, false);
        assert_eq!(frame.data[0], 0);
    }

    #[test]
    fn af_register_format() {
        let frame = af_register(1, 0x0104, 0x0005, &[0x0000, 0x0006], &[0x0006]);
        assert_eq!(frame.cmd1, 0x00);
        assert_eq!(frame.data[0], 1);
        assert_eq!(u16::from_le_bytes([frame.data[1], frame.data[2]]), 0x0104);
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
        assert_eq!(u16::from_le_bytes([frame.data[4], frame.data[5]]), 0x0006);
        assert_eq!(frame.data[6], 5); // trans_id
        assert_eq!(frame.data[7], 0x10); // options: APS ack, route discovery allowed
        assert_eq!(frame.data[8], 0xFF); // radius
        assert_eq!(frame.data[9], 3); // payload length
        assert_eq!(frame.data[10..13], [0x11, 0x05, 0x01]);
    }

    #[test]
    fn parse_af_incoming_msg() {
        let mut raw = vec![0; 17];
        raw[2] = 0x06;
        raw[3] = 0x00; // cluster 0x0006
        raw[4] = 0x34;
        raw[5] = 0x12; // 0x1234
        raw[6] = 1; // src_ep
        raw[7] = 1; // dst_ep
        raw[8] = 1; // was_broadcast
        raw[9] = 120; // link_quality
        raw[16] = 3; // data len
        raw.extend_from_slice(&[0x11, 0x01, 0x01]);
        let msg = AfIncomingMsg::parse(&raw).unwrap();
        assert_eq!(msg.cluster_id, 0x0006);
        assert_eq!(msg.src_addr, 0x1234);
        assert_eq!(msg.dst_ep, 1);
        assert!(msg.was_broadcast);
        assert_eq!(msg.link_quality, 120);
        assert_eq!(msg.data, vec![0x11, 0x01, 0x01]);
    }

    #[test]
    fn parse_af_incoming_msg_truncated_data_is_rejected() {
        let mut raw = vec![0; 17];
        raw[16] = 3; // claims 3 bytes, has 1
        raw.push(0x18);
        assert!(AfIncomingMsg::parse(&raw).is_none());
    }

    #[test]
    fn zdo_nwk_addr_req_format() {
        let frame = zdo_nwk_addr_req([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(frame.frame_type, FrameType::SReq);
        assert_eq!((frame.subsystem, frame.cmd1), (Subsystem::Zdo, 0x00));
        assert_eq!(frame.data, vec![1, 2, 3, 4, 5, 6, 7, 8, 0, 0]);
    }

    #[test]
    fn zdo_ext_route_disc_format() {
        let frame = zdo_ext_route_disc(0x1234, 30);
        assert_eq!((frame.subsystem, frame.cmd1), (Subsystem::Zdo, 0x45));
        assert_eq!(frame.data, vec![0x34, 0x12, 0x00, 30]);
    }

    #[test]
    fn nwk_addr_rsp_parses_like_ieee_addr_rsp() {
        let mut data = vec![0x00];
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        data.extend_from_slice(&[0x78, 0x56, 0x00, 0x00]);
        let rsp = IeeeAddrRsp::parse(&data).unwrap();
        assert_eq!(rsp.nwk_addr, 0x5678);
        assert_eq!(rsp.ieee_addr, [1, 2, 3, 4, 5, 6, 7, 8]);
        data[0] = 0x81; // device not found
        assert!(IeeeAddrRsp::parse(&data).is_none());
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
        assert_eq!(ind.nwk_addr, 0x7856);
        assert_eq!(
            ind.ieee_addr,
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
    }

    #[test]
    fn parse_device_info_skips_status_byte() {
        let data = vec![
            0x00, // status
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // ieee
            0x00, 0x00, // nwk
            0x07, // device type
            0x09, // device state: started as coordinator
            0x00, // associated device count
        ];
        let info = DeviceInfo::parse(&data).unwrap();
        assert_eq!(info.ieee_addr, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(info.device_state, Some(DEVICE_STATE_STARTED_AS_COORDINATOR));
    }

    #[test]
    fn parse_device_info_failure_status() {
        let mut data = vec![0x01];
        data.extend_from_slice(&[0u8; 13]);
        assert!(DeviceInfo::parse(&data).is_none());
    }

    #[test]
    fn parse_leave_ind_rejoin() {
        let mut data = vec![
            0x34, 0x12, // nwk
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // ieee
            0x00, // request
            0x00, // remove children
            0x01, // rejoin
        ];
        let ind = LeaveInd::parse(&data).unwrap();
        assert_eq!(ind.ieee_addr, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(ind.rejoin);

        data[12] = 0x00;
        assert!(!LeaveInd::parse(&data).unwrap().rejoin);
    }

    #[test]
    fn parse_leave_ind_truncated_is_plain_leave() {
        let data = vec![0x34, 0x12, 1, 2, 3, 4, 5, 6, 7, 8];
        assert!(!LeaveInd::parse(&data).unwrap().rejoin);
        assert!(LeaveInd::parse(&data[..9]).is_none());
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
        assert_eq!(frame.data[0], 0x0F); // broadcast addr mode
        assert_eq!(u16::from_le_bytes([frame.data[1], frame.data[2]]), 0xFFFC);
        assert_eq!(frame.data[3], 254);
        assert_eq!(zdo_permit_join(0x1234, 60).data[0], 0x02); // unicast: NWK mode
    }

    #[test]
    fn sys_version_rsp_parse() {
        let data = vec![20, 1, 2, 7, 1];
        let rsp = SysVersionRsp::parse(&data).unwrap();
        assert_eq!(rsp.transport_rev, 20);
        assert_eq!(rsp.major_rel, 2);
        assert_eq!(rsp.minor_rel, 7);
    }

    #[test]
    fn sys_version_rsp_parse_too_short() {
        assert!(SysVersionRsp::parse(&[1, 2, 3]).is_none());
    }

    #[test]
    fn zdo_bind_req_format() {
        let src_ieee = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let dst_ieee = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18];
        let frame = zdo_bind_req(0x1234, src_ieee, 1, 0x0006, dst_ieee, 1);

        assert_eq!(frame.frame_type, FrameType::SReq);
        assert_eq!(frame.subsystem, Subsystem::Zdo);
        assert_eq!(frame.cmd1, 0x21);

        assert_eq!(u16::from_le_bytes([frame.data[0], frame.data[1]]), 0x1234);
        assert_eq!(frame.data[2..10], src_ieee);
        assert_eq!(frame.data[10], 1); // src_ep
        assert_eq!(u16::from_le_bytes([frame.data[11], frame.data[12]]), 0x0006);
        assert_eq!(frame.data[13], 0x03); // DstAddrMode: 64-bit IEEE
        assert_eq!(frame.data[14..22], dst_ieee);
        assert_eq!(frame.data[22], 1); // dst_ep
        assert_eq!(frame.data.len(), 23);
    }

    #[test]
    fn parse_bind_rsp_success() {
        let data = vec![0x34, 0x12, 0x00];
        let rsp = BindRsp::parse(&data).unwrap();
        assert_eq!(rsp.nwk_addr, 0x1234);
        assert_eq!(rsp.status, 0);
    }

    #[test]
    fn parse_bind_rsp_failure_status() {
        let data = vec![0x34, 0x12, 0xAF];
        let rsp = BindRsp::parse(&data).unwrap();
        assert_eq!(rsp.status, 0xAF);
    }

    #[test]
    fn parse_bind_rsp_too_short() {
        assert!(BindRsp::parse(&[0x00, 0x00]).is_none());
    }
}
