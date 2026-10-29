# AGENTS.md -- Notes for AI Assistants

This file provides context for AI agents working on zigbee2mqtt-rs.

## Project Overview

A pure Rust drop-in replacement for zigbee2mqtt. Bridges Zigbee devices (via Z-Stack coordinators) to MQTT, with Home Assistant auto-discovery. Targets embedded ARM (aarch64 RPi 3).

## Critical Constraints

- **No C dependencies**. The `rumqttc` crate MUST use `default-features = false` to avoid pulling in `ring` (C/asm crypto). Never add crates that bundle C code.
- **Z-Stack 1.2 compatibility**. The CC2531 coordinator uses Z-Stack HA 1.2 (product_id=0). APP_CNF/BDB subsystem commands don't exist on this firmware; it answers them with an RPC error SRSP (see transport below). Skip BDB channel config when `product_id < 1`.
- **zigbee2mqtt compatibility**. MQTT message formats must exactly match zigbee2mqtt for Home Assistant integration. Key patterns:
  - Bridge state: `{"state":"online"}` (JSON, not plain string)
  - IEEE addresses: lowercase hex `0xec1bbdfffeaa66db`
  - unique_id: `{ieee}_{type}_{base_topic}` (ends with `_zigbee2mqtt`)
  - Color state: nested `{"color":{"x":...,"y":...}}` not flat keys
  - Optimistic state: set commands must immediately publish state back to MQTT
  - Switch payload_on/off: strings `"ON"`/`"OFF"`, not JSON objects
- **database.db import**. The NDJSON format from zigbee-herdsman. Each line is a device record with camelCase fields (ieeeAddr, nwkAddr, manufName, epList, endpoints with inClusterList/outClusterList).

## Architecture Decisions

- **Event loop never blocks on round-trips**. The bridge runs one tokio task with `select!` over coordinator events, MQTT commands and a shutdown signal. Coordinator request sequences (simple-desc requests, binds, attribute reads, IAS enrollment, IEEE lookups) run in spawned tasks sharing `Arc<Mutex<ZnpHandle>>`; ZCL transaction IDs come from a shared `AtomicU8`. Never hold a DashMap ref across an `.await`.
- **MQTT event loop must never block**. The task driving rumqttc's `EventLoop::poll()` must not await the request channel or the bridge (subscribe runs in its own task; inbound commands use an unbounded channel), otherwise a broker reconnect deadlocks. Max packet size is raised from rumqttc's 10 KiB default, which errors the *connection* on a large `bridge/devices`. Like z2m, we subscribe to `{base}/#` and route `{name}/set[/{attr}]` and `{name}/get` (names may contain `/`).
- **Persistence**. `src/persistence.rs` merges registry changes into `database.db` (herdsman NDJSON, unknown fields/records preserved) and writes `state.json`, atomically, every 30s and on exit.
- **Library + binary crate**. `src/lib.rs` exports all modules. `src/main.rs` is just CLI parsing. `src/bridge.rs` is in the library for testability.
- **Static cluster handlers**. `handler_for()` returns `&'static dyn ClusterHandler` -- no heap allocation per message.
- **ZNP transport actor**. Serial I/O runs in a spawned task. One SREQ is in flight at a time; the actor owns the 10s SRSP timeout. An SRSP matches the pending request by (subsystem, cmd1). An RPC error SRSP (subsystem 0x00, cmd1 0x00, payload `ErrorCode, ReqCmd0, ReqCmd1` -- layout per zigpy-znp `RPCError.CommandNotRecognized`) is matched by the embedded request header and returned as `Error::ZnpRejected`.
- **Fail fast, let the supervisor restart**. Serial read/write errors, 3 consecutive SREQ timeouts (hung adapter), or a SYS_RESET_IND after startup (endpoint registration and network state lost) end the transport actor / event pump. That closes `CoordinatorHandle::events`, and the bridge must exit non-zero so systemd/docker re-runs the full init.

## Common Pitfalls

1. **IEEE address case**: `IeeeAddr::as_hex()` returns lowercase. Config keys, MQTT topics, and HA discovery all use lowercase. Case mismatches break device lookup.
2. **NWK address lifecycle**: Pre-seeded devices (from config or database.db) have real NWK addresses from the database. Devices that join fresh get NWK from EndDeviceAnnceInd. Unknown NWK addresses trigger ZDO_IEEE_ADDR_REQ for resolution.
3. **Frame codec**: The ZNP codec must not discard frames with unknown subsystems. Error responses arrive as subsystem 0x00 (RPC error); dropping them would turn every rejected command into a 10s timeout.
4. **Optimistic state publishing**: After every set command, the expected state must be published back to MQTT immediately. Without this, Home Assistant UI reverts switches to off.

## Testing

~275 tests across 2 test targets:
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
| `src/bridge.rs` | Main event loop -- most changes land here |
| `src/homeassistant.rs` | HA discovery -- must match z2m format exactly |
| `src/coordinator/znp/mod.rs` | Z-Stack init sequence -- fragile, version-dependent |
| `src/coordinator/znp/transport.rs` | Serial transport -- SREQ/SRSP matching logic |
| `src/database.rs` | database.db import -- camelCase NDJSON parsing |
| `src/persistence.rs` | database.db / state.json writing -- merge, don't regenerate |
| `src/mqtt/mod.rs` | MQTT client -- event-loop task must never block |
| `tests/z2m_compat.rs` | z2m format validation -- run after any MQTT/state changes |
