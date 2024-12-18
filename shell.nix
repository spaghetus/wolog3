{pkgs ? import <nixpkgs> {}, ...}:
with pkgs; let
  deps = [
    sqlx-cli
    openssl
    cargo
    rustc
    clippy
    rust-analyzer
    pandoc
    pkg-config
    rustup
    postgresql
  ];
in
  mkShell {
    buildInputs = deps;
    LD_LIBRARY_PATH = lib.makeLibraryPath deps;
    shellHook = ''
      if [ ! -f ".db/postmaster.pid" ]; then
        pg_ctl -D .db -l db.log -o "--unix_socket_directories='$PWD'" start
      fi
    '';
  }
