{ config, pkgs, ... }:

{
  imports = [
    # Copy this from the Pi's existing install, don't hand-write it:
    #   scp root@zigbee-pi:/etc/nixos/hardware-configuration.nix ./
    ./hardware-configuration.nix
  ];

  networking.hostName = "zigbee-pi";
  nixpkgs.hostPlatform = "aarch64-linux";

  time.timeZone = "Europe/Berlin"; # adjust

  # Needed for `nixos-rebuild --target-host` deploys from the dev machine.
  services.openssh.enable = true;
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAA... replace-with-your-deploy-key"
  ];

  # Local MQTT broker on the Pi. Skip this if you already point
  # mqtt.server at a broker running elsewhere.
  services.mosquitto = {
    enable = true;
    listeners = [{
      address = "0.0.0.0";
      port = 1883;
      settings.allow_anonymous = true;
      acl = [ "pattern readwrite #" ];
    }];
  };

  services.zigbee2mqtt-rs = {
    enable = true;
    settings = {
      serial.port = "/dev/ttyACM0";
      serial.adapter = "znp";
      mqtt.server = "localhost";
      mqtt.base_topic = "zigbee2mqtt-rs";
      permit_join = false;
      advanced.channel = 11;
    };
  };

  networking.firewall.allowedTCPPorts = [ 1883 ];

  # Keep this at whatever value was set when the Pi was first installed --
  # changing it retroactively can trigger unwanted state-format migrations.
  system.stateVersion = "24.05";
}
