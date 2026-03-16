/// Async ZNP serial transport.
///
/// Provides a split channel interface:
///   - `ZnpSender`  – send SREQ/AREQ frames
///   - `ZnpReceiver` – receive incoming SRSP/AREQ frames
///
/// SREQ/SRSP pairing is handled internally: `request()` sends an SREQ and
/// waits for the matching SRSP (same cmd0 subsystem bits + cmd1).
use std::time::Duration;

use futures::SinkExt;
use tokio::sync::{mpsc, oneshot};
use tokio_serial::SerialPortBuilderExt;
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;
use tracing::{error, trace, warn};

use super::frame::{FrameType, Subsystem, ZnpCodec, ZnpFrame};
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize  = 64;

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
    TcDevInd(Vec<u8>),
    StateChangeInd,
    Other,
}

// Internal message types for the actor task
enum ActorMsg {
    Send(ZnpFrame),
    Request {
        frame:      ZnpFrame,
        reply_tx:   oneshot::Sender<Result<ZnpFrame>>,
    },
}

// ── Actor task ────────────────────────────────────────────────────────────────

struct TransportActor {
    framed:     Framed<tokio_serial::SerialStream, ZnpCodec>,
    actor_rx:   mpsc::Receiver<ActorMsg>,
    event_tx:   mpsc::Sender<ZnpEvent>,
    pending:    Option<(u8, u8, oneshot::Sender<Result<ZnpFrame>>)>, // (cmd0_sub, cmd1, tx)
}

impl TransportActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                // Outgoing messages from callers
                msg = self.actor_rx.recv() => {
                    match msg {
                        None => break,
                        Some(ActorMsg::Send(frame)) => {
                            trace!(?frame, "→ ZNP send AREQ");
                            if let Err(e) = self.framed.send(frame).await {
                                error!("ZNP serial write error: {e}");
                            }
                        }
                        Some(ActorMsg::Request { frame, reply_tx }) => {
                            let expected_cmd0 = frame.cmd0();
                            let cmd1          = frame.cmd1;
                            trace!(?frame, "→ ZNP send SREQ");
                            if let Err(e) = self.framed.send(frame).await {
                                let _ = reply_tx.send(Err(Error::Io(e)));
                                continue;
                            }
                            self.pending = Some((expected_cmd0 & 0x1F, cmd1, reply_tx));
                        }
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
                            error!("ZNP serial read error: {e}");
                        }
                        Some(Ok(f)) => {
                            self.dispatch(f);
                        }
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, frame: ZnpFrame) {
        trace!(?frame, "← ZNP recv");

        if frame.frame_type == FrameType::SRsp {
            if let Some((expected_sub, expected_cmd1, _)) = &self.pending {
                if frame.subsystem as u8 == *expected_sub && frame.cmd1 == *expected_cmd1 {
                    let (_, _, reply_tx) = self.pending.take().unwrap();
                    let _ = reply_tx.send(Ok(frame));
                    return;
                }
                // Mismatched subsystem but there's a pending request — deliver it
                // anyway. Z-Stack 1.2 may return subsystem 0 for errors.
                if frame.cmd1 == *expected_cmd1 {
                    warn!(
                        "SRSP subsystem mismatch: expected 0x{:02X} got 0x{:02X}, delivering anyway",
                        expected_sub, frame.subsystem as u8
                    );
                    let (_, _, reply_tx) = self.pending.take().unwrap();
                    let _ = reply_tx.send(Ok(frame));
                    return;
                }
            }
            warn!(
                "Unexpected SRSP sub=0x{:02X} cmd1=0x{:02X}",
                frame.subsystem as u8, frame.cmd1
            );
            return;
        }

        // AREQ – convert to typed event and fan-out
        let event = match (frame.subsystem, frame.cmd1) {
            (Subsystem::Sys, 0x80) => ZnpEvent::ResetInd,
            (Subsystem::Zdo, 0x81) => ZnpEvent::IeeeAddrRsp(frame.data),
            (Subsystem::Zdo, 0x84) => ZnpEvent::SimpleDescRsp(frame.data),
            (Subsystem::Zdo, 0x85) => ZnpEvent::ActiveEpRsp(frame.data),
            (Subsystem::Zdo, 0xC0) => ZnpEvent::StateChangeInd,
            (Subsystem::Zdo, 0xC1) => ZnpEvent::EndDeviceAnnceInd(frame.data),
            (Subsystem::Zdo, 0xC9) => ZnpEvent::LeaveInd(frame.data),
            (Subsystem::Zdo, 0xCA) => ZnpEvent::TcDevInd(frame.data),
            (Subsystem::Af, 0x81) => ZnpEvent::AfIncomingMsg(frame.data),
            _ => ZnpEvent::Other,
        };

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

        let actor = TransportActor { framed, actor_rx, event_tx, pending: None };
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

    /// Send SREQ and wait for matching SRSP.
    pub async fn request(&self, frame: ZnpFrame) -> Result<ZnpFrame> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.actor_tx
            .send(ActorMsg::Request { frame, reply_tx })
            .await
            .map_err(|_| Error::ChannelClosed)?;

        tokio::time::timeout(REQUEST_TIMEOUT, reply_rx)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::ChannelClosed)?
    }
}
