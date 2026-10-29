/// Async ZNP serial transport.
///
/// Provides a split channel interface:
///   - `ZnpSender`  – send SREQ/AREQ frames
///   - `ZnpReceiver` – receive incoming SRSP/AREQ frames
///
/// SREQ/SRSP pairing is handled internally: `request()` sends an SREQ and
/// waits for the matching SRSP (same cmd0 subsystem bits + cmd1). Only one
/// SREQ is outstanding at a time -- the ZNP wire format carries no request
/// ID, so a second in-flight SREQ of the same shape couldn't be told apart.
///
/// Any serial I/O failure, or `MAX_CONSECUTIVE_TIMEOUTS` SREQs in a row going
/// unanswered, is treated as fatal: the actor exits, which fails all pending
/// and future requests with `Error::ChannelClosed` and closes the event
/// channel, so the bridge shuts down and its supervisor restarts it.
use std::time::Duration;

use futures::SinkExt;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_serial::SerialPortBuilderExt;
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;
use tracing::{error, trace, warn};

use super::frame::{FrameType, Subsystem, ZnpCodec, ZnpFrame};
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// A serial write that doesn't complete in this time means the adapter has
// stopped draining its USB/UART buffer -- treated as fatal like a write error.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
// SRSPs come from the coordinator itself (not the remote device), so even a
// ZDO request to a sleeping device is answered promptly. Several timeouts in
// a row mean the adapter firmware has hung (a known CC2531 failure mode)
// rather than a one-off glitch.
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;
// `event_tx` (AREQ events to the bridge) uses try_send and drops on overflow
// (see dispatch()) rather than blocking the serial read loop -- sized well
// above typical steady-state traffic so a burst (e.g. many devices reporting
// right after coordinator restart) doesn't silently lose messages.
const CHANNEL_CAPACITY: usize = 256;

// ── Public events that consumers receive ─────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ZnpEvent {
    ResetInd,
    EndDeviceAnnceInd(Vec<u8>),
    LeaveInd(Vec<u8>),
    AfIncomingMsg(Vec<u8>),
    ActiveEpRsp(Vec<u8>),
    SimpleDescRsp(Vec<u8>),
    IeeeAddrRsp(Vec<u8>),
    BindRsp(Vec<u8>),
    TcDevInd(Vec<u8>),
    /// ZDO_STATE_CHANGE_IND with the new device state (0x09 = started as
    /// coordinator).
    StateChangeInd(u8),
    Other,
}

// Internal message types for the actor task
enum ActorMsg {
    Send(ZnpFrame),
    Request {
        frame: ZnpFrame,
        reply_tx: oneshot::Sender<Result<ZnpFrame>>,
    },
}

// ── SRSP matching ─────────────────────────────────────────────────────────────

/// How an incoming SRSP relates to the outstanding request.
#[derive(Debug, PartialEq, Eq)]
enum SrspMatch {
    /// The SRSP answering the pending request.
    Reply,
    /// An RPC error (SRSP, subsystem 0x00, cmd1 0x00) whose embedded request
    /// header names the pending request -- the firmware rejected it, e.g.
    /// APP_CNF/BDB commands on Z-Stack Home 1.2. Carries the error code.
    Rejected(u8),
    /// Not for the pending request (stale, or unsolicited).
    Unrelated,
}

/// Classify `frame` (an SRSP) against a pending request of shape
/// (`expected_sub`, `expected_cmd1`). RPC error layout per zigpy-znp's
/// `RPCError.CommandNotRecognized`: ErrorCode(u8) + RequestHeader(cmd0, cmd1).
fn match_srsp(frame: &ZnpFrame, expected_sub: u8, expected_cmd1: u8) -> SrspMatch {
    if frame.subsystem as u8 == expected_sub && frame.cmd1 == expected_cmd1 {
        return SrspMatch::Reply;
    }
    if frame.subsystem == Subsystem::Rpc && frame.cmd1 == 0x00 && frame.data.len() >= 3 {
        let (code, req_cmd0, req_cmd1) = (frame.data[0], frame.data[1], frame.data[2]);
        if req_cmd0 & 0x1F == expected_sub && req_cmd1 == expected_cmd1 {
            return SrspMatch::Rejected(code);
        }
    }
    SrspMatch::Unrelated
}

fn rpc_error_name(code: u8) -> &'static str {
    match code {
        0x01 => "invalid subsystem",
        0x02 => "invalid command id",
        0x03 => "invalid parameter",
        0x04 => "invalid length",
        _ => "unknown error",
    }
}

// ── Actor task ────────────────────────────────────────────────────────────────

struct TransportActor {
    framed: Framed<tokio_serial::SerialStream, ZnpCodec>,
    actor_rx: mpsc::Receiver<ActorMsg>,
    event_tx: mpsc::Sender<ZnpEvent>,
    pending: Option<PendingRequest>,
    consecutive_timeouts: u32,
}

struct PendingRequest {
    expected_sub: u8,
    expected_cmd1: u8,
    deadline: Instant,
    reply_tx: oneshot::Sender<Result<ZnpFrame>>,
}

impl TransportActor {
    /// Runs until the serial port fails or the coordinator stops answering.
    /// Returning drops `pending`, `actor_rx` and `event_tx`, which fails every
    /// outstanding/queued request and closes the event channel.
    async fn run(mut self) {
        loop {
            let deadline = self.pending.as_ref().map(|p| p.deadline);
            tokio::select! {
                // Outgoing messages from callers. Not read while an SREQ is
                // outstanding, so requests queue up here in order.
                msg = self.actor_rx.recv(), if self.pending.is_none() => {
                    match msg {
                        None => break,
                        Some(ActorMsg::Send(frame)) => {
                            trace!(?frame, "→ ZNP send AREQ");
                            if let Err(e) = self.write(frame).await {
                                error!("ZNP serial write failed, shutting down transport: {e}");
                                break;
                            }
                        }
                        Some(ActorMsg::Request { frame, reply_tx }) => {
                            let expected_sub = frame.cmd0() & 0x1F;
                            let expected_cmd1 = frame.cmd1;
                            trace!(?frame, "→ ZNP send SREQ");
                            if let Err(e) = self.write(frame).await {
                                error!("ZNP serial write failed, shutting down transport: {e}");
                                let _ = reply_tx.send(Err(e));
                                break;
                            }
                            self.pending = Some(PendingRequest {
                                expected_sub,
                                expected_cmd1,
                                deadline: Instant::now() + REQUEST_TIMEOUT,
                                reply_tx,
                            });
                        }
                    }
                }

                // SRSP for the outstanding SREQ didn't arrive in time
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    let pending = self.pending.take().expect("deadline implies pending");
                    self.consecutive_timeouts += 1;
                    warn!(
                        "ZNP request sub=0x{:02X} cmd1=0x{:02X} timed out ({}/{MAX_CONSECUTIVE_TIMEOUTS} in a row)",
                        pending.expected_sub, pending.expected_cmd1, self.consecutive_timeouts
                    );
                    let _ = pending.reply_tx.send(Err(Error::Timeout));
                    if self.consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                        error!("ZNP coordinator stopped responding, shutting down transport");
                        break;
                    }
                }

                // Incoming frames from serial port
                frame = self.framed.next() => {
                    match frame {
                        None => {
                            error!("ZNP serial port closed");
                            break;
                        }
                        Some(Err(e)) => {
                            // tokio-util's Framed ends the stream after any
                            // read error anyway; stop explicitly.
                            error!("ZNP serial read error, shutting down transport: {e}");
                            break;
                        }
                        Some(Ok(f)) => {
                            self.dispatch(f);
                        }
                    }
                }
            }
        }
    }

    async fn write(&mut self, frame: ZnpFrame) -> Result<()> {
        match tokio::time::timeout(WRITE_TIMEOUT, self.framed.send(frame)).await {
            Ok(res) => res.map_err(Error::Io),
            Err(_) => Err(Error::Timeout),
        }
    }

    fn dispatch(&mut self, frame: ZnpFrame) {
        trace!(?frame, "← ZNP recv");

        if frame.frame_type == FrameType::SRsp {
            let matched = self
                .pending
                .as_ref()
                .map(|p| match_srsp(&frame, p.expected_sub, p.expected_cmd1));
            match matched {
                Some(SrspMatch::Reply) => {
                    self.consecutive_timeouts = 0;
                    let pending = self.pending.take().unwrap();
                    let _ = pending.reply_tx.send(Ok(frame));
                }
                Some(SrspMatch::Rejected(code)) => {
                    // The coordinator answered, so it's alive.
                    self.consecutive_timeouts = 0;
                    let pending = self.pending.take().unwrap();
                    let _ = pending.reply_tx.send(Err(Error::ZnpRejected {
                        subsystem: pending.expected_sub,
                        cmd1: pending.expected_cmd1,
                        reason: rpc_error_name(code),
                    }));
                }
                Some(SrspMatch::Unrelated) | None => {
                    warn!(
                        "Unexpected SRSP sub=0x{:02X} cmd1=0x{:02X} data={:02X?}",
                        frame.subsystem as u8, frame.cmd1, frame.data
                    );
                }
            }
            return;
        }

        // AREQ – convert to typed event and fan-out
        let event = match (frame.subsystem, frame.cmd1) {
            (Subsystem::Sys, 0x80) => ZnpEvent::ResetInd,
            (Subsystem::Zdo, 0x81) => ZnpEvent::IeeeAddrRsp(frame.data),
            (Subsystem::Zdo, 0x84) => ZnpEvent::SimpleDescRsp(frame.data),
            (Subsystem::Zdo, 0x85) => ZnpEvent::ActiveEpRsp(frame.data),
            (Subsystem::Zdo, 0xA1) => ZnpEvent::BindRsp(frame.data),
            (Subsystem::Zdo, 0xC0) => {
                ZnpEvent::StateChangeInd(frame.data.first().copied().unwrap_or(0))
            }
            (Subsystem::Zdo, 0xC1) => ZnpEvent::EndDeviceAnnceInd(frame.data),
            (Subsystem::Zdo, 0xC9) => ZnpEvent::LeaveInd(frame.data),
            (Subsystem::Zdo, 0xCA) => ZnpEvent::TcDevInd(frame.data),
            (Subsystem::Af, 0x81) => ZnpEvent::AfIncomingMsg(frame.data),
            _ => ZnpEvent::Other,
        };

        // Nothing consumes the catch-all; don't let it (e.g. one
        // AF_DATA_CONFIRM per command sent) crowd real events out of the queue.
        if matches!(event, ZnpEvent::Other) {
            return;
        }
        if let Err(e) = self.event_tx.try_send(event) {
            warn!("ZNP event queue full, dropping: {e}");
        }
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ZnpTransport {
    actor_tx: mpsc::Sender<ActorMsg>,
}

impl ZnpTransport {
    /// Open the serial port and spawn the actor task.
    pub fn open(port: &str, baud: u32) -> Result<(Self, mpsc::Receiver<ZnpEvent>)> {
        let serial = tokio_serial::new(port, baud)
            .timeout(Duration::from_millis(100))
            .open_native_async()?;

        let framed = Framed::new(serial, ZnpCodec);
        let (actor_tx, actor_rx) = mpsc::channel::<ActorMsg>(CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<ZnpEvent>(CHANNEL_CAPACITY);

        let actor = TransportActor {
            framed,
            actor_rx,
            event_tx,
            pending: None,
            consecutive_timeouts: 0,
        };
        tokio::spawn(actor.run());

        Ok((Self { actor_tx }, event_rx))
    }

    /// Fire-and-forget AREQ send.
    pub async fn send(&self, frame: ZnpFrame) -> Result<()> {
        self.actor_tx
            .send(ActorMsg::Send(frame))
            .await
            .map_err(|_| Error::ChannelClosed)
    }

    /// Send SREQ and wait for matching SRSP. The actor enforces
    /// `REQUEST_TIMEOUT` (answering `Error::Timeout`); an RPC error from the
    /// firmware comes back as `Error::ZnpRejected`, and the actor shutting
    /// down surfaces as `Error::ChannelClosed`.
    pub async fn request(&self, frame: ZnpFrame) -> Result<ZnpFrame> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.actor_tx
            .send(ActorMsg::Request { frame, reply_tx })
            .await
            .map_err(|_| Error::ChannelClosed)?;
        reply_rx.await.map_err(|_| Error::ChannelClosed)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srsp(subsystem: Subsystem, cmd1: u8, data: Vec<u8>) -> ZnpFrame {
        ZnpFrame::new(FrameType::SRsp, subsystem, cmd1, data)
    }

    #[test]
    fn matching_srsp_is_reply() {
        let f = srsp(Subsystem::Af, 0x01, vec![0x00]);
        assert_eq!(match_srsp(&f, Subsystem::Af as u8, 0x01), SrspMatch::Reply);
    }

    #[test]
    fn rpc_error_for_pending_request_is_rejected() {
        // APP_CNF BDB_SET_CHANNEL (SREQ cmd0=0x2F, cmd1=0x08) rejected as an
        // invalid subsystem, as Z-Stack Home 1.2 does.
        let f = srsp(Subsystem::Rpc, 0x00, vec![0x01, 0x2F, 0x08]);
        assert_eq!(
            match_srsp(&f, Subsystem::AppCnf as u8, 0x08),
            SrspMatch::Rejected(0x01)
        );
    }

    #[test]
    fn rpc_error_for_other_request_is_unrelated() {
        let f = srsp(Subsystem::Rpc, 0x00, vec![0x02, 0x25, 0x01]);
        assert_eq!(
            match_srsp(&f, Subsystem::Af as u8, 0x01),
            SrspMatch::Unrelated
        );
    }

    #[test]
    fn same_cmd1_other_subsystem_is_unrelated() {
        // ZDO_IEEE_ADDR_REQ's SRSP (cmd1 0x01) must not answer AF_DATA_REQUEST.
        let f = srsp(Subsystem::Zdo, 0x01, vec![0x00]);
        assert_eq!(
            match_srsp(&f, Subsystem::Af as u8, 0x01),
            SrspMatch::Unrelated
        );
    }

    #[test]
    fn truncated_rpc_error_is_unrelated() {
        let f = srsp(Subsystem::Rpc, 0x00, vec![0x01]);
        assert_eq!(
            match_srsp(&f, Subsystem::AppCnf as u8, 0x08),
            SrspMatch::Unrelated
        );
    }
}
