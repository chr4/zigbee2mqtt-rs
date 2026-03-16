# Standalone Nix build — usable via pkgs.callPackage or nix-build.
# When using the flake, prefer the flake outputs instead (they cache deps).
{ lib
, rustPlatform
, pkg-config
, autoPatchelfHook
, stdenv
}:

rustPlatform.buildRustPackage {
  pname   = "zigbee2mqtt-rs";
  version = "0.1.0";

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ pkg-config autoPatchelfHook ];

  # Runtime: only glibc + libgcc_s (no udev, no openssl — uses rustls)
  buildInputs = [ stdenv.cc.cc.lib ];

  meta = {
    description = "Zigbee to MQTT bridge written in Rust";
    license     = lib.licenses.mit;
    platforms   = lib.platforms.linux;
    mainProgram = "zigbee2mqtt-rs";
  };
}
