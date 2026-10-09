# nix-darwin module: lazy-cow-tree stands in for devenv's `devenv-proxy` everywhere,
# with one CA trusted once, so `devenv up` stops asking to trust a new mkcert CA for
# every project (and every worktree) it sees.
#
#   # flake.nix of the nix-darwin configuration
#   inputs.lazy-cow-tree.url = "github:onnimonni/devenv-lazy-cow-worktrees";
#   darwinConfigurations.<host> = nix-darwin.lib.darwinSystem {
#     modules = [
#       inputs.lazy-cow-tree.darwinModules.default
#       { services.lazy-cow-tree.enable = true; }
#     ];
#   };
#
# - DEVENV_PROXY_BINARY: `devenv up` in a project with `process.proxy.enable` starts
#   lazy-cow-tree (or registers with the running daemon) instead of devenv-proxy.
# - TRUST_STORES=none: devenv's `mkcert -install` still creates each project's CA but
#   leaves the keychain alone, so no "System Certificate Trust Settings" prompt.
# - LAZY_COW_TREE_DEVENV_PROXY_CA=1: those projects' hostnames are served with
#   lazy-cow-tree's CA instead of their (now untrusted) mkcert certificates.
# - Activation runs `lazy-cow-tree trust` as the user: it asks for the password only
#   while the CA isn't trusted yet (first switch, or after the CA was replaced).
{
  config,
  lib,
  ...
}:
let
  cfg = config.services.lazy-cow-tree;
  env = {
    DEVENV_PROXY_BINARY = "${cfg.package}/bin/lazy-cow-tree-devenv-proxy";
    LAZY_COW_TREE_DEVENV_PROXY_CA = "1";
  }
  // lib.optionalAttrs (cfg.mkcertTrustStores != null) {
    TRUST_STORES = cfg.mkcertTrustStores;
  };
in
{
  options.services.lazy-cow-tree = {
    enable = lib.mkEnableOption "lazy-cow-tree as devenv's shared HTTPS proxy, with one trusted CA";
    package = lib.mkOption {
      type = lib.types.package;
      description = "lazy-cow-tree package (the flake's module sets this flake's default, the one on lazy-cow-tree.cachix.org).";
    };
    user = lib.mkOption {
      type = lib.types.str;
      default = config.system.primaryUser;
      defaultText = lib.literalExpression "config.system.primaryUser";
      description = "User whose login keychain trusts lazy-cow-tree's CA (~/.local/state/lazy-cow-tree/ca/ca.pem).";
    };
    trustCa = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run `lazy-cow-tree trust` on activation; it prompts only while the CA isn't trusted.";
    };
    mkcertTrustStores = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "none";
      example = "nss";
      description = "TRUST_STORES for every mkcert (devenv's too): \"none\" never touches the keychain; \"nss\" only Firefox's store (no prompt); null leaves mkcert's default (system keychain, prompts per new CA).";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    # Login shells (zsh, bash, fish) and what launchd starts: GUI terminals, editors.
    environment.variables = env;
    launchd.user.envVariables = env;

    system.activationScripts.postActivation.text = lib.mkIf cfg.trustCa ''
      echo "Trusting the lazy-cow-tree local CA for ${cfg.user}..." >&2
      launchctl asuser "$(id -u -- ${cfg.user})" sudo --user=${cfg.user} --set-home -- \
        ${cfg.package}/bin/lazy-cow-tree trust \
        || echo "Trusting the lazy-cow-tree local CA failed; run \`lazy-cow-tree trust\` yourself" >&2
    '';
  };
}
