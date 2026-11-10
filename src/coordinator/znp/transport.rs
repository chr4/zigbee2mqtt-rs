/// Async ZNP serial transport.
///
/// Provides a split channel interface:
///   - `ZnpTransport` – send SREQ/AREQ frames
///   - an `mpsc::Receiver<ZnpEvent>` – incoming AREQ indications
///
/// SREQ/SRSP pairing is handled internally: `request()` sends an SREQ and
/// waits for the matching SRSP (same cmd0 subsystem bits + cmd1). Only one
/// SREQ is outstanding at a time -- the ZNP wire format carries no request
/// ID, so a second in-flight SREQ of the same shape couldn't be told apart.
///
/// AF_DATA_REQUEST's SRSP only means the frame was *queued* by the stack;
/// whether it was actually delivered arrives later as an AF_DATA_CONFIRM
/// keyed by the AF transaction ID. `request_with_confirm()` registers a
/// waiter for that confirm, which the actor completes when it arrives.
///
/// Any serial I/O failure, or `MAX_CONSECUTIVE_TIMEOUTS` SREQs in a row going
/// unanswered, is treated as fatal: the actor exits, which fails all pending
/// and future requests with `Error::ChannelClosed` and closes the event
/// channel, so the bridge shuts down and its supervisor restarts it.
use std::collections::HashMap;
use std::time::Duration;

use futures::SinkExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_serial::SerialPortBuilderExt;
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;
use tracing::{debug, error, trace, warn};

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
// Capacity of the caller -> actor request queue.
const REQUEST_QUEUE_CAPACITY: usize = 256;
// `event_tx` (AREQ events to the bridge) uses try_send and drops on overflow
// (see dispatch()) rather than blocking the serial read loop -- sized well
// above typical steady-state traffic so a burst (e.g. many devices reporting
// right after coordinator restart) doesn't silently lose messages.
const EVENT_QUEUE_CAPACITY: usize = 1024;

// AF subsystem AREQ: AF_DATA_CONFIRM = Status(u8) Endpoint(u8) TransID(u8).
const AF_DATA_CONFIRM: u8 = 0x80;
// SYS_PING SREQ, used as an ordering barrier after a timeout.
const SYS_PING: u8 = 0x01;

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
    /// ZDO_NWK_ADDR_RSP -- same layout as ZDO_IEEE_ADDR_RSP.
    NwkAddrRsp(Vec<u8>),
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
        /// AF_DATA_REQUEST only: (AF transaction ID, where to deliver the
        /// status of the matching AF_DATA_CONFIRM).
        confirm: Option<(u8, oneshot::Sender<u8>)>,
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

struct TransportActor<T> {
    framed: Framed<T, ZnpCodec>,
    actor_rx: mpsc::Receiver<ActorMsg>,
    event_tx: mpsc::Sender<ZnpEvent>,
    pending: Option<PendingRequest>,
    consecutive_timeouts: u32,
    /// AF_DATA_CONFIRM waiters by AF transaction ID.
    confirms: HashMap<u8, oneshot::Sender<u8>>,
    /// Events dropped because the bridge wasn't keeping up, since the last
    /// time that was logged.
    dropped_events: u64,
}

struct PendingRequest {
    expected_sub: u8,
    expected_cmd1: u8,
    deadline: Instant,
    /// `None` for the actor's own SYS_PING barrier after a timeout.
    reply_tx: Option<oneshot::Sender<Result<ZnpFrame>>>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> TransportActor<T> {
    /// Runs until the serial port fails or the coordinator stops answering.
    /// Returning drops `pending`, `actor_rx`, `confirms` and `event_tx`,
    /// which fails every outstanding/queued request and closes the event
    /// channel.
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
                        Some(ActorMsg::Request { frame, reply_tx, confirm }) => {
                            if reply_tx.is_closed() {
                                // The caller gave up (e.g. its task was
                                // cancelled) before this was sent; don't
                                // spend a round-trip on it.
                                continue;
                            }
                            let expected_sub = frame.cmd0() & 0x1F;
                            let expected_cmd1 = frame.cmd1;
                            if let Some((trans_id, tx)) = confirm {
                                // Registered before the write, so even a
                                // confirm racing the SRSP finds its waiter.
                                // Waiters whose caller stopped listening
                                // (timed out, or the SRSP was an error so no
                                // confirm will ever come) are pruned here.
                                self.confirms.retain(|_, w| !w.is_closed());
                                self.confirms.insert(trans_id, tx);
                            }
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
                                reply_tx: Some(reply_tx),
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
                    if let Some(tx) = pending.reply_tx {
                        let _ = tx.send(Err(Error::Timeout));
                    }
                    if self.consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                        error!("ZNP coordinator stopped responding, shutting down transport");
                        break;
                    }
                    // SRSPs carry no request id: if the one we gave up on
                    // still turns up, it would be taken as the answer to the
                    // next request of the same shape. MT answers in order,
                    // so ping and treat every SRSP before the ping's own as
                    // stale.
                    let ping = ZnpFrame::sreq(Subsystem::Sys, SYS_PING, vec![]);
                    if let Err(e) = self.write(ping).await {
                        error!("ZNP serial write failed, shutting down transport: {e}");
                        break;
                    }
                    self.pending = Some(PendingRequest {
                        expected_sub: Subsystem::Sys as u8,
                        expected_cmd1: SYS_PING,
                        deadline: Instant::now() + REQUEST_TIMEOUT,
                        reply_tx: None,
                    });
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
                            if !self.dispatch(f) {
                                break;
                            }
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

    /// Handle one incoming frame. Returns false if the transport must shut
    /// down.
    fn dispatch(&mut self, frame: ZnpFrame) -> bool {
        trace!(?frame, "← ZNP recv");

        match frame.frame_type {
            FrameType::SRsp => {
                self.dispatch_srsp(frame);
                return true;
            }
            FrameType::AReq => {}
            // A well-formed coordinator never sends these; don't let a
            // corrupted cmd0 masquerade as an indication.
            FrameType::SReq | FrameType::Poll => {
                debug!("Ignoring unexpected {:?} frame from coordinator", frame.frame_type);
                return true;
            }
        }

        // AREQ – convert to typed event and fan-out
        let event = match (frame.subsystem, frame.cmd1) {
            (Subsystem::Sys, 0x80) => ZnpEvent::ResetInd,
            (Subsystem::Af, AF_DATA_CONFIRM) => {
                self.complete_confirm(&frame.data);
                return true;
            }
            (Subsystem::Zdo, 0x80) => ZnpEvent::NwkAddrRsp(frame.data),
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

        // Nothing consumes the catch-all; don't let it crowd real events out
        // of the queue.
        if matches!(event, ZnpEvent::Other) {
            return true;
        }
        let is_reset = matches!(event, ZnpEvent::ResetInd);
        match self.event_tx.try_send(event) {
            Ok(()) => {
                if self.dropped_events > 0 {
                    warn!(
                        "ZNP event queue drained again; {} events were dropped",
                        self.dropped_events
                    );
                    self.dropped_events = 0;
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) if is_reset => {
                // A coordinator reset loses endpoint registration and the
                // running network; it must not vanish in an overflowing
                // queue and leave a deaf bridge behind. Shutting down the
                // transport is equally fatal and always noticed.
                error!("Coordinator reset while the event queue was full, shutting down transport");
                return false;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if self.dropped_events == 0 {
                    warn!("ZNP event queue full (bridge not keeping up), dropping events");
                }
                self.dropped_events += 1;
            }
            // The event pump is gone (bridge shutting down); nothing to do.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
        true
    }

    fn dispatch_srsp(&mut self, frame: ZnpFrame) {
        let matched = self
            .pending
            .as_ref()
            .map(|p| match_srsp(&frame, p.expected_sub, p.expected_cmd1));
        match matched {
            Some(SrspMatch::Reply) => {
                self.consecutive_timeouts = 0;
                let pending = self.pending.take().expect("matched implies pending");
                if let Some(tx) = pending.reply_tx {
                    let _ = tx.send(Ok(frame));
                }
            }
            Some(SrspMatch::Rejected(code)) => {
                // The coordinator answered, so it's alive.
                self.consecutive_timeouts = 0;
                let pending = self.pending.take().expect("matched implies pending");
                if let Some(tx) = pending.reply_tx {
                    let _ = tx.send(Err(Error::ZnpRejected {
                        subsystem: pending.expected_sub,
                        cmd1: pending.expected_cmd1,
                        reason: rpc_error_name(code),
                    }));
                }
            }
            Some(SrspMatch::Unrelated) | None => {
                // Typically the late answer to a request that already timed
                // out: the coordinator is evidently still alive.
                warn!(
                    "Unexpected SRSP sub=0x{:02X} cmd1=0x{:02X} data={:02X?}",
                    frame.subsystem as u8, frame.cmd1, frame.data
                );
            }
        }
    }

    /// Route an AF_DATA_CONFIRM (Status, Endpoint, TransID) to its waiter.
    fn complete_confirm(&mut self, data: &[u8]) {
        let [status, _endpoint, trans_id, ..] = *data else {
            debug!("Truncated AF_DATA_CONFIRM: {data:02X?}");
            return;
        };
        match self.confirms.remove(&trans_id) {
            Some(tx) => {
                let _ = tx.send(status);
            }
            None => trace!("AF_DATA_CONFIRM for untracked trans_id {trans_id} status 0x{status:02X}"),
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
    pub fn open(port: &str, baud: u32, rtscts: bool) -> Result<(Self, mpsc::Receiver<ZnpEvent>)> {
        let flow_control = if rtscts {
            tokio_serial::FlowControl::Hardware
        } else {
            tokio_serial::FlowControl::None
        };
        let serial = tokio_serial::new(port, baud)
            .flow_control(flow_control)
            .timeout(Duration::from_millis(100))
            .open_native_async()?;
        Ok(Self::spawn(serial))
    }

    /// Spawn the actor on any byte stream (the serial port, or an in-memory
    /// pipe in tests).
    pub fn spawn<T>(io: T) -> (Self, mpsc::Receiver<ZnpEvent>)
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let framed = Framed::new(io, ZnpCodec);
        let (actor_tx, actor_rx) = mpsc::channel::<ActorMsg>(REQUEST_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<ZnpEvent>(EVENT_QUEUE_CAPACITY);

        let actor = TransportActor {
            framed,
            actor_rx,
            event_tx,
            pending: None,
            consecutive_timeouts: 0,
            confirms: HashMap::new(),
            dropped_events: 0,
        };
        tokio::spawn(actor.run());

        (Self { actor_tx }, event_rx)
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
        self.request_inner(frame, None).await
    }

    /// Like `request()`, for an AF_DATA_REQUEST carrying AF transaction ID
    /// `trans_id`: the status of the matching AF_DATA_CONFIRM is delivered to
    /// `confirm_tx`. If the SRSP reports an error no confirm follows; drop
    /// the receiver then.
    pub async fn request_with_confirm(
        &self,
        frame: ZnpFrame,
        trans_id: u8,
        confirm_tx: oneshot::Sender<u8>,
    ) -> Result<ZnpFrame> {
        self.request_inner(frame, Some((trans_id, confirm_tx))).await
    }

    async fn request_inner(
        &self,
        frame: ZnpFrame,
        confirm: Option<(u8, oneshot::Sender<u8>)>,
    ) -> Result<ZnpFrame> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.actor_tx
            .send(ActorMsg::Request {
                frame,
                reply_tx,
                confirm,
            })
            .await
            .map_err(|_| Error::ChannelClosed)?;
        reply_rx.await.map_err(|_| Error::ChannelClosed)?
    }
}

/// An in-memory stand-in for the serial link, with the test playing the
/// coordinator.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio_util::codec::{Decoder, Encoder};

    /// The coordinator side of an in-memory serial link.
    pub(crate) struct FakeCoordinator {
        io: DuplexStream,
        rx: BytesMut,
    }

    impl FakeCoordinator {
        pub(crate) async fn recv(&mut self) -> ZnpFrame {
            loop {
                if let Some(f) = ZnpCodec.decode(&mut self.rx).unwrap() {
                    return f;
                }
                let mut buf = [0u8; 256];
                let n = self.io.read(&mut buf).await.unwrap();
                assert!(n > 0, "transport closed the link");
                self.rx.extend_from_slice(&buf[..n]);
            }
        }

        pub(crate) async fn send(&mut self, frame: ZnpFrame) {
            let mut out = BytesMut::new();
            ZnpCodec.encode(frame, &mut out).unwrap();
            self.io.write_all(&out).await.unwrap();
        }

        /// Answer an SREQ with an SRSP of the same shape carrying `data`.
        pub(crate) async fn reply(&mut self, req: &ZnpFrame, data: Vec<u8>) {
            self.send(ZnpFrame::new(FrameType::SRsp, req.subsystem, req.cmd1, data))
                .await;
        }

        /// Send AF_DATA_CONFIRM for AF transaction `trans_id`.
        pub(crate) async fn confirm(&mut self, trans_id: u8, status: u8) {
            self.send(ZnpFrame::areq(
                Subsystem::Af,
                AF_DATA_CONFIRM,
                vec![status, 1, trans_id],
            ))
            .await;
        }
    }

    pub(crate) fn link() -> (ZnpTransport, mpsc::Receiver<ZnpEvent>, FakeCoordinator) {
        let (ours, theirs) = tokio::io::duplex(4096);
        let (transport, events) = ZnpTransport::spawn(ours);
        (
            transport,
            events,
            FakeCoordinator {
                io: theirs,
                rx: BytesMut::new(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fake::link;
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

    fn data_request() -> ZnpFrame {
        ZnpFrame::sreq(Subsystem::Af, 0x01, vec![0; 10])
    }

    #[tokio::test]
    async fn af_data_confirm_reaches_its_waiter() {
        let (transport, _events, mut coord) = link();
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let req = tokio::spawn({
            let t = transport.clone();
            async move { t.request_with_confirm(data_request(), 42, confirm_tx).await }
        });

        let sent = coord.recv().await;
        assert_eq!((sent.subsystem, sent.cmd1), (Subsystem::Af, 0x01));
        coord.send(srsp(Subsystem::Af, 0x01, vec![0x00])).await;
        assert_eq!(req.await.unwrap().unwrap().data, vec![0x00]);

        // A confirm for another transaction is not ours...
        coord.confirm(41, 0x00).await;
        // ...this one is: MAC no-ack.
        coord.confirm(42, 0xE9).await;
        assert_eq!(confirm_rx.await.unwrap(), 0xE9);
    }

    #[tokio::test]
    async fn af_data_confirm_is_not_forwarded_as_event() {
        let (transport, mut events, mut coord) = link();
        coord.confirm(7, 0x00).await;
        coord
            .send(ZnpFrame::areq(Subsystem::Zdo, 0x80, vec![0; 13]))
            .await;
        let ev = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, ZnpEvent::NwkAddrRsp(_)), "{ev:?}");
        drop(transport);
    }

    #[tokio::test]
    async fn requests_are_serialized_and_answered_in_order() {
        let (transport, _events, mut coord) = link();
        let a = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![])).await }
        });
        let first = coord.recv().await;
        let b = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::Zdo, 0x36, vec![])).await }
        });
        // B must not be written while A is outstanding.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), coord.recv())
                .await
                .is_err()
        );
        coord.reply(&first, vec![1]).await;
        assert_eq!(a.await.unwrap().unwrap().data, vec![1]);
        let second = coord.recv().await;
        assert_eq!((second.subsystem, second.cmd1), (Subsystem::Zdo, 0x36));
        coord.reply(&second, vec![0]).await;
        assert_eq!(b.await.unwrap().unwrap().data, vec![0]);
    }

    #[tokio::test]
    async fn rpc_error_srsp_rejects_request() {
        let (transport, _events, mut coord) = link();
        let req = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::AppCnf, 0x08, vec![0; 5])).await }
        });
        coord.recv().await;
        coord.send(srsp(Subsystem::Rpc, 0x00, vec![0x01, 0x2F, 0x08])).await;
        assert!(matches!(
            req.await.unwrap(),
            Err(Error::ZnpRejected { cmd1: 0x08, .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_timeouts_shut_the_transport_down() {
        let (transport, mut events, mut coord) = link();
        let req = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![])).await }
        });
        coord.recv().await;
        assert!(matches!(req.await.unwrap(), Err(Error::Timeout)));
        // Each timeout is followed by a barrier ping; those time out too.
        for _ in 1..MAX_CONSECUTIVE_TIMEOUTS {
            let ping = coord.recv().await;
            assert_eq!((ping.subsystem, ping.cmd1), (Subsystem::Sys, SYS_PING));
        }
        // The actor has exited: the event channel closes and requests fail.
        assert!(events.recv().await.is_none());
        assert!(matches!(
            transport.request(ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![])).await,
            Err(Error::ChannelClosed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn late_srsp_after_timeout_is_not_taken_for_the_next_answer() {
        let (transport, _events, mut coord) = link();
        let first = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::Af, 0x01, vec![1])).await }
        });
        let req1 = coord.recv().await;
        assert!(matches!(first.await.unwrap(), Err(Error::Timeout)));
        // The barrier ping goes out; a second request queues behind it.
        let ping = coord.recv().await;
        assert_eq!((ping.subsystem, ping.cmd1), (Subsystem::Sys, SYS_PING));
        let second = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::Af, 0x01, vec![2])).await }
        });
        // The late answer to the first request, then the ping's.
        coord.reply(&req1, vec![0xAA]).await;
        coord.reply(&ping, vec![0, 0]).await;
        let req2 = coord.recv().await;
        assert_eq!(req2.data, vec![2]);
        coord.reply(&req2, vec![0x00]).await;
        assert_eq!(second.await.unwrap().unwrap().data, vec![0x00]);
    }

    #[tokio::test(start_paused = true)]
    async fn answered_ping_resets_timeout_count() {
        let (transport, _events, mut coord) = link();
        for _ in 0..MAX_CONSECUTIVE_TIMEOUTS * 2 {
            let req = tokio::spawn({
                let t = transport.clone();
                async move { t.request(ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![])).await }
            });
            coord.recv().await;
            assert!(matches!(req.await.unwrap(), Err(Error::Timeout)));
            let ping = coord.recv().await;
            coord.reply(&ping, vec![0, 0]).await;
        }
        // Still alive.
        let req = tokio::spawn({
            let t = transport.clone();
            async move { t.request(ZnpFrame::sreq(Subsystem::Sys, 0x02, vec![])).await }
        });
        let sent = coord.recv().await;
        coord.reply(&sent, vec![0]).await;
        assert!(req.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn serial_close_closes_event_channel() {
        let (transport, mut events, coord) = link();
        drop(coord);
        assert!(events.recv().await.is_none());
        drop(transport);
    }
}
