# AGENTS.md -- Notes for AI Assistants

This file provides context for AI agents working on zigbee2mqtt-rs.

## Project Overview

A pure Rust drop-in replacement for zigbee2mqtt. Bridges Zigbee devices (via Z-Stack coordinators) to MQTT, with Home Assistant auto-discovery. Targets embedded ARM (aarch64 RPi 3).

## Critical Constraints

- **No C dependencies**. The `rumqttc` crate MUST use `default-features = false` to avoid pulling in `ring` (C/asm crypto). Never add crates that bundle C code.
- **Network key distribution**. NV `PRECFGKEYS_ENABLE` must be written as 0 (FALSE): TRUE makes the trust center send an all-zero key to joining and TC-rejoining devices.
- **Z-Stack 1.2 compatibility**. The CC2531 coordinator uses Z-Stack HA 1.2 (product_id=0). APP_CNF/BDB subsystem commands don't exist on this firmware; it answers them with an RPC error SRSP (see transport below). Skip BDB channel config when `product_id < 1`.
- **zigbee2mqtt compatibility**. MQTT message formats must exactly match zigbee2mqtt for Home Assistant integration. Key patterns:
  - Bridge state: `{"state":"online"}` (JSON, not plain string)
  - IEEE addresses: lowercase hex `0xec1bbdfffeaa66db`
  - unique_id: `{ieee}_{type}_{base_topic}` (ends with `_zigbee2mqtt`)
  - Color state: nested `{"color":{"x":...,"y":...}}` not flat keys
  - Optimistic state: after a set command the expected state must be published back to MQTT as soon as the coordinator confirms delivery (AF_DATA_CONFIRM success, or no confirm within its timeout); never for a command whose delivery failed
  - Switch payload_on/off: strings `"ON"`/`"OFF"`, not JSON objects
  - Legacy action sensor: a state publish containing `action` is followed by one with `action: ""`, so repeated identical presses each trigger HA
  - Retained command messages (`/set`, `bridge/request/...`) replayed by the broker on subscribe are ignored
- **database.db import**. The NDJSON format from zigbee-herdsman. Each line is a device record with camelCase fields (ieeeAddr, nwkAddr, manufName, epList, endpoints with inClusterList/outClusterList).

## Architecture Decisions

- **Event loop never blocks on round-trips; it is the only state writer**. The bridge runs one tokio task (`bridge.rs`, `Runtime`) with `select!` over coordinator events, MQTT commands, `Internal` results and housekeeping. Anything that talks to a device runs in that device's *lane* (`bridge/jobs.rs`: an ordered per-device job queue for set/get/refresh/setup) or a spawned task, sharing `Arc<ZnpHandle>` (no lock: the transport actor serializes SREQs). Lanes never mutate device state; they send `Internal` events back and the main loop merges and publishes. `bridge/commands.rs` holds the pure set/get → ZCL planning. Never hold a DashMap ref across an `.await`, and never lock one registry map while holding another.
- **Delivery is confirmed**. `ZnpHandle::send_zcl` waits for the AF_DATA_CONFIRM matching its AF transaction ID (APS ack requested, options 0x10 -- never 0x20, which suppresses route discovery) and limits in-flight data requests (2 on Z-Stack 1.2). `jobs::send_to_device` retries like zigbee-herdsman: back off on congestion, rediscover the route, then re-resolve the NWK address by IEEE (ZDO_NWK_ADDR_REQ).
- **Keeping state true**. Lights are re-read after a device announce (power cycle) and at startup; `/get` reads the device. Newly interviewed devices, and interviewed ones neither z2m (`meta.configured`) nor we (`meta.zigbee2mqttRsConfigured`) configured, get input clusters bound and attribute reporting configured (LUMI devices excluded). Setups run one at a time, outside the device lanes (`jobs::spawn_setup`); a trust-center (re)join redoes the full setup (at most every 10 min per device; setups are generation-tagged and aborted when the device leaves), like z2m reconfiguring on join. Incoming frames that request one get a ZCL Default Response. IAS ZoneType is persisted (herdsman cluster cache) and re-read when unknown; HA discovery is republished (stale entity cleared) when it changes.
- **MQTT event loop must never block**. The task driving rumqttc's `EventLoop::poll()` must not await the request channel or the bridge (subscribe runs in its own task; inbound commands use an unbounded channel), otherwise a broker reconnect deadlocks. Max packet size is raised from rumqttc's 10 KiB default, which errors the *connection* on a large `bridge/devices`. Like z2m, we subscribe to `{base}/#` and route `{name}/set[/{attr}]` and `{name}/get` (names may contain `/`).
- **Persistence**. `src/persistence.rs` merges registry changes into `database.db` (herdsman NDJSON, unknown fields/records preserved; only devices that left are removed) every 30s, `state.json` every 5 min (SD-card wear), both atomically and on exit.
- **Library + binary crate**. `src/lib.rs` exports all modules. `src/main.rs` is just CLI parsing. `src/bridge.rs` is in the library for testability.
- **Static cluster handlers**. `handler_for()` returns `&'static dyn ClusterHandler` -- no heap allocation per message.
- **ZNP transport actor**. Serial I/O runs in a spawned task. One SREQ is in flight at a time; the actor owns the 10s SRSP timeout. An SRSP matches the pending request by (subsystem, cmd1). An RPC error SRSP (subsystem 0x00, cmd1 0x00, payload `ErrorCode, ReqCmd0, ReqCmd1` -- layout per zigpy-znp `RPCError.CommandNotRecognized`) is matched by the embedded request header and returned as `Error::ZnpRejected`.
- **Fail fast, let the supervisor restart**. Serial read/write errors, 3 consecutive SREQ timeouts (hung adapter; housekeeping pings it every 60s so this is noticed while idle), a second reset during init, or a SYS_RESET_IND after startup (endpoint registration and network state lost; never dropped even if the event queue is full) end the transport actor / event pump. That closes `CoordinatorHandle::events`, and the bridge must exit non-zero so systemd/docker re-runs the full init.

## Common Pitfalls

1. **IEEE address case**: `IeeeAddr::as_hex()` returns lowercase. Config keys, MQTT topics, and HA discovery all use lowercase. Case mismatches break device lookup.
2. **NWK address lifecycle**: Pre-seeded devices (from config or database.db) have real NWK addresses from the database. Devices that join fresh get NWK from EndDeviceAnnceInd. Unknown NWK addresses trigger ZDO_IEEE_ADDR_REQ for resolution.
3. **Frame codec**: The ZNP codec must not discard frames with unknown subsystems. Error responses arrive as subsystem 0x00 (RPC error); dropping them would turn every rejected command into a 10s timeout.
4. **Optimistic state publishing**: After every delivered set command, the expected state must be published back to MQTT. Without this, Home Assistant UI reverts switches to off.
5. **Endpoint order**: endpoints are kept sorted; the first endpoint with a cluster receives its commands. database.db endpoints come from a JSON object, so they must be sorted on import.

## Testing

~320 tests across 2 test targets (the transport/coordinator tests drive the real actor over an in-memory pipe, see `transport::fake`):
- **Library unit tests**: ZNP frame/command parsing and SRSP matching, ZCL attribute/cluster handlers, device registry, HA discovery format, MQTT routing, database.db import/persistence, bridge helpers
- **z2m compatibility tests** (30 in `tests/z2m_compat.rs`): End-to-end validation against real zigbee2mqtt test patterns from the upstream test suite

Run: `cargo test`

## Build

```bash
cargo build --release                                           # native
cargo build --release --target aarch64-unknown-linux-gnu        # ARM cross
```

Cross-compilation requires `gcc-aarch64-linux-gnu` and the `.cargo/config.toml` linker config.

## Key Files

| File | Purpose |
|---|---|
| `src/bridge.rs` | Main event loop (single state writer) -- most changes land here |
| `src/bridge/jobs.rs` | Per-device lanes: set/get/refresh/setup, delivery retries, reporting config |
| `src/bridge/commands.rs` | Pure z2m set/get payload → ZCL command/read planning |
| `src/homeassistant.rs` | HA discovery -- must match z2m format exactly |
| `src/coordinator/znp/mod.rs` | Z-Stack init sequence -- fragile, version-dependent |
| `src/coordinator/znp/transport.rs` | Serial transport -- SREQ/SRSP matching logic |
| `src/database.rs` | database.db import -- camelCase NDJSON parsing |
| `src/persistence.rs` | database.db / state.json writing -- merge, don't regenerate |
| `src/mqtt/mod.rs` | MQTT client -- event-loop task must never block |
| `tests/z2m_compat.rs` | z2m format validation -- run after any MQTT/state changes |
