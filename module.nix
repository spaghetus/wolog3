{
  config,
  pkgs,
  lib,
  ...
}: let
  stdenv = pkgs.stdenv;
  cfg = config.services.wolog;
  toml = pkgs.formats.toml {};
  default_url = "postgres://localhost/wolog?host=/run/postgresql";
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

    db-url = mkOption {
      type = types.str;
      default = default_url;
      description = "URL of the Wolog's database.";
    };

    args = mkOption {
      type = types.listOf types.str;
      default = [];
    };

    enableWebmention = mkOption {
      type = types.bool;
      default = true;
      description = "Should we send and receive WebMentions?";
    };

    address = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = ''
        Listen address for the wolog.
      '';
    };

    origin = mkOption {
      type = types.str;
      default = "https://wolo.dev";
      description = "Origin to use for webmention";
    };

    articlesDir = mkOption {
      type = types.str;
      default = "/var/lib/wolog/post";
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

    assetsDir = mkOption {
      type = types.path;
      default = "/var/lib/wolog/post/assets";
      description = ''
        The directory where the wolog reads its post assets.
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

    development = mkOption {
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

    enableDynamic = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Give the Wolog the ability to run dynamic blocks as nobody via sudo
      '';
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
        environment = {
          DATABASE_URL = cfg.db-url;
          WOLOG_CONTENT_ROOT = cfg.articlesDir;
          WOLOG_STATIC_ROOT = cfg.staticDir;
          WOLOG_ASSETS_ROOT = cfg.assetsDir;
          WOLOG_TEMPLATES_ROOT = "${cfg.templatesDir}/*.tera";
          WOLOG_ORIGIN = cfg.origin;
        };

        serviceConfig = {
          Type = "simple";
          User = cfg.user;
          Group = cfg.group;

          EnvironmentFile = cfg.envFile;
          ExecStart = pkgs.writeScript "wolog-start" ''
            #!/bin/sh
            cd ${builtins.toString workdir}
            ${wolog} ${
              if cfg.development
              then "-d"
              else ""
            } ${
              if cfg.enableWebmention
              then "-W"
              else ""
            } ${builtins.concatStringsSep " " cfg.args}
          '';
          Restart = "always";
          BindReadOnlyPaths = "${cfg.articlesDir} ${cfg.assetsDir} ${cfg.templatesDir} ${cfg.staticDir} ${workdir}";
          PrivateTmp = true;
          ProtectHome = "tmpfs";
        };
      };
      users.users = optionalAttrs (cfg.user == "wolog") {
        wolog = {
          group = cfg.group;
          home = "/var/lib/wolog";
          isSystemUser = true;
        };
      };

      security.sudo.extraRules = optionalAttrs (cfg.enableDynamic) [
        {
          users = [cfg.user];
          runAs = "nobody:nogroup";
          commands = ["NOPASSWD:ALL"];
        }
      ];

      users.groups = optionalAttrs (cfg.group == "wolog") {
        wolog.members = [cfg.user];
      };

      services.postgresql = mkIf (cfg.user == "wolog" && cfg.db-url == default_url) {
        enable = true;
        ensureUsers = [
          {
            name = "wolog";
            ensureDBOwnership = true;
          }
        ];
        ensureDatabases = ["wolog"];
      };
    };
}
