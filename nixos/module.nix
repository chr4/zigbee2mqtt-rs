{ flake }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.zigbee2mqtt-rs;

  settingsFormat = pkgs.formats.yaml { };

  configFile = settingsFormat.generate "zigbee2mqtt-rs-config.yaml" cfg.settings;

in {
  options.services.zigbee2mqtt-rs = {

    enable = lib.mkEnableOption "zigbee2mqtt-rs Zigbee-to-MQTT bridge";

    package = lib.mkOption {
      type    = lib.types.package;
      # Prefer the pre-built package from the flake; fall back to callPackage
      # so the module also works when imported standalone (e.g. via overlays).
      default = flake.packages.${pkgs.stdenv.hostPlatform.system}.zigbee2mqtt-rs
                or (pkgs.callPackage ../. { });
      description = "The zigbee2mqtt-rs package to use.";
    };

    dataDir = lib.mkOption {
      type    = lib.types.path;
      default = "/var/lib/zigbee2mqtt-rs";
      description = "Directory for runtime state (devices, logs).";
    };

    user = lib.mkOption {
      type    = lib.types.str;
      default = "zigbee2mqtt-rs";
      description = "User account to run the service under.";
    };

    group = lib.mkOption {
      type    = lib.types.str;
      default = "zigbee2mqtt-rs";
      description = "Group for the service user.";
    };

    settings = lib.mkOption {
      type    = lib.types.submodule {
        freeformType = settingsFormat.type;
        options = {
          serial.port = lib.mkOption {
            type    = lib.types.str;
            default = "/dev/ttyACM0";
            description = "Zigbee adapter serial port.";
          };
          serial.baudrate = lib.mkOption {
            type    = lib.types.int;
            default = 115200;
          };
          serial.adapter = lib.mkOption {
            type    = lib.types.enum [ "znp" "ezsp" "auto" ];
            default = "znp";
          };
          mqtt.server = lib.mkOption {
            type    = lib.types.str;
            default = "localhost";
            description = "MQTT broker hostname.";
          };
          mqtt.port = lib.mkOption {
            type    = lib.types.port;
            default = 1883;
          };
          mqtt.base_topic = lib.mkOption {
            type    = lib.types.str;
            default = "zigbee2mqtt";
          };
          permit_join = lib.mkOption {
            type    = lib.types.bool;
            default = false;
            description = "Allow new devices to join on startup.";
          };
          advanced.channel = lib.mkOption {
            type    = lib.types.ints.between 11 26;
            default = 11;
            description = "Zigbee RF channel (11-26).";
          };
          advanced.log_level = lib.mkOption {
            type    = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
            default = "info";
          };
        };
      };
      default = { };
      description = "zigbee2mqtt-rs configuration (written to configuration.yaml).";
    };

  };

  config = lib.mkIf cfg.enable {

    # Create the service user/group
    users.users.${cfg.user} = {
      isSystemUser  = true;
      group         = cfg.group;
      home          = cfg.dataDir;
      createHome    = true;
      # Add to dialout for serial port access
      extraGroups   = [ "dialout" ];
      description   = "zigbee2mqtt-rs service user";
    };

    users.groups.${cfg.group} = { };

    # udev rule so the adapter is accessible to the dialout group
    services.udev.extraRules = ''
      SUBSYSTEM=="tty", ATTRS{idVendor}=="0451", ATTRS{idProduct}=="16a8", \
        MODE="0660", GROUP="dialout", SYMLINK+="zigbee"
      SUBSYSTEM=="tty", ATTRS{idVendor}=="1a86", ATTRS{idProduct}=="7523", \
        MODE="0660", GROUP="dialout", SYMLINK+="zigbee"
    '';

    # systemd service
    systemd.services.zigbee2mqtt-rs = {
      description   = "zigbee2mqtt-rs Zigbee-to-MQTT bridge";
      after         = [ "network.target" "mosquitto.service" ];
      wants         = [ "mosquitto.service" ];
      wantedBy      = [ "multi-user.target" ];

      serviceConfig = {
        Type             = "simple";
        User             = cfg.user;
        Group            = cfg.group;
        WorkingDirectory = cfg.dataDir;
        ExecStartPre     = "+${pkgs.coreutils}/bin/cp ${configFile} ${cfg.dataDir}/configuration.yaml";
        ExecStart        = "${cfg.package}/bin/zigbee2mqtt-rs --config ${cfg.dataDir}/configuration.yaml";

        Restart          = "on-failure";
        RestartSec       = "5s";

        # Hardening
        NoNewPrivileges       = true;
        ProtectSystem         = "strict";
        ProtectHome           = true;
        ReadWritePaths        = [ cfg.dataDir ];
        PrivateTmp            = true;
        PrivateDevices        = false;  # needs /dev/ttyACM0
        DeviceAllow           = [ "char-ttyACM rw" "char-ttyUSB rw" ];
        CapabilityBoundingSet = "";
        LockPersonality       = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime      = true;
        RestrictNamespaces    = true;
        SystemCallFilter      = "@system-service";

        # Journal logging
        StandardOutput = "journal";
        StandardError  = "journal";
        SyslogIdentifier = "zigbee2mqtt-rs";
      };
    };

  };
}
