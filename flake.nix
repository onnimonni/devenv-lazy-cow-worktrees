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
        localforest = pkgs.callPackage ./package.nix { };
        default = localforest;
      });
      overlays.default = final: _prev: {
        localforest = final.callPackage ./package.nix { };
      };
      # `imports = [ inputs.localforest.devenvModules.default ];` or, with
      # `flake: false`, `imports: [ localforest/devenv-module ]` in devenv.yaml.
      devenvModules.default = ./devenv-module/devenv.nix;
    };
}
