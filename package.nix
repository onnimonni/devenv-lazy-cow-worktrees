{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
}:

rustPlatform.buildRustPackage {
  pname = "lazy-cow-tree";
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

  # DEVENV_PROXY_BINARY takes one path: this name runs `lazy-cow-tree devenv-proxy`.
  postInstall = ''
    ln -s lazy-cow-tree "$out"/bin/lazy-cow-tree-devenv-proxy
  '';

  meta = {
    description = "Worktrees, databases, HTTPS hosts, Redis and LSP for parallel coding agents";
    mainProgram = "lazy-cow-tree";
    platforms = lib.platforms.unix;
  };
}
