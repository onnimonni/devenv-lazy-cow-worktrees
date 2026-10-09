{ pkgs, inputs, ... }:

let
  # statix's release binary (no compiling, no cachix); its script lints run the
  # shellcheck and ruff in PATH.
  statix = inputs.statix.packages.${pkgs.stdenv.system}.prebuilt;
in
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
    statix = {
      enable = true;
      package = statix;
      # Fix what can be fixed in the staged files, then fail on anything left or
      # changed, so the fixes get reviewed and staged. A staged script (.sh/.py) is
      # checked through the .nix files that refer to it.
      entry = toString (
        pkgs.writeShellScript "statix-hook" ''
          export PATH=${pkgs.lib.makeBinPath [ pkgs.shellcheck pkgs.ruff ]}:$PATH
          ${statix}/bin/statix fix --staged
          ${statix}/bin/statix check --staged
        ''
      );
      files = "\\.(nix|sh|bash|py)$";
    };
  };

  enterTest = ''
    cargo test
  '';
}
