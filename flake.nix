{
  description = "zigbee2mqtt-rs – Zigbee ↔ MQTT bridge in Rust";

  inputs = {
    nixpkgs.url     = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay    = {
      url    = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url        = "github:ipetkov/crane";
    flake-utils.url  = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, crane, flake-utils, ... }:
    let
      # nixosModules must live at the TOP LEVEL (outside eachDefaultSystem)
      # because NixOS modules are not per-system.
      nixosModules = {
        default = import ./nixos/module.nix { flake = self; };
      };

      # Per-system outputs (packages, devShells, checks)
      perSystem = flake-utils.lib.eachDefaultSystem (system:
        let
          overlays    = [ (import rust-overlay) ];
          pkgs        = import nixpkgs { inherit system overlays; };

          # Stable Rust with aarch64 cross-compilation target
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "aarch64-unknown-linux-gnu" ];
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # Common source filter (avoids rebuilds when only docs change)
          src = craneLib.cleanCargoSource ./.;

          # Build-time deps — note: no libudev needed (binary is glibc-only)
          commonArgs = {
            inherit src;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [];
          };

          # Cache dependency compilation separately for faster iteration
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # ── Native build ─────────────────────────────────────────────────
          zigbee2mqtt-rs = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            # autoPatchelfHook sets the correct ELF interpreter and RPATH
            # so the binary runs on NixOS (no FHS /lib64 needed)
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
              pkgs.autoPatchelfHook
            ];
            buildInputs = [ pkgs.stdenv.cc.cc.lib ]; # libgcc_s, libm
          });

          # ── Cross-compiled aarch64 binary (Raspberry Pi 3) ───────────────
          pkgsCross = import nixpkgs {
            inherit system overlays;
            crossSystem.config = "aarch64-unknown-linux-gnu";
          };

          craneLibCross = (crane.mkLib pkgsCross).overrideToolchain rustToolchain;

          cargoArtifactsCross = craneLibCross.buildDepsOnly (commonArgs // {
            src = craneLib.cleanCargoSource ./.;
            strictDeps           = true;
            CARGO_BUILD_TARGET   = "aarch64-unknown-linux-gnu";
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER =
              "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            nativeBuildInputs = [ pkgsCross.buildPackages.pkg-config ];
            buildInputs = [];
          });

          zigbee2mqtt-rs-aarch64 = craneLibCross.buildPackage {
            src              = craneLib.cleanCargoSource ./.;
            cargoArtifacts   = cargoArtifactsCross;
            strictDeps       = true;
            CARGO_BUILD_TARGET = "aarch64-unknown-linux-gnu";
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER =
              "${pkgsCross.stdenv.cc}/bin/${pkgsCross.stdenv.cc.targetPrefix}cc";
            nativeBuildInputs = [
              pkgsCross.buildPackages.pkg-config
              pkgsCross.buildPackages.autoPatchelfHook or pkgs.autoPatchelfHook
            ];
            buildInputs = [ pkgsCross.stdenv.cc.cc.lib ];
          };

          # ── Checks ───────────────────────────────────────────────────────
          checks = {
            # Run `cargo test`
            tests = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
            # Run `cargo clippy`
            clippy = craneLib.cargoClippy (commonArgs // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "-- -D warnings";
            });
            # Check formatting
            fmt = craneLib.cargoFmt { inherit src; };
          };

        in {
          inherit checks;

          packages = {
            default            = zigbee2mqtt-rs;
            zigbee2mqtt-rs     = zigbee2mqtt-rs;
            aarch64            = zigbee2mqtt-rs-aarch64;
          };

          devShells.default = pkgs.mkShell {
            inputsFrom = [ zigbee2mqtt-rs ];
            packages   = with pkgs; [
              rustToolchain
              rust-analyzer
              cargo-watch
              cargo-expand
              mosquitto   # mosquitto_pub / mosquitto_sub for testing
              minicom     # serial port debugging
            ];
          };
        }
      );

    in
      perSystem // { inherit nixosModules; };
}
