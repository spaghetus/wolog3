{
  inputs,
  lib,
  ...
}: {
  imports = [
    inputs.devshell.flakeModule
  ];

  config.perSystem = {pkgs, ...}: {
    config.devshells.default = {
      imports = [
        "${inputs.devshell}/extra/language/c.nix"
        "${inputs.devshell}/extra/language/rust.nix"
      ];

      devshell.packages = with pkgs; [
        pandoc
        postgresql
      ];

      commands = with pkgs; [
        {
          package = sqlx-cli;
        }
        {
          name = "database:init";
          command = ''
            initdb -D .db;
          '';
        }
      ];

      serviceGroups.database.services.postgres = {
        command = ''
          postgres -D .db -k "$PWD" -c listen_addresses="" > db.log
        '';
      };

      # language.c = {
      #   libraries = lib.optional pkgs.stdenv.isDarwin pkgs.libiconv;
      # };
    };
  };
}
