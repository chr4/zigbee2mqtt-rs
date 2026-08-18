# zigbee2mqtt-rs

A drop-in replacement for [zigbee2mqtt](https://www.zigbee2mqtt.io/) written in pure Rust. Bridges Zigbee devices to MQTT with full Home Assistant auto-discovery support.

## Features

- **Drop-in replacement** -- imports existing zigbee2mqtt `database.db` so devices don't need re-pairing
- **Home Assistant MQTT discovery** -- lights, switches, sensors, binary sensors, diagnostics
- **Pure Rust** -- no C dependencies, no TLS libraries (ring/openssl), fast cross-compilation
- **Z-Stack support** -- works with CC2531 (Z-Stack 1.2) and CC2652/CC1352 (Z-Stack 3.0)
- **ZCL cluster support** -- On/Off, Level, Color (HS/XY/CT), Temperature, Humidity, Illuminance, Occupancy, IAS Zone, Power/Battery
- **Optimistic state** -- set commands immediately publish expected state back to MQTT
- **Small binary** -- optimized for embedded targets like Raspberry Pi 3

## Quick Start

```bash
# Build
cargo build --release

# Run (looks for configuration.yaml in current directory)
./target/release/zigbee2mqtt-rs

# Or specify config path
./target/release/zigbee2mqtt-rs -c /path/to/configuration.yaml
```

## Cross-Compile for ARM (aarch64)

```bash
# Install cross-compiler (Ubuntu/Debian)
sudo apt install gcc-aarch64-linux-gnu

# Add Rust target
rustup target add aarch64-unknown-linux-gnu

# Build (the .cargo/config.toml configures the linker automatically)
cargo build --release --target aarch64-unknown-linux-gnu

# Binary at: target/aarch64-unknown-linux-gnu/release/zigbee2mqtt-rs
```

## Nix / NixOS

The repo ships a flake (`flake.nix`), a standalone `default.nix`, and a NixOS
module (`nixos/module.nix`) -- no manual cross-compiler setup required.

### Building with Nix

```bash
# Native build for your current system
nix build .#zigbee2mqtt-rs

# Cross-compiled aarch64 binary (Raspberry Pi 3), built via real Rust cross-
# compilation -- no QEMU emulation needed
nix build .#aarch64
# Binary at: result/bin/zigbee2mqtt-rs

# Without flakes (nix-command/flakes experimental features disabled):
nix-build default.nix

# Dev shell with Rust toolchain, rust-analyzer, cargo-watch, mosquitto
# CLI tools, and minicom preinstalled
nix develop
```

### Installing as a NixOS service

Add this repo as a flake input and import its module:

```nix
{
  inputs.zigbee2mqtt-rs.url = "github:chr4/zigbee2mqtt-rs";
  # For a private fork: "git+ssh://git@github.com/<you>/zigbee2mqtt-rs.git"

  outputs = { self, nixpkgs, zigbee2mqtt-rs, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux"; # or "aarch64-linux" on a Pi
      modules = [
        zigbee2mqtt-rs.nixosModules.default
        {
          services.zigbee2mqtt-rs = {
            enable = true;
            settings = {
              serial.port = "/dev/ttyACM0";
              mqtt.server = "localhost";
              permit_join = false;
            };
          };
        }
      ];
    };
  };
}
```

Available `services.zigbee2mqtt-rs` options:

| Option | Default | Description |
|---|---|---|
| `enable` | `false` | Enable the service |
| `package` | flake's native build | Package providing the binary |
| `dataDir` | `/var/lib/zigbee2mqtt-rs` | Runtime state directory |
| `user` / `group` | `zigbee2mqtt-rs` | Service account |
| `settings.serial.port` | `/dev/ttyACM0` | Zigbee adapter serial port |
| `settings.serial.baudrate` | `115200` | Serial baud rate |
| `settings.serial.adapter` | `znp` | `znp`, `ezsp`, or `auto` |
| `settings.mqtt.server` | `localhost` | MQTT broker host |
| `settings.mqtt.port` | `1883` | MQTT broker port |
| `settings.mqtt.base_topic` | `zigbee2mqtt` | MQTT topic prefix |
| `settings.permit_join` | `false` | Allow new devices to join on startup |
| `settings.advanced.channel` | `11` | Zigbee RF channel (11-26) |
| `settings.advanced.log_level` | `info` | `trace`, `debug`, `info`, `warn`, `error` |

`settings` accepts any other key from the [Configuration](#configuration)
section too (it's a free-form YAML passthrough on top of the typed options
above). The service runs hardened by default (`ProtectSystem = "strict"`,
locked-down syscall filter, no new privileges) and creates a udev rule
granting the `dialout` group access to common Zigbee USB adapters (Texas
Instruments CC253x/CC26x2, CH340).

### Deploying to a Raspberry Pi 3B

`nixos/pi-example/` contains a full example: a system flake that pulls this
project straight from its git remote and a `configuration.nix` for the Pi.
Given a Pi 3B that already runs NixOS:

```bash
cd nixos/pi-example
# Copy the Pi's existing hardware config -- don't hand-write a new one
scp root@zigbee-pi:/etc/nixos/hardware-configuration.nix .

# Cross-compile on this machine and push the closure over SSH
nixos-rebuild switch --flake .#zigbee-pi \
  --target-host root@zigbee-pi --build-host localhost
```

The example wires `services.zigbee2mqtt-rs.package` to this flake's
`packages.x86_64-linux.aarch64` output, so the Rust binary is built via real
cross-compilation on your dev machine rather than emulated aarch64
compilation. Everything else in the system closure (systemd, glibc, etc.)
substitutes from `cache.nixos.org`, since aarch64-linux is a Tier-1 nixpkgs
platform.

## Configuration

Uses the same `configuration.yaml` format as zigbee2mqtt:

```yaml
serial:
  port: /dev/ttyACM0
  baudrate: 115200
  adapter: znp    # znp (TI CC2531/CC2652) or auto
  rtscts: false

mqtt:
  server: localhost
  port: 1883
  base_topic: zigbee2mqtt
  client_id: zigbee2mqtt-rs
  # username: my_user
  # password: my_password
  keepalive: 60

permit_join: true
homeassistant: true

advanced:
  pan_id: 0x1a62
  channel: 11
  network_key: [1, 3, 5, 7, 9, 11, 13, 15, 0, 2, 4, 6, 8, 10, 12, 13]
  log_level: info

devices:
  '0xec1bbdfffeaa66db':
    friendly_name: living_room_bulb
  '0xcc86ecfffe9fd1b1':
    friendly_name: bedroom_sensor
```

## Migrating from zigbee2mqtt

1. Stop zigbee2mqtt
2. Copy `database.db` from zigbee2mqtt's data directory to the same directory as `configuration.yaml`
3. Update `configuration.yaml` with your settings (or copy from zigbee2mqtt)
4. Start zigbee2mqtt-rs -- it will import all paired devices automatically

The bridge auto-discovers `database.db` in these locations:
- Same directory as `configuration.yaml`
- `data/` subdirectory
- `/opt/zigbee2mqtt/data/`
- `/var/lib/zigbee2mqtt/`

## MQTT Topics

Fully compatible with zigbee2mqtt's MQTT interface:

| Topic | Description |
|---|---|
| `zigbee2mqtt/bridge/state` | `{"state":"online"}` or `{"state":"offline"}` |
| `zigbee2mqtt/bridge/info` | Coordinator version, network info |
| `zigbee2mqtt/bridge/devices` | JSON array of all devices |
| `zigbee2mqtt/bridge/logging` | Log messages |
| `zigbee2mqtt/<name>` | Device state (retained) |
| `zigbee2mqtt/<name>/set` | Send commands to device |
| `zigbee2mqtt/<name>/get` | Request current state |
| `zigbee2mqtt/bridge/request/permit_join` | `{"value":true,"time":254}` |

## Set Command Examples

```bash
# Turn on a light
mosquitto_pub -t 'zigbee2mqtt/bulb/set' -m '{"state":"ON"}'

# Set brightness
mosquitto_pub -t 'zigbee2mqtt/bulb/set' -m '{"brightness":200}'

# Set color temperature
mosquitto_pub -t 'zigbee2mqtt/bulb/set' -m '{"color_temp":370}'

# Set color (XY)
mosquitto_pub -t 'zigbee2mqtt/bulb/set' -m '{"color":{"x":0.37,"y":0.28}}'

# Set color (HS)
mosquitto_pub -t 'zigbee2mqtt/bulb/set' -m '{"color":{"hue":250,"saturation":50}}'

# Combined with transition
mosquitto_pub -t 'zigbee2mqtt/bulb/set' -m '{"state":"ON","brightness":254,"transition":2.0}'

# Permit join
mosquitto_pub -t 'zigbee2mqtt/bridge/request/permit_join' -m '{"value":true,"time":120}'
```

## Supported Devices

Any Zigbee device using standard ZCL clusters:

| Cluster | Devices | State Fields |
|---|---|---|
| On/Off (0x0006) | Lights, switches, plugs | `state` |
| Level (0x0008) | Dimmable lights | `brightness` |
| Color (0x0300) | Color lights | `color`, `color_temp`, `color_mode` |
| Temperature (0x0402) | Temp sensors | `temperature` |
| Humidity (0x0405) | Humidity sensors | `humidity` |
| Illuminance (0x0400) | Light sensors | `illuminance` |
| Occupancy (0x0406) | Motion sensors | `occupancy` |
| IAS Zone (0x0500) | Door/window, smoke | `contact`, `tamper` |
| Power (0x0001) | Battery devices | `battery`, `battery_low` |

## Development

```bash
# Run tests
cargo test

# Run with debug logging
RUST_LOG=zigbee2mqtt_rs=debug cargo run -- -l debug

# Check for warnings
cargo clippy
```

## Architecture

```
src/
  main.rs           - CLI entry point
  lib.rs            - Library crate root
  bridge.rs         - Main event loop, MQTT command handling
  config.rs         - YAML configuration parsing
  database.rs       - zigbee2mqtt database.db import
  error.rs          - Error types
  homeassistant.rs  - HA MQTT discovery messages
  mqtt/mod.rs       - MQTT client, publish/subscribe
  coordinator/
    mod.rs          - Adapter-agnostic coordinator interface
    znp/
      mod.rs        - Z-Stack ZNP initialization and event pump
      commands.rs   - ZNP command builders and response parsers
      frame.rs      - ZNP frame codec (SOF/FCS)
      transport.rs  - Async serial transport with SREQ/SRSP pairing
  devices/mod.rs    - Device registry with IEEE/NWK/name indexes
  zigbee/
    mod.rs          - IeeeAddr, NwkAddr, EndpointDesc types
    zcl/
      mod.rs        - ZCL message parsing
      frame.rs      - ZCL frame header parsing
      attribute.rs  - ZCL attribute types and value parsing
      clusters/     - Per-cluster report/command handlers
```

## License

MIT
