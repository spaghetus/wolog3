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

    db-url = mkOption {
      type = types.str;
      default = "postgres://localhost/wolog";
      description = "URL of the Wolog's database.";
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
        environment =
          {
            DATABASE_URL = cfg.db-url;
            CONTENT_ROOT = cfg.articlesDir;
            STATIC_ROOT = cfg.staticDir;
            ASSETS_ROOT = cfg.assetsDir;
            TEMPLATES_ROOT = cfg.templatesDir;
            ORIGIN = cfg.origin;
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
            ${wolog} ${
              if cfg.development
              then "-d"
              else ""
            } ${
              if cfg.enableWebmention
              then "-W"
              else ""
            }
          '';
          Restart = "always";
          BindReadOnlyPaths = "${cfg.articlesDir} ${cfg.assetsDir} ${cfg.templatesDir} ${cfg.staticDir} ${workdir}";
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

      users.groups = optionalAttrs (cfg.group == "wolog") {
        wolog.members = [cfg.user];
      };

      services.postgresql = mkIf (cfg.user == "wolog" && cfg.db-url == "postgres://localhost/wolog") {
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
