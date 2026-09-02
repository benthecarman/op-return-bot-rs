{
  description = "OP_RETURN Bot Rust service";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs@{ self, nixpkgs, flake-utils, crane, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
    in
    flake-utils.lib.eachSystem systems (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;
        source = pkgs.lib.cleanSource ./.;
        commonArgs = {
          src = source;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config pkgs.protobuf ];
        };
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        package = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          postInstall = ''
            mkdir -p $out/share/op-return-bot
            cp -r public $out/share/op-return-bot/public
            substitute scripts/walletnotify.sh $out/bin/op-return-bot-walletnotify \
              --replace-fail "exec curl" "exec ${pkgs.curl}/bin/curl"
            chmod 0555 $out/bin/op-return-bot-walletnotify
          '';
        });
      in {
        packages = {
          op-return-bot = package;
          default = package;
        };
        apps.default = {
          type = "app";
          program = "${package}/bin/op-return-bot";
          meta.description = "Run OP_RETURN Bot";
        };
        checks = {
          inherit package;
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- -D warnings";
          });
          tests = craneLib.cargoNextest (commonArgs // { inherit cargoArtifacts; });
        };
        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = [ pkgs.protobuf pkgs.sqlite ];
        };
      }) // {
        nixosModules.default = import ./nix/module.nix self;
      };
}
