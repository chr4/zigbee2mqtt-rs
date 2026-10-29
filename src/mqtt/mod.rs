/// MQTT bridge – publishes device state and subscribes to set/get commands.
/// Message formats are compatible with zigbee2mqtt for Home Assistant integration.
pub mod attribute_output;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, ClientError, Event, EventLoop, MqttOptions, Packet, QoS};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use crate::config::MqttConfig;
use crate::error::{Error, Result};

const MQTT_RECONNECT_DELAY_MIN: Duration = Duration::from_secs(1);
const MQTT_RECONNECT_DELAY_MAX: Duration = Duration::from_secs(30);
// Capacity of rumqttc's request channel (AsyncClient → EventLoop). Only the
// event loop drains it, and only while connected -- see `MqttBridge::publish`.
const MQTT_REQUEST_CAPACITY: usize = 64;
// Upper bound on waiting for room in the request channel while connected,
// covering a disconnect that races with the `is_connected()` check.
const MQTT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

// ── Inbound MQTT commands ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum MqttCommand {
    Connected,
    PermitJoin {
        duration: u8,
    },
    SetDevice {
        friendly_name: String,
        payload: serde_json::Value,
    },
    GetDevice {
        friendly_name: String,
    },
}

// ── Link state ────────────────────────────────────────────────────────────────

/// Broker connection state, shared between the bridge (publishing) and the
/// event loop task (which sees connects/disconnects).
#[derive(Default)]
struct LinkState {
    connected: AtomicBool,
    /// Publishes dropped since the link last went down.
    dropped: AtomicU64,
}

impl LinkState {
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn set_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    /// Mark the link up; returns how many publishes were dropped while it was down.
    fn set_connected(&self) -> u64 {
        self.connected.store(true, Ordering::Relaxed);
        self.dropped.swap(0, Ordering::Relaxed)
    }

    /// Count a dropped publish; true for the first drop of an outage, so the
    /// caller logs once per outage rather than once per message.
    fn note_dropped(&self) -> bool {
        self.dropped.fetch_add(1, Ordering::Relaxed) == 0
    }
}

// ── MqttBridge ────────────────────────────────────────────────────────────────

pub struct MqttBridge {
    client: AsyncClient,
    base_topic: String,
    link: Arc<LinkState>,
}

impl MqttBridge {
    /// Connect to the broker, subscribe to command topics, and spawn the event loop.
    pub async fn connect(cfg: &MqttConfig) -> Result<(Self, mpsc::UnboundedReceiver<MqttCommand>)> {
        let mut opts = MqttOptions::new(&cfg.client_id, &cfg.server, cfg.port);
        opts.set_keep_alive(Duration::from_secs(cfg.keepalive as u64));
        opts.set_clean_session(true);

        if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
            opts.set_credentials(user, pass);
        }

        // Last-will: bridge goes offline (z2m JSON format)
        let will_topic = format!("{}/bridge/state", cfg.base_topic);
        let will_payload = serde_json::to_vec(&json!({"state": "offline"})).unwrap();
        opts.set_last_will(rumqttc::LastWill::new(
            &will_topic,
            will_payload,
            QoS::AtLeastOnce,
            true,
        ));

        let (client, event_loop) = AsyncClient::new(opts, MQTT_REQUEST_CAPACITY);
        // Unbounded: the event loop task must never block on the bridge (see
        // run_event_loop). Inbound volume is bounded by what the broker sends.
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<MqttCommand>();
        let (connected_tx, connected_rx) = oneshot::channel();
        let link = Arc::new(LinkState::default());

        tokio::spawn(run_event_loop(
            event_loop,
            client.clone(),
            cfg.base_topic.clone(),
            cmd_tx,
            connected_tx,
            Arc::clone(&link),
        ));

        connected_rx.await.map_err(|_| {
            Error::Config("MQTT event loop exited before initial connection".to_string())
        })?;

        Ok((
            Self {
                client,
                base_topic: cfg.base_topic.clone(),
                link,
            },
            cmd_rx,
        ))
    }

    // ── Publish helpers ───────────────────────────────────────────────────────

    /// Publish bridge/state as JSON (z2m format).
    pub async fn publish_bridge_state(&self, online: bool) -> Result<()> {
        let topic = format!("{}/bridge/state", self.base_topic);
        let state = if online { "online" } else { "offline" };
        let payload = serde_json::to_vec(&json!({"state": state}))?;
        self.publish(&topic, payload, true).await
    }

    /// Publish bridge/info (z2m format).
    pub async fn publish_bridge_info(&self, info: &serde_json::Value) -> Result<()> {
        let topic = format!("{}/bridge/info", self.base_topic);
        let payload = serde_json::to_vec(info)?;
        self.publish(&topic, payload, true).await
    }

    pub async fn publish_device_state(
        &self,
        friendly_name: &str,
        state: &serde_json::Value,
    ) -> Result<()> {
        let topic = format!("{}/{}", self.base_topic, friendly_name);
        let payload = serde_json::to_vec(state)?;
        self.publish(&topic, payload, true).await
    }

    /// Publish one raw attribute value for `advanced.output: attribute`/
    /// `attribute_and_json` mode -- z2m: topic
    /// `<base_topic>/<friendly_name>/<key_path>`, unquoted raw string
    /// payload, retained (same as the merged-JSON publish). See
    /// `attribute_output::flatten_attribute_output` for how `key_path`/
    /// `value` are derived from a device's state.
    pub async fn publish_device_attribute(
        &self,
        friendly_name: &str,
        key_path: &str,
        value: &str,
    ) -> Result<()> {
        let topic = format!("{}/{}/{}", self.base_topic, friendly_name, key_path);
        self.publish(&topic, value.as_bytes().to_vec(), true).await
    }

    pub async fn publish_bridge_devices(&self, devices: &serde_json::Value) -> Result<()> {
        let topic = format!("{}/bridge/devices", self.base_topic);
        let payload = serde_json::to_vec(devices)?;
        self.publish(&topic, payload, true).await
    }

    /// Publish to bridge/logging (z2m format).
    pub async fn publish_bridge_log(&self, level: &str, message: &str) -> Result<()> {
        let topic = format!("{}/bridge/logging", self.base_topic);
        let payload = json!({
            "level": level,
            "message": message,
        });
        let bytes = serde_json::to_vec(&payload)?;
        self.publish(&topic, bytes, false).await
    }

    /// Publish a Home Assistant MQTT discovery config.
    pub async fn publish_ha_discovery(
        &self,
        component: &str,
        node_id: &str,
        object_id: &str,
        config: &serde_json::Value,
    ) -> Result<()> {
        let topic = format!("homeassistant/{component}/{node_id}/{object_id}/config");
        tracing::debug!("HA discovery → {topic}");
        let payload = serde_json::to_vec(config)?;
        self.publish(&topic, payload, true).await.map_err(|e| {
            error!("Failed to publish HA discovery to {topic}: {e}");
            e
        })
    }

    /// All publishes go through here. While connected, wait for room in
    /// rumqttc's request channel (the event loop drains it). While
    /// disconnected nothing drains it, so waiting would stall the whole bridge
    /// -- Zigbee processing included -- for the length of the broker outage.
    /// Instead queue what fits and drop the rest: everything published here is
    /// either retained state the bridge republishes in full on reconnect
    /// (`MqttCommand::Connected`) or a transient log line.
    async fn publish(&self, topic: &str, payload: Vec<u8>, retain: bool) -> Result<()> {
        if self.link.is_connected() {
            let send = self
                .client
                .publish(topic, QoS::AtLeastOnce, retain, payload);
            return match tokio::time::timeout(MQTT_PUBLISH_TIMEOUT, send).await {
                Ok(result) => result.map_err(Error::Mqtt),
                Err(_) => {
                    self.note_dropped(topic);
                    Ok(())
                }
            };
        }
        match self
            .client
            .try_publish(topic, QoS::AtLeastOnce, retain, payload)
        {
            Ok(()) => Ok(()),
            Err(ClientError::TryRequest(_)) => {
                self.note_dropped(topic);
                Ok(())
            }
            Err(e) => Err(Error::Mqtt(e)),
        }
    }

    fn note_dropped(&self, topic: &str) {
        if self.link.note_dropped() {
            warn!("MQTT send queue full (broker unreachable?), dropping publishes until reconnect, first: {topic}");
        }
    }
}

// ── Event loop task ───────────────────────────────────────────────────────────

/// Drive the rumqttc event loop forever, reconnecting with backoff.
///
/// Nothing awaited in here may depend on the bridge task or on rumqttc's
/// request channel: `poll()` is the only thing that drains that channel, so
/// e.g. awaiting `client.subscribe()` here after a reconnect, with the channel
/// already filled by the bridge's publishes, deadlocks both tasks for good.
async fn run_event_loop(
    mut event_loop: EventLoop,
    client: AsyncClient,
    base_topic: String,
    cmd_tx: mpsc::UnboundedSender<MqttCommand>,
    connected_tx: oneshot::Sender<()>,
    link: Arc<LinkState>,
) {
    let mut connected_tx = Some(connected_tx);
    let mut reconnect_delay = MQTT_RECONNECT_DELAY_MIN;

    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                reconnect_delay = MQTT_RECONNECT_DELAY_MIN;
                let dropped = link.set_connected();
                if dropped > 0 {
                    warn!("Dropped {dropped} MQTT publishes while disconnected");
                }
                info!("MQTT connected");
                tokio::spawn(subscribe_and_notify(
                    client.clone(),
                    base_topic.clone(),
                    cmd_tx.clone(),
                    connected_tx.take(),
                ));
            }
            Ok(Event::Incoming(Packet::Publish(pub_msg))) => {
                if let Some(cmd) =
                    command_from_publish(&pub_msg.topic, &pub_msg.payload, &base_topic)
                {
                    let _ = cmd_tx.send(cmd);
                }
            }
            Ok(Event::Incoming(Packet::Disconnect)) => {
                link.set_disconnected();
                warn!(
                    "MQTT disconnected, retrying in {}s",
                    reconnect_delay.as_secs()
                );
                tokio::time::sleep(reconnect_delay).await;
                reconnect_delay = next_backoff(reconnect_delay);
            }
            Ok(_) => {}
            Err(e) => {
                link.set_disconnected();
                error!(
                    "MQTT event loop error: {e}; retrying in {}s",
                    reconnect_delay.as_secs()
                );
                tokio::time::sleep(reconnect_delay).await;
                reconnect_delay = next_backoff(reconnect_delay);
            }
        }
    }
}

/// Subscribe to the command topics, then tell the bridge the session is up:
/// via `connected_tx` on the first connect, `MqttCommand::Connected` after a
/// reconnect. Runs in its own task so `subscribe()` can wait for room in the
/// request channel while the event loop keeps draining it.
async fn subscribe_and_notify(
    client: AsyncClient,
    base_topic: String,
    cmd_tx: mpsc::UnboundedSender<MqttCommand>,
    connected_tx: Option<oneshot::Sender<()>>,
) {
    if !subscribe_topics(&client, &base_topic).await {
        return;
    }
    info!("MQTT subscribed");
    match connected_tx {
        Some(tx) => {
            let _ = tx.send(());
        }
        None => {
            let _ = cmd_tx.send(MqttCommand::Connected);
        }
    }
}

/// Map an inbound publish under `{base_topic}/#` to a bridge command.
///
/// Accepts z2m's forms `{name}/set`, `{name}/set/{attribute}`, `{name}/get`
/// and `{name}/get/{attribute}`, where `name` may itself contain '/'. Our own
/// state publishes echoed back by the `#` subscription match none of these
/// and are ignored.
fn command_from_publish(topic: &str, payload: &[u8], base_topic: &str) -> Option<MqttCommand> {
    let rest = topic.strip_prefix(base_topic)?.strip_prefix('/')?;
    if rest == "bridge/request/permit_join" {
        let duration = parse_permit_join_payload(payload);
        return Some(MqttCommand::PermitJoin { duration });
    }
    if rest.starts_with("bridge/") {
        return None;
    }

    let segments: Vec<&str> = rest.split('/').collect();
    let n = segments.len();
    let (verb_idx, attribute) = if n >= 2 && matches!(segments[n - 1], "set" | "get") {
        (n - 1, None)
    } else if n >= 3 && matches!(segments[n - 2], "set" | "get") {
        (n - 2, Some(segments[n - 1]))
    } else {
        return None;
    };
    let friendly_name = segments[..verb_idx].join("/");
    if friendly_name.is_empty() {
        return None;
    }

    if segments[verb_idx] == "get" {
        return Some(MqttCommand::GetDevice { friendly_name });
    }
    let parsed = serde_json::from_slice::<serde_json::Value>(payload);
    let payload = match attribute {
        // `{name}/set/brightness` with payload `200` == `{name}/set` with `{"brightness":200}`
        Some(attr) => {
            let value = match parsed {
                Ok(v) => v,
                Err(_) => match std::str::from_utf8(payload) {
                    Ok(s) => json!(s.trim()),
                    Err(_) => return None,
                },
            };
            json!({ attr: value })
        }
        // z2m accepts a bare state as well as a JSON object: `ON`, or `"ON"`.
        None => match parsed {
            Ok(serde_json::Value::String(s)) => json!({ "state": s.trim() }),
            Ok(v) => v,
            Err(_) => match std::str::from_utf8(payload) {
                Ok(s) => json!({ "state": s.trim() }),
                Err(_) => serde_json::Value::Null,
            },
        },
    };
    Some(MqttCommand::SetDevice {
        friendly_name,
        payload,
    })
}

/// Double the reconnect delay, capped at MQTT_RECONNECT_DELAY_MAX.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MQTT_RECONNECT_DELAY_MAX)
}

async fn subscribe_topics(client: &AsyncClient, base_topic: &str) -> bool {
    // Like z2m, subscribe to the whole tree: `+/set` can't match friendly
    // names containing '/' or the `{name}/set/{attribute}` form.
    let all = format!("{}/#", base_topic);

    for topic in &[&all] {
        if let Err(e) = client.subscribe(*topic, QoS::AtLeastOnce).await {
            error!("MQTT subscribe error for {topic}: {e}");
            return false;
        }
    }

    true
}
/// Parse permit_join payload. Supports z2m JSON format and plain number.
fn parse_permit_join_payload(payload: &[u8]) -> u8 {
    // Try JSON: {"value": true, "time": 254} or {"value": true}
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
        if let Some(time) = v.get("time").and_then(|t| t.as_u64()) {
            return time.min(254) as u8;
        }
        if let Some(val) = v.get("value") {
            if val.as_bool() == Some(true) {
                return 254;
            }
            if val.as_bool() == Some(false) {
                return 0;
            }
            if let Some(n) = val.as_u64() {
                return n.min(254) as u8;
            }
        }
    }
    // Plain number. Fail closed (no join) on anything unparseable, rather than
    // enabling pairing for the max duration on a garbage/unexpected payload.
    std::str::from_utf8(payload)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permit_join_json_with_time() {
        assert_eq!(
            parse_permit_join_payload(br#"{"value": true, "time": 120}"#),
            120
        );
    }

    #[test]
    fn parse_permit_join_json_true() {
        assert_eq!(parse_permit_join_payload(br#"{"value": true}"#), 254);
    }

    #[test]
    fn parse_permit_join_json_false() {
        assert_eq!(parse_permit_join_payload(br#"{"value": false}"#), 0);
    }

    #[test]
    fn parse_permit_join_plain_number() {
        assert_eq!(parse_permit_join_payload(b"120"), 120);
    }

    #[test]
    fn parse_permit_join_empty_fails_closed() {
        assert_eq!(parse_permit_join_payload(b""), 0);
    }

    #[test]
    fn parse_permit_join_garbage_fails_closed() {
        assert_eq!(parse_permit_join_payload(b"not a number or json"), 0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut delay = MQTT_RECONNECT_DELAY_MIN;
        for _ in 0..10 {
            delay = next_backoff(delay);
        }
        assert_eq!(delay, MQTT_RECONNECT_DELAY_MAX);
    }

    #[test]
    fn backoff_starts_at_double_the_min() {
        assert_eq!(
            next_backoff(MQTT_RECONNECT_DELAY_MIN),
            MQTT_RECONNECT_DELAY_MIN * 2
        );
    }

    #[test]
    fn link_state_logs_first_drop_only_and_resets_on_connect() {
        let link = LinkState::default();
        assert!(!link.is_connected());
        assert!(link.note_dropped());
        assert!(!link.note_dropped());
        assert_eq!(link.set_connected(), 2);
        assert!(link.is_connected());
        link.set_disconnected();
        assert!(link.note_dropped());
    }

    #[tokio::test]
    async fn publish_while_disconnected_never_blocks() {
        // The event loop is never polled, as during a broker outage, so
        // nothing drains rumqttc's request channel.
        let opts = MqttOptions::new("test", "localhost", 1883);
        let (client, _event_loop) = AsyncClient::new(opts, MQTT_REQUEST_CAPACITY);
        let bridge = MqttBridge {
            client,
            base_topic: "zigbee2mqtt".into(),
            link: Arc::new(LinkState::default()),
        };
        let state = json!({"state": "ON"});
        let publish_all = async {
            for _ in 0..MQTT_REQUEST_CAPACITY * 3 {
                bridge.publish_device_state("lamp", &state).await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(1), publish_all)
            .await
            .expect("publish blocked while disconnected");
        assert_eq!(
            bridge.link.set_connected(),
            (MQTT_REQUEST_CAPACITY * 2) as u64
        );
    }

    #[test]
    fn command_from_set_json() {
        match command_from_publish("zigbee2mqtt/lamp/set", br#"{"state":"ON"}"#, "zigbee2mqtt") {
            Some(MqttCommand::SetDevice {
                friendly_name,
                payload,
            }) => {
                assert_eq!(friendly_name, "lamp");
                assert_eq!(payload, json!({"state": "ON"}));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn command_from_set_bare_and_quoted_state() {
        for raw in [&b"ON"[..], &br#""ON""#[..]] {
            match command_from_publish("zigbee2mqtt/lamp/set", raw, "zigbee2mqtt") {
                Some(MqttCommand::SetDevice { payload, .. }) => {
                    assert_eq!(payload, json!({"state": "ON"}));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn command_from_get() {
        assert!(matches!(
            command_from_publish("zigbee2mqtt/lamp/get", b"", "zigbee2mqtt"),
            Some(MqttCommand::GetDevice { friendly_name }) if friendly_name == "lamp"
        ));
    }

    #[test]
    fn command_from_permit_join() {
        assert!(matches!(
            command_from_publish(
                "zigbee2mqtt/bridge/request/permit_join",
                br#"{"value": true, "time": 60}"#,
                "zigbee2mqtt"
            ),
            Some(MqttCommand::PermitJoin { duration: 60 })
        ));
    }

    #[test]
    fn command_from_unrelated_topic_is_none() {
        assert!(command_from_publish("zigbee2mqtt/lamp", b"{}", "zigbee2mqtt").is_none());
        // Only the exact bridge request topic, not anything mentioning it.
        assert!(
            command_from_publish("zigbee2mqtt/permit_join_lamp", b"1", "zigbee2mqtt").is_none()
        );
    }

    fn set_target(topic: &str, payload: &[u8]) -> Option<(String, serde_json::Value)> {
        match command_from_publish(topic, payload, "zigbee2mqtt") {
            Some(MqttCommand::SetDevice { friendly_name, payload }) => Some((friendly_name, payload)),
            _ => None,
        }
    }

    #[test]
    fn friendly_name_with_nested_path() {
        // z2m allows friendly names containing '/' (e.g. "room/light")
        assert_eq!(
            set_target("zigbee2mqtt/room/light/set", br#"{"state":"ON"}"#),
            Some(("room/light".into(), json!({"state": "ON"})))
        );
        assert!(matches!(
            command_from_publish("zigbee2mqtt/room/light/get", b"", "zigbee2mqtt"),
            Some(MqttCommand::GetDevice { friendly_name }) if friendly_name == "room/light"
        ));
    }

    #[test]
    fn set_with_attribute_suffix() {
        assert_eq!(
            set_target("zigbee2mqtt/lamp/set/brightness", b"200"),
            Some(("lamp".into(), json!({"brightness": 200})))
        );
        assert_eq!(
            set_target("zigbee2mqtt/room/lamp/set/state", b"ON"),
            Some(("room/lamp".into(), json!({"state": "ON"})))
        );
    }

    #[test]
    fn echoed_state_and_bridge_topics_ignored() {
        for topic in [
            "zigbee2mqtt/lamp",
            "zigbee2mqtt/lamp/state",
            "zigbee2mqtt/bridge/state",
            "zigbee2mqtt/bridge/devices",
            "zigbee2mqtt/set",
            "other/lamp/set",
        ] {
            assert!(command_from_publish(topic, b"x", "zigbee2mqtt").is_none(), "{topic}");
        }
    }
}
