# AGENTS.md -- Notes for AI Assistants

This file provides context for AI agents working on zigbee2mqtt-rs.

## Project Overview

A pure Rust drop-in replacement for zigbee2mqtt. Bridges Zigbee devices (via Z-Stack coordinators) to MQTT, with Home Assistant auto-discovery. Targets embedded ARM (aarch64 RPi 3).

## Critical Constraints

- **No C dependencies**. The `rumqttc` crate MUST use `default-features = false` to avoid pulling in `ring` (C/asm crypto). Never add crates that bundle C code.
- **Z-Stack 1.2 compatibility**. The CC2531 coordinator uses Z-Stack HA 1.2 (product_id=0). APP_CNF/BDB subsystem commands don't exist on this firmware. The codec must handle subsystem 0x00 in SRSP frames (error responses). Skip BDB channel config when `product_id < 1`.
- **zigbee2mqtt compatibility**. MQTT message formats must exactly match zigbee2mqtt for Home Assistant integration. Key patterns:
  - Bridge state: `{"state":"online"}` (JSON, not plain string)
  - IEEE addresses: lowercase hex `0xec1bbdfffeaa66db`
  - unique_id: `{ieee}_{type}_{base_topic}` (ends with `_zigbee2mqtt`)
  - Color state: nested `{"color":{"x":...,"y":...}}` not flat keys
  - Optimistic state: set commands must immediately publish state back to MQTT
  - Switch payload_on/off: strings `"ON"`/`"OFF"`, not JSON objects
- **database.db import**. The NDJSON format from zigbee-herdsman. Each line is a device record with camelCase fields (ieeeAddr, nwkAddr, manufName, epList, endpoints with inClusterList/outClusterList).

## Architecture Decisions

- **Single-threaded event loop**. The bridge runs one tokio task with `select!` over coordinator events and MQTT commands. DashMap is used for the device registry but true concurrency isn't needed -- it was chosen for ergonomic interior mutability.
- **Library + binary crate**. `src/lib.rs` exports all modules. `src/main.rs` is just CLI parsing. `src/bridge.rs` is in the library for testability.
- **Static cluster handlers**. `handler_for()` returns `&'static dyn ClusterHandler` -- no heap allocation per message.
- **ZNP transport actor**. Serial I/O runs in a spawned task. SREQ/SRSP pairing uses oneshot channels with 10s timeout. Mismatched subsystem SRSP is delivered to the pending request rather than dropped (Z-Stack 1.2 quirk).

## Common Pitfalls

1. **IEEE address case**: `IeeeAddr::as_hex()` returns lowercase. Config keys, MQTT topics, and HA discovery all use lowercase. Case mismatches break device lookup.
2. **NWK address lifecycle**: Pre-seeded devices (from config or database.db) have real NWK addresses from the database. Devices that join fresh get NWK from EndDeviceAnnceInd. Unknown NWK addresses trigger ZDO_IEEE_ADDR_REQ for resolution.
3. **Frame codec**: The ZNP codec must not discard frames with unknown subsystems. Z-Stack 1.2 returns subsystem 0x00 for error responses. Discarding these causes 10s timeouts.
4. **Optimistic state publishing**: After every set command, the expected state must be published back to MQTT immediately. Without this, Home Assistant UI reverts switches to off.

## Testing

137 tests across 4 test targets:
- **Library unit tests** (107): ZNP frame/command parsing, ZCL attribute/cluster handlers, device registry, HA discovery format, MQTT message parsing, database.db import
- **Bridge unit tests** (9 in main binary): IEEE parsing, timestamp generation, transition time, endpoint lookup (these will move to lib tests in a future refactor)
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
| `tests/z2m_compat.rs` | z2m format validation -- run after any MQTT/state changes |
