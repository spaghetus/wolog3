{
  config,
  pkgs,
  lib,
  ...
}: let
  stdenv = pkgs.stdenv;
  cfg = config.services.wolog;
  toml = pkgs.formats.toml {};
  path2derivation = path: pkgs.runCommand (builtins.toString path) {} ''cp -r ${path} $out'';
  inherit (lib) mkEnableOption mkPackageOption mkIf mkOption types optionalAttrs;
in {
  options.services.wolog = {
    enable = mkEnableOption "wolog";
    package = mkPackageOption pkgs "wolog" {};
    port = mkOption {
      type = types.port;
      default = 8000;
      description = ''
        Listen port for the wolog.
      '';
    };

    db-path = mkOption {
      type = types.str;
      default = "/var/lib/wolog.db";
      description = "Path to the Wolog's database.";
    };

    address = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = ''
        Listen address for the wolog.
      '';
    };

    articlesDir = mkOption {
      type = types.str;
      default = "/var/lib/wolog/posts";
      description = ''
        The directory where the wolog reads its posts.
      '';
    };

    templatesDir = mkOption {
      type = types.str;
      default = builtins.toString (path2derivation ./templates);
      description = ''
        The directory where the wolog reads its templates.
      '';
    };

    # fragmentsDir = mkOption {
    #   type = types.str;
    #   default = builtins.toString (path2derivation ./fragments);
    #   description = ''
    #     The directory where the wolog reads its fragments.
    #   '';
    # };

    staticDir = mkOption {
      type = types.path;
      default = builtins.toString (path2derivation ./static);
      description = ''
        The directory where the wolog reads its static files.
      '';
    };

    user = mkOption {
      type = types.str;
      default = "wolog";
      description = "User account under which the wolog runs.";
    };

    group = mkOption {
      type = types.str;
      default = "wolog";
      description = "Group account under which the wolog runs.";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open ports in the firewall for the wolog.
      '';
    };

    preview = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Allow rendering articles that aren't marked as ready.
      '';
    };

    settings = mkOption {
      type = toml.type;
      default = {};
      description = ''
        Rocket configuration for the wolog.
      '';
    };

    envFile = mkOption {
      type = types.str;
      default = builtins.toString (pkgs.writeText "default.env" "");
    };
  };

  config = let
    settings = toml.generate "Rocket.toml" ({
        release = {
          port = cfg.port;
          address = cfg.address;
        };
      }
      // cfg.settings);
    workdir = stdenv.mkDerivation {
      name = "wolog-workdir";
      unpackPhase = "true";
      installPhase = ''
        mkdir -p $out
        ln -s ${settings} $out/Rocket.toml
        ln -s ${cfg.articlesDir} $out/articles
        ln -s ${cfg.templatesDir} $out/templates
        ln -s ${cfg.staticDir} $out/static
      '';
    };
    wolog = cfg.package + /bin/wolog;
  in
    mkIf cfg.enable {
      systemd.services.wolog = {
        description = "Willow's blog engine";
        after = ["network.target"];
        wantedBy = ["multi-user.target"];

        path = [pkgs.pandoc];
        environment =
          {
            DATABASE_URL = "sqlite:${config.services.wolog.db-path}/wolog.db";
          }
          // mkIf cfg.preview {WOLOG_PREVIEW_NONREADY = "1";};

        serviceConfig = {
          Type = "simple";
          User = cfg.user;
          Group = cfg.group;

          EnvironmentFile = cfg.envFile;
          ExecStart = pkgs.writeScript "wolog-start" ''
            #!/bin/sh
            cd ${builtins.toString workdir}
            ${wolog}
          '';
          Restart = "always";
          BindReadOnlyPaths = "${cfg.articlesDir} ${cfg.templatesDir} ${cfg.staticDir} ${workdir}";
          BindReadWritePaths = "${config.services.wolog.db-path}";
          ProtectHome = "tmpfs";
        };
      };
      users.users = optionalAttrs (cfg.user == "wolog") {
        wolog = {
          group = cfg.group;
          isSystemUser = true;
        };
      };

      users.groups = optionalAttrs (cfg.group == "wolog") {
        wolog.members = [cfg.user];
      };
    };
}
