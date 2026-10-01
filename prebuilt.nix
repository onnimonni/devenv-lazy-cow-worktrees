# Release binaries from GitHub instead of a build. prebuilt.json (version and hashes) is
# updated by release.yml's PR after each release, so it can lag Cargo.toml.
{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  zlib,
}:

let
  release = lib.importJSON ./prebuilt.json;
  inherit (release) version;
  system = stdenv.hostPlatform.system;
  target =
    {
      aarch64-darwin = "aarch64-apple-darwin";
      aarch64-linux = "aarch64-unknown-linux-gnu";
      x86_64-linux = "x86_64-unknown-linux-gnu";
    }
    .${system} or (throw "lazy-cow-tree-prebuilt: no release binary for ${system}");
  name = "lazy-cow-tree-v${version}-${target}";
in
stdenv.mkDerivation {
  pname = "lazy-cow-tree-prebuilt";
  inherit version;

  src = fetchurl {
    url = "https://github.com/onnimonni/devenv-lazy-cow-worktrees/releases/download/v${version}/${name}.tar.gz";
    hash = release.hashes.${system};
  };
  sourceRoot = name;

  # Linux binaries link glibc, libgcc_s and zlib; OpenSSL is static.
  nativeBuildInputs = lib.optionals stdenv.hostPlatform.isLinux [ autoPatchelfHook ];
  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    stdenv.cc.cc.lib
    zlib
  ];

  installPhase = ''
    runHook preInstall
    install -Dm755 -t $out/bin lazy-cow-tree lazy-cow-tree-cow
    ln -s lazy-cow-tree $out/bin/lazy-cow-tree-devenv-proxy
    runHook postInstall
  '';

  meta = {
    description = "Worktrees, databases, HTTPS hosts, Redis and LSP for parallel coding agents (release binary)";
    homepage = "https://github.com/onnimonni/devenv-lazy-cow-worktrees";
    license = lib.licenses.mit;
    mainProgram = "lazy-cow-tree";
    platforms = builtins.attrNames release.hashes;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
