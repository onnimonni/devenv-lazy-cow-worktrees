{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
}:

rustPlatform.buildRustPackage {
  pname = "localforest";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };
  # git-cow is a git dependency.
  cargoLock = {
    lockFile = ./Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  # Tests create git repositories and worktrees; `cargo test` in devenv runs them.
  doCheck = false;

  # DEVENV_PROXY_BINARY takes one path: this name runs `localforest devenv-proxy`.
  postInstall = ''
    ln -s localforest $out/bin/localforest-devenv-proxy
  '';

  meta = {
    description = "Worktrees, databases, HTTPS hosts, Redis and LSP for parallel coding agents";
    mainProgram = "localforest";
    platforms = lib.platforms.unix;
  };
}
