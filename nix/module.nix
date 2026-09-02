self: { config, lib, pkgs, ... }:
let
  cfg = config.services.op-return-bot;
in {
  options.services.op-return-bot = {
    enable = lib.mkEnableOption "OP_RETURN Bot";
    package = lib.mkPackageOption self.packages.${pkgs.stdenv.hostPlatform.system} "op-return-bot" { };
    configFile = lib.mkOption {
      type = lib.types.path;
      description = "Path to the OP_RETURN Bot TOML configuration.";
    };
    credentials = lib.mkOption {
      type = lib.types.attrsOf lib.types.path;
      default = { };
      example = {
        bitcoin-rpc-password = "/run/secrets/bitcoin-rpc-password";
        wallet-notify-key = "/run/secrets/wallet-notify-key";
      };
      description = "Systemd credentials made available to the service.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.op-return-bot = {
      isSystemUser = true;
      group = "op-return-bot";
    };
    users.groups.op-return-bot = { };

    systemd.services.op-return-bot = {
      description = "OP_RETURN Bot";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" "bitcoind.service" "lnd.service" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        User = "op-return-bot";
        Group = "op-return-bot";
        StateDirectory = "op-return-bot";
        WorkingDirectory = "${cfg.package}/share/op-return-bot";
        ExecStart = "${lib.getExe' cfg.package "op-return-bot"} --config ${cfg.configFile}";
        LoadCredential = lib.mapAttrsToList (name: path: "${name}:${path}") cfg.credentials;
        Restart = "on-failure";
        RestartSec = "5s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectHome = true;
        ProtectSystem = "strict";
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RestrictNamespaces = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        UMask = "0077";
        ReadWritePaths = [ "/var/lib/op-return-bot" ];
      };
    };
  };
}
