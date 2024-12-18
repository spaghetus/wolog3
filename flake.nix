{
  inputs = {
    nixpkgs.url = "nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
  };
  outputs = inputs @ {
    self,
    nixpkgs,
    flake-parts,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      flake = {system, ...}: rec {
        overlays.default = final: prev: {
          inherit final prev;
          wolog = self.packages."${final.system}".default;
        };
        nixosModules.default = import ./module.nix;
        nixosConfigurations.container = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            {nixpkgs.overlays = [overlays.default];}
            nixosModules.default
            ({pkgs, ...}: {
              # Only allow this to boot as a container
              boot.isContainer = true;
              networking.hostName = "wolog-hello";

              # Allow nginx through the firewall
              networking.firewall.allowedTCPPorts = [8000];

              services.wolog.enable = true;
              services.wolog.articlesDir = builtins.toString ./articles;
              services.wolog.address = "0.0.0.0";
            })
          ];
        };
      };

      perSystem = {
        system,
        pkgs,
        lib,
        inputs',
        ...
      }: let
        cargo = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        pkg = pkgs.rustPlatform.buildRustPackage rec {
          pname = cargo.package.name;
          version = cargo.package.version;
          nativeBuildInputs = [pkgs.pkg-config];
          buildInputs = with pkgs; [openssl];
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          SQLX_OFFLINE = true;
          src = pkgs.runCommand "src" {} ''
            mkdir $out
            cp -r ${./src} $out/src
            cp -r ${./.sqlx} $out/.sqlx
            cp -r ${./migrations} $out/migrations
            cp -r ${./Cargo.toml} $out/Cargo.toml
            cp -r ${./Cargo.lock} $out/Cargo.lock
          '';
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
        };
      in rec {
        packages = {
          wolog = pkg;
          default = packages.wolog;
        };
      };
    };
}
