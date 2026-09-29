{
  description = "Worktrees, databases, HTTPS hosts, Redis and LSP for parallel coding agents";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        lazy-cow-tree = pkgs.callPackage ./package.nix { };
        default = lazy-cow-tree;
      });
      overlays.default = final: _prev: {
        lazy-cow-tree = final.callPackage ./package.nix { };
      };
      # `imports = [ inputs.lazy-cow-tree.devenvModules.default ];` or, with
      # `flake: false`, `imports: [ lazy-cow-tree/devenv-module ]` in devenv.yaml. As a
      # flake input the package is this flake's (the one CI pushes to Cachix), so the
      # module doesn't evaluate a second nixpkgs.
      devenvModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./devenv-module/devenv.nix ];
          lazyCowTree.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
    };
}
