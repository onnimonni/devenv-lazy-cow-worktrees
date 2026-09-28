{ pkgs, ... }:

{
  packages = [
    pkgs.git
    pkgs.pkg-config
    pkgs.openssl
    pkgs.libiconv
    # The daemon runs `postgres`/`initdb` and `redis-server` from PATH.
    pkgs.postgresql_18
    pkgs.redis
  ];

  languages.rust.enable = true;

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy.enable = true;
  };

  enterTest = ''
    cargo test
  '';
}
