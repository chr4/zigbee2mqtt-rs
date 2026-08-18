{
  description = "Example NixOS system flake for a Raspberry Pi 3B running zigbee2mqtt-rs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Pull the service + package + module straight from the project's git remote.
    # Swap for "git+ssh://git@github.com/chr4/zigbee2mqtt-rs.git" if the repo is private.
    zigbee2mqtt-rs = {
      url = "github:chr4/zigbee2mqtt-rs";
      # Keep the Pi's nixpkgs in lockstep with the one the package was built
      # against, so you don't pull down/build a second copy of the world.
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, zigbee2mqtt-rs, ... }: {
    nixosConfigurations.zigbee-pi = nixpkgs.lib.nixosSystem {
      system = "aarch64-linux";
      modules = [
        zigbee2mqtt-rs.nixosModules.default
        ./configuration.nix

        # Point the module at the flake's real cross-compiled binary
        # (built with the aarch64-unknown-linux-gnu Rust target on the x86_64
        # dev machine) instead of its default, which resolves to a *native*
        # aarch64-linux build -- fine on a real remote builder, but forces
        # QEMU-emulated compilation if you build locally on x86_64.
        {
          services.zigbee2mqtt-rs.package =
            zigbee2mqtt-rs.packages.x86_64-linux.aarch64;
        }
      ];
    };
  };
}
