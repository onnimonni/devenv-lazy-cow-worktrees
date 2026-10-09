# devenv module: runs `lazy-cow-tree serve` for this project and wires every checkout to it.
#
#   # devenv.yaml                              # devenv.nix
#   inputs:                                    { inputs, ... }: {
#     lazy-cow-tree:                               imports = [ inputs.lazy-cow-tree.devenvModules.default ];
#       url: github:onnimonni/devenv-lazy-cow-worktrees      }
#
#   (or `flake: false` on the input and `imports: [ lazy-cow-tree/devenv-module ]`)
#
# It derives lazy-cow-tree's services from devenv's own `processes` (exec, cwd, ports,
# proxy.hostname, after, ready, restart, watch, plus the new `start.on` /
# `start.idleTimeout`), its database from `services.postgres` (plus `instance`,
# `start`, `copyOnWrite`, `dangerouslyDisableDurabilityForSpeed`), Redis from
# `services.redis` (plus `instance`, `start`), and keeps devenv from starting its own copies. `lazyCowTree.*` options still work and
# win over what is derived:
#
#   lazyCowTree.migrate = "mix ecto.migrate";
#   lazyCowTree.seed = "mix run priv/repo/seeds.exs";
#   lazyCowTree.setup = "mix deps.get";           # once in every new checkout
#   lazyCowTree.services.api = { exec = "bun run dev"; cwd = "api"; };
#   languages.elixir.lsp.package = pkgs.dexter;  # per-worktree LSP for Claude Code and Codex
#
# Every checkout (primary and worktrees) gets each service on demand:
# https://<worktree>.<service>.<project>.localhost (https://<service>.<project>.localhost
# in the primary) starts it, after what it depends on, with its own PORT and the
# checkout's DATABASE_URL and REDIS_URL. Every bash/zsh started from the devenv shell
# (agents' tool shells) gets the same environment for the checkout it runs in, again
# after cd. Claude Code's WorktreeCreate/WorktreeRemove
# hooks go through the daemon, so `claude --worktree` and `isolation: worktree`
# subagents get provisioned worktrees too, and its NODE_EXTRA_CA_CERTS trusts the
# local CA (MCP servers on https://*.localhost).
{
  pkgs,
  lib,
  config,
  options,
  inputs ? { },
  ...
}:

let
  cfg = config.lazyCowTree;
  exe = lib.getExe cfg.package;
  # lazy-cow-tree's own nixpkgs (flake.lock), so the default package is the exact
  # derivation CI pushes to lazy-cow-tree.cachix.org, whatever nixpkgs the consumer uses.
  lock = builtins.fromJSON (builtins.readFile ../flake.lock);
  nixpkgsLock = lock.nodes.${lock.nodes.${lock.root}.inputs.nixpkgs}.locked;
  pinnedPkgs =
    import
      (builtins.fetchTarball {
        url = "https://github.com/${nixpkgsLock.owner}/${nixpkgsLock.repo}/archive/${nixpkgsLock.rev}.tar.gz";
        sha256 = nixpkgsLock.narHash;
      })
      {
        system = pkgs.stdenv.hostPlatform.system;
        config = { };
        overlays = [ ];
      };
  # devenv evaluates impurely, so the invoking user's environment is readable.
  envHome = builtins.getEnv "LAZY_COW_TREE_HOME";
  userHome = builtins.getEnv "HOME";
  inherit (lib) mkOption types;
  # macOS: keeps the worktree process marker (`git worktree remove` lists what a shell in
  # a worktree started) open in what node, bun, python and erlang start; see
  # process-marker.c. SIP's /bin/sh, /bin/bash, /bin/zsh and /usr/bin/env drop DYLD_*:
  # every shell hook puts it back (WORKTREE_PROCESS_SHIM keeps the path).
  shim = cfg.processMarkerShim.enable && pkgs.stdenv.hostPlatform.isDarwin;
  processMarkerShim = pkgs.runCommandCC "lazy-cow-tree-process-marker" { } ''
    mkdir -p "$out"/lib
    # Universal: arm64e (Apple's own binaries, when SIP is off they honor DYLD_*) and
    # x86_64 (Rosetta) processes would be killed by an arm64-only library.
    $CC -dynamiclib -O2 -arch arm64 -arch arm64e -arch x86_64 \
      -DSH_PATH='"${pkgs.bash}/bin/sh"' -DENV_PATH='"${pkgs.coreutils}/bin/env"' \
      -o "$out"/lib/liblazy-cow-tree-process-marker.dylib ${./process-marker.c}
  '';
  # dyld kills every process whose DYLD_INSERT_LIBRARIES names a missing library: only
  # while it is there (a garbage-collected store path is dropped from the list instead).
  shimEnv = lib.optionalString shim ''
    export WORKTREE_PROCESS_SHIM=${processMarkerShim}/lib/liblazy-cow-tree-process-marker.dylib
    if [ -r "$WORKTREE_PROCESS_SHIM" ]; then
      case ":''${DYLD_INSERT_LIBRARIES:-}:" in
        *":$WORKTREE_PROCESS_SHIM:"*) ;;
        *) export DYLD_INSERT_LIBRARIES="$WORKTREE_PROCESS_SHIM''${DYLD_INSERT_LIBRARIES:+:$DYLD_INSERT_LIBRARIES}" ;;
      esac
    else
      __lct_dyld=":''${DYLD_INSERT_LIBRARIES:-}:"
      __lct_dyld=''${__lct_dyld//:$WORKTREE_PROCESS_SHIM:/:}
      __lct_dyld=''${__lct_dyld#:}
      __lct_dyld=''${__lct_dyld%:}
      if [ -n "$__lct_dyld" ]; then export DYLD_INSERT_LIBRARIES=$__lct_dyld; else unset DYLD_INSERT_LIBRARIES; fi
      unset __lct_dyld
    fi
  '';
  # Sourced by every bash (BASH_ENV) and zsh (ZDOTDIR) started from the devenv shell, so
  # agents' tool shells (Claude Code: bash -c, Codex: zsh -lc) get the checkout they run
  # in: its env again after each cd/pushd/popd (`cd .claude/worktrees/x && cmd`).
  # Shell functions aren't inherited, env vars are.
  shellHook = pkgs.writeText "lazy-cow-tree-shell-hook.sh" ''
    ${shimEnv}
    # Never a lazy-cow-tree hook (an older build's would source itself forever).
    if [ -n "''${BASH_VERSION:-}" ]; then
      case "''${LAZY_COW_TREE_BASH_ENV:-}" in
        "" | *-lazy-cow-tree-shell-hook.sh) ;;
        *) . "$LAZY_COW_TREE_BASH_ENV" ;;
      esac
    fi
    __lazy_cow_tree_env() { eval "$(command ${exe} shell-hook 2>/dev/null)"; }
    cd() { builtin cd "$@" && __lazy_cow_tree_env; }
    pushd() { builtin pushd "$@" && __lazy_cow_tree_env; }
    popd() { builtin popd "$@" && __lazy_cow_tree_env; }
    __lazy_cow_tree_env
    ${lib.optionalString cfg.protectPrimary.enable ". ${protectPrimaryGuard}"}
  '';
  # lazyCowTree.protectPrimary's command allowlist, for the shell hook.
  protectPrimaryGuard = pkgs.writeText "lazy-cow-tree-protect-primary.sh" (
    builtins.replaceStrings
      [ "@root@" "@allowed@" "@worktreesDir@" ]
      [
        (lib.escapeShellArg config.devenv.root)
        (lib.escapeShellArgs cfg.protectPrimary.allow)
        cfg.worktreesDir
      ]
      (builtins.readFile ./protect-primary.sh)
  );
  # Every zsh reads $ZDOTDIR/.zshenv. Codex's shell snapshot replays the ZDOTDIR it
  # captured, so it stays this dir: each startup file runs the user's own one (from
  # their ZDOTDIR, or $HOME) and notes where a file of theirs moved ZDOTDIR to.
  zdotdir = pkgs.runCommand "lazy-cow-tree-zdotdir" { } ''
    mkdir "$out"
    echo ". ${shellHook}" > "$out"/.zshenv
    for f in .zshenv .zprofile .zshrc .zlogin .zlogout; do
      cat >> "$out"/$f <<EOF
    ZDOTDIR=\''${LAZY_COW_TREE_ZDOTDIR:-\$HOME}
    case \$ZDOTDIR in *-lazy-cow-tree-zdotdir) ZDOTDIR=\$HOME ;; esac
    [ -f "\$ZDOTDIR/$f" ] && . "\$ZDOTDIR/$f"
    LAZY_COW_TREE_ZDOTDIR=\$ZDOTDIR ZDOTDIR=$out
    EOF
    done
  '';
  # hiPrio in packages: wins over bin/git of git itself (still there for git-upload-pack etc.).
  gitWrapper = pkgs.writeShellScriptBin "git" (
    builtins.replaceStrings
      [ "@git@" "@cow@" "@gh@" "@lazyCowTree@" ]
      [
        (lib.getExe cfg.git.package)
        "${cfg.package}/bin/lazy-cow-tree-cow"
        (lib.getExe cfg.gh.package)
        exe
      ]
      (builtins.readFile ./git.sh)
  );
  # hiPrio in packages: wins over cfg.gh.package's bin/gh.
  ghWrapper = pkgs.writeShellScriptBin "gh" (
    builtins.replaceStrings
      [ "@gh@" "@git@" "@lazyCowTree@" ]
      [
        (lib.getExe cfg.gh.package)
        (lib.getExe cfg.git.package)
        exe
      ]
      (builtins.readFile ./gh.sh)
  );
  # `<binary>` of a supported language server (`lspServers`): starts as a language server behind
  # `lazy-cow-tree lsp`; hiPrio in packages, over a real one in the shell.
  lspWrapper =
    cmd:
    let
      bin = lib.head cmd;
      name = baseNameOf bin;
    in
    pkgs.writeShellScriptBin name (
      builtins.replaceStrings
        [ "@name@" "@real@" "@lazyCowTree@" "@lspArgs@" ]
        [
          name
          (if lib.hasPrefix "/" bin then lib.escapeShellArg bin else "")
          exe
          (lib.escapeShellArgs (lib.tail cmd))
        ]
        (builtins.readFile ./lsp.sh)
    );
  # Language servers of the enabled `languages.*` whose LSP is on, by binary (the
  # `lsp.package`'s main program; javascript and typescript share one).
  # FIXME: only dexter (`languages.elixir.lsp.package = pkgs.dexter`),
  # typescript-language-server, pyright and rust-analyzer for now. devenv's `languages.*.lsp` has only `enable`
  # and `package`, not the arguments a server starts with nor its file extensions
  # (cachix/devenv#3202); Helix's languages.toml has both for most servers.
  supportedLsp = {
    dexter = {
      args = [ "lsp" ];
      extensions = {
        ".ex" = "elixir";
        ".exs" = "elixir";
        ".heex" = "phoenix-heex";
      };
    };
    typescript-language-server = {
      args = [ "--stdio" ];
      extensions = {
        ".js" = "javascript";
        ".mjs" = "javascript";
        ".cjs" = "javascript";
        ".jsx" = "javascriptreact";
        ".ts" = "typescript";
        ".mts" = "typescript";
        ".cts" = "typescript";
        ".tsx" = "typescriptreact";
      };
    };
    pyright-langserver = {
      args = [ "--stdio" ];
      extensions = {
        ".py" = "python";
        ".pyi" = "python";
      };
    };
    rust-analyzer = {
      args = [ ];
      extensions.".rs" = "rust";
    };
  };
  # The server where a package's main program isn't it: pyright's is its CLI, and
  # languages.rust.lsp.package is often a whole toolchain (rust-overlay, fenix).
  lspBinary = {
    main = { pyright = "pyright-langserver"; };
    language = { rust = "rust-analyzer"; };
  };
  lspServers = lib.concatMapAttrs (
    lang: l:
    let
      pkg =
        if (l.enable or false) && l ? lsp && (l.lsp.enable or false) then l.lsp.package or null else null;
      # A name, not a path: attribute names can't refer to the store.
      main = builtins.unsafeDiscardStringContext (baseNameOf (lib.getExe pkg));
      bin =
        if pkg == null then null else lspBinary.language.${lang} or lspBinary.main.${main} or main;
    in
    lib.optionalAttrs (bin != null && supportedLsp ? ${bin}) {
      ${bin} = supportedLsp.${bin} // {
        cmd = [ "${pkg}/bin/${bin}" ] ++ supportedLsp.${bin}.args;
      };
    }
  ) config.languages;
  lspWrapperPath = s: "${lspWrapper s.cmd}/bin/${baseNameOf (lib.head s.cmd)}";
  # Each language server as an MCP server `lsp-<binary>` for agents without an LSP client
  # (Codex, pi): mcp-language-server on its wrapper, given by path (Codex passes MCP
  # servers only PATH, HOME and a few more variables).
  lspMcpServers = lib.mapAttrs' (
    name: s:
    lib.nameValuePair "lsp-${name}" {
      command = lib.getExe cfg.codex.mcpLanguageServer;
      args = [
        "--workspace"
        config.devenv.root
        "--lsp"
        (lspWrapperPath s)
        "--"
      ]
      ++ s.args;
    }
  ) lspServers;
  # Claude Code runs language servers from plugins only: a local marketplace with one
  # plugin holding them, each through its wrapper.
  claudeLsp = cfg.claude.lsp && lspServers != { };
  claudeMarketplace = pkgs.linkFarm "lazy-cow-tree-claude-marketplace" [
    {
      name = ".claude-plugin/marketplace.json";
      path = pkgs.writeText "marketplace.json" (
        builtins.toJSON {
          name = "lazy-cow-tree";
          owner.name = "lazy-cow-tree";
          plugins = [
            {
              name = "lazy-cow-tree-lsp";
              source = "./lsp";
              description = "The devenv shell's language servers, one per worktree";
            }
          ];
        }
      );
    }
    {
      name = "lsp/.claude-plugin/plugin.json";
      path = pkgs.writeText "plugin.json" (
        builtins.toJSON {
          name = "lazy-cow-tree-lsp";
          description = "The devenv shell's language servers behind `lazy-cow-tree lsp` (one per worktree)";
          lspServers = lib.mapAttrs (_: s: {
            command = lspWrapperPath s;
            inherit (s) args;
            extensionToLanguage = s.extensions;
          }) lspServers;
        }
      );
    }
  ];
  # With its extensions, like devenv's services.postgres.
  postgres =
    if cfg.postgres.extensions != null then
      cfg.postgres.package.withPackages cfg.postgres.extensions
    else
      cfg.postgres.package;

  extraPort = types.submodule {
    options = {
      env = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "LIVE_DEBUGGER_PORT";
        description = "Variable holding the port (default: `<NAME>_PORT`); also always LAZY_COW_TREE_<SERVICE>_<NAME>_PORT (and _URL with http).";
      };
      http = mkOption {
        type = types.bool;
        default = false;
        description = "Served at https://<worktree>.<name>.<project>.localhost, whose first request starts the service.";
      };
      offset = mkOption {
        type = types.nullOr (types.ints.between 0 9);
        default = null;
        description = "Port = the checkout's base port + this (default: the highest free offset, 9 down).";
      };
    };
  };

  # "30s" / "15m" / "1h" / 90 -> seconds; null stays null.
  seconds =
    d:
    if d == null || builtins.isInt d then
      d
    else
      let
        m = builtins.match "([0-9]+)(s|m|h)?" d;
        n = lib.toInt (builtins.elemAt m 0);
        unit = builtins.elemAt m 1;
      in
      if m == null then
        throw "lazyCowTree: duration ${builtins.toJSON d} is not like 30s, 15m or 1h"
      else if unit == "h" then
        n * 3600
      else if unit == "m" then
        n * 60
      else
        n;
  # "4G" / "512M" / 4096 (MB) -> MB.
  megabytes =
    s:
    if builtins.isInt s then
      s
    else
      let
        m = builtins.match "([0-9]+)([MmGg])?[Bb]?" s;
        n = lib.toInt (builtins.elemAt m 0);
      in
      if m == null then
        throw "lazyCowTree: size ${builtins.toJSON s} is not like 512M or 4G"
      else if lib.toLower (toString (builtins.elemAt m 1)) == "g" then
        n * 1024
      else
        n;
  duration = types.nullOr (types.either types.ints.unsigned types.str);

  # devenv's processes that lazy-cow-tree runs instead: every one but its own, devenv's
  # postgres/redis (lazy-cow-tree provides those) and ones opted out.
  own = [
    "lazy-cow-tree"
    "postgres"
    "redis"
  ];
  derivedProcs = lib.filterAttrs (
    name: p: cfg.enable && !(builtins.elem name own) && p.lazyCowTree.enable
  ) config.processes;
  root = config.devenv.root;
  # A path under the checkout root as relative to `base` (itself relative to root, or
  # null for root); null when outside.
  relativeTo =
    base: path:
    let
      abs = toString path;
      prefix = if base == null then "${root}/" else "${root}/${base}/";
    in
    if lib.hasPrefix prefix abs then lib.removePrefix prefix abs else null;
  processDep =
    entry:
    let
      m = builtins.match "devenv:processes:([^@]+)(@.*)?" entry;
    in
    if m == null then null else builtins.head m;
  deriveService =
    name: p:
    let
      portNames = lib.attrNames p.ports;
      httpPort =
        if p.ports ? http then
          "http"
        else if builtins.length portNames == 1 then
          builtins.head portNames
        else
          null;
      value = port: toString p.ports.${port}.value;
      envFor =
        port:
        let
          matching = lib.attrNames (lib.filterAttrs (_: v: v == value port) p.env);
        in
        if matching != [ ] then
          builtins.head matching
        else
          "${lib.toUpper (lib.replaceStrings [ "-" ] [ "_" ] port)}_PORT";
      extraPorts = lib.filter (port: port != httpPort) portNames;
      # The http port's own variable (e.g. `env.WEB_PORT = toString ports.http.value`),
      # set per checkout like PORT, in its shells too. `env.PORT = ...` is no alias:
      # lazy-cow-tree sets PORT itself.
      httpAliases =
        if httpPort == null then
          [ ]
        else
          lib.attrNames (
            lib.filterAttrs (
              n: v: v == value httpPort && n != "PORT" && !lib.hasPrefix "LAZY_COW_TREE_" n
            ) p.env
          );
      # lazy-cow-tree sets PORT and each named port's variable per checkout.
      portValues = map value portNames;
      env = lib.filterAttrs (_: v: !(builtins.elem v portValues)) p.env;
      cwd =
        if p.cwd == null || p.cwd == root then
          null
        else if lib.hasPrefix "${root}/" p.cwd then
          lib.removePrefix "${root}/" p.cwd
        else
          p.cwd;
      deps = lib.filter (d: d != null) (map processDep p.after);
      others = lib.filter (d: !(builtins.elem d own)) deps;
      ignored = lib.filter (
        e:
        let
          d = processDep e;
        in
        d == null || !(builtins.elem d own || derivedProcs ? ${d})
      ) p.after;
      watched = lib.filter (x: x != null) (map (relativeTo cwd) p.watch.paths);
    in
    lib.warnIf (ignored != [ ])
      "processes.${name}.after: ${lib.concatStringsSep ", " ignored} ignored (only other processes lazy-cow-tree runs are started first)"
      {
        exec = toString (pkgs.writeShellScript "lazy-cow-tree-${name}" p.exec);
        portEnv = if httpAliases == [ ] then null else builtins.head httpAliases;
        inherit cwd env;
        http = httpPort != null;
        ports = lib.genAttrs extraPorts (port: {
          env = envFor port;
          http = p.ports.${port}.proxy.hostname != null;
        });
        dependsOn = lib.filter (d: derivedProcs ? ${d}) others;
        restart =
          {
            never = "no";
            on_failure = "on-failure";
            always = "always";
          }
          .${p.restart.on};
        restartOnChange = if watched == [ ] then null else watched;
        ready =
          if p.ready != null && p.ready.http.get != null then
            {
              inherit (p.ready.http.get) path;
              timeout = if p.ready.timeout != null then p.ready.timeout else 60;
            }
          else
            null;
        start =
          if p.start.on != null then
            p.start.on
          else if httpPort != null || dependedOn name then
            "demand"
          else
            "up";
        idleTimeout = seconds p.start.idleTimeout;
        hostname = p.proxy.hostname;
        inherit (p.lazyCowTree) migrate restartOnPull;
      };

  # Another process lists it in `after`: that process's start starts it.
  dependedOn =
    name:
    lib.any (other: builtins.elem name (lib.filter (d: d != null) (map processDep other.after))) (
      lib.attrValues (removeAttrs derivedProcs [ name ])
    );

  derived = lib.mapAttrs deriveService derivedProcs;

  # The new per-process options, merged into devenv's `processes.<name>` submodule.
  processExtension = types.submodule (
    { name, config, ... }:
    {
      options = {
        start = mkOption {
          type = types.submodule {
            options = {
              on = mkOption {
                type = types.nullOr (
                  types.enum [
                    "up"
                    "demand"
                    "manual"
                  ]
                );
                default = null;
                defaultText = lib.literalMD "`demand` when a request or another process can start it (an http port, or another process's `after`), else `up`, as `devenv up` would";
                description = "When lazy-cow-tree starts it: with its checkout (`up`), on the first request or as a dependency (`demand`), or only by `lazy-cow-tree service start` (`manual`).";
              };
              idleTimeout = mkOption {
                type = duration;
                default = null;
                example = "15m";
                description = "Stop it after this long without open connections (30s, 15m, 1h or seconds); the next request starts it again. null: never.";
              };
            };
          };
        };
        lazyCowTree = {
          enable = mkOption {
            type = types.bool;
            default = true;
            description = "Run this process in every checkout through lazy-cow-tree; false leaves it to devenv (primary checkout only).";
          };
          migrate = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Migrate command for the checkout's database, run in the process's cwd after `lazyCowTree.migrate`.";
          };
          restartOnPull = mkOption {
            type = types.bool;
            default = false;
            description = "Restart it (if running) after the base branch was pulled into its checkout and migrations ran.";
          };
        };
      };
      # lazy-cow-tree runs it (and provides postgres/redis): `devenv up` doesn't too.
      config.start.enable = lib.mkIf (
        cfg.enable
        && (
          (builtins.elem name own && name != "lazy-cow-tree")
          || (!(builtins.elem name own) && config.lazyCowTree.enable)
        )
      ) (lib.mkForce false);
    }
  );
  startOptions = defaultOn: {
    on = mkOption {
      type = types.enum [
        "up"
        "demand"
      ];
      default = defaultOn;
      description = "Start with the daemon (`up`) or on the first connection (`demand`).";
    };
    idleTimeout = mkOption {
      type = duration;
      default = null;
      example = "30m";
      description = "Stop after this long without connections (30s, 15m, 1h or seconds); null: never.";
    };
  };
  pgCfg = config.services.postgres;
  redisCfg = config.services.redis;
  # devenv's postgres module sets these itself; lazy-cow-tree's cluster decides them.
  pgOwnSettings = [
    "listen_addresses"
    "port"
    "unix_socket_directories"
  ];

  service = types.submodule {
    options = {
      exec = mkOption {
        type = types.str;
        example = "mix phx.server";
        description = "Command, split like a shell would and run directly (no shell) with the service's environment.";
      };
      cwd = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "api";
        description = "Working directory relative to the checkout.";
      };
      http = mkOption {
        type = types.bool;
        default = true;
        description = "Listens on $PORT and gets https://<worktree>.<service>.<project>.localhost, whose first request starts it. Without http it only runs as a dependency of others (workers).";
      };
      default = mkOption {
        type = types.bool;
        default = false;
        description = "The service whose PORT the shells get and `lazy-cow-tree service` picks without a name (default: `web`, else the first http service).";
      };
      portOffset = mkOption {
        type = types.nullOr (types.ints.between 0 9);
        default = null;
        description = "PORT = the checkout's base port + this (default: position by name).";
      };
      portEnv = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "WEB_PORT";
        description = "A variable of its own holding its port, besides PORT, in every environment of the checkout (shells too). Derived from a process's `env.<NAME> = toString ports.http.value`.";
      };
      # FIXME: all services of a checkout share its DATABASE_URL and REDIS_URL. Add
      # `postgres` / `redis` options for a database / redis-server of their own.
      migrate = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Migrate/seed command for the checkout's database, run in its cwd after `lazyCowTree.migrate` when the base branch moves (primary) or was merged in (worktrees).";
      };
      dependsOn = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Services started before this one.";
      };
      env = mkOption {
        type = types.attrsOf types.str;
        default = { };
        description = "Extra environment.";
      };
      restart = mkOption {
        type = types.enum [
          "no"
          "on-failure"
          "always"
        ];
        default = "no";
        description = "When it exits on its own: leave it down, start it again after a non-zero exit, or after any exit (backing off 1-30 s; `lazy-cow-tree service stop` keeps it down).";
      };
      ports = mkOption {
        type = types.attrsOf extraPort;
        default = { };
        example = {
          debugger = {
            env = "LIVE_DEBUGGER_PORT";
            http = true;
          };
          test.env = "TEST_PORT";
        };
        description = "Further ports the service listens on, from the checkout's 10-port block, in every environment of the checkout.";
      };
      restartOnPull = mkOption {
        type = types.bool;
        default = false;
        description = "Restart it (if running) after the base branch was pulled into its checkout and the migrations ran; for servers without a code reloader.";
      };
      restartOnChange = mkOption {
        type = types.nullOr (types.listOf types.str);
        default = null;
        example = [
          "Gemfile.lock"
          "config/*.rb"
        ];
        description = "Files (relative to cwd, `*` / `?` in the file name) whose content changing restarts it if running (after lazyCowTree.setup when a dependency manifest or lockfile such as mix.lock, Gemfile.lock or package.json changed; not while its checkout pulls or migrates). Default: for a command running `mix`, mix.exs, mix.lock and config/*.exs (Phoenix's code reloader refuses to compile after those change); `[ ]` for others and to turn it off.";
      };
      start = mkOption {
        type = types.enum [
          "up"
          "demand"
          "manual"
        ];
        default = "demand";
        description = "Start it with its checkout (`up`), on the first request or as a dependency (`demand`), or only with `lazy-cow-tree service start` (`manual`).";
      };
      idleTimeout = mkOption {
        type = types.nullOr types.ints.unsigned;
        default = null;
        description = "Stop it after this many seconds without open connections; null: never.";
      };
      ready = mkOption {
        type = types.nullOr (
          types.submodule {
            options = {
              path = mkOption {
                type = types.str;
                default = "/";
              };
              timeout = mkOption {
                type = types.ints.unsigned;
                default = 60;
              };
            };
          }
        );
        default = null;
        description = "HTTP probe on its PORT (ready on 200-399) a first request waits for, up to `timeout` seconds; null: the port listening.";
      };
      hostname = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "web.myapp.localhost";
        description = "Hostname in the primary checkout instead of <service>.<project>.localhost, under .localhost or `lazyCowTree.tls.domain`; worktrees prefix `<worktree>.`.";
      };
    };
  };
in
{
  # Merged into devenv's own options: `processes.<name>.start.{on,idleTimeout}`,
  # `processes.<name>.lazyCowTree.*`, and new `services.postgres` / `services.redis`
  # settings. lazy-cow-tree derives its services and database settings from them.
  options = {
    processes = mkOption { type = types.attrsOf processExtension; };

    services.postgres = {
      instance = mkOption {
        type = types.enum [
          "shared"
          "unique"
        ];
        default = "shared";
        description = "One cluster with databases per checkout (`shared`, needed for copyOnWrite), or a cluster of its own per checkout (`unique`).";
      };
      start = startOptions "up";
      copyOnWrite = {
        enable = mkOption {
          type = types.bool;
          default = pgCfg.instance == "shared";
          defaultText = lib.literalExpression ''services.postgres.instance == "shared"'';
          description = "Worktree databases are copy-on-write clones of a template (milliseconds, near-zero disk); false: created empty, then migrated and seeded.";
        };
        template = mkOption {
          type = types.enum [ "primary" ];
          default = "primary";
          description = "What worktree databases are cloned from: the primary checkout's databases.";
        };
        refresh = mkOption {
          type = types.enum [
            "on-base-change"
            "manual"
          ];
          default = "on-base-change";
          description = "Refresh the template from the primary when the base branch moves (after migrations), or only with `lazy-cow-tree snapshot`.";
        };
      };
      dangerouslyDisableDurabilityForSpeed = {
        enable = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Run PostgreSQL on a RAM disk with fsync, synchronous_commit and
            full_page_writes off. Every database is lost on reboot, on a crash (which
            can also corrupt the cluster) and on `lazy-cow-tree down --eject`. For
            disposable development and test data only.
          '';
        };
        ramdiskSize = mkOption {
          type = types.either types.ints.positive types.str;
          default = "4G";
          example = "8G";
          description = "RAM disk size (512M, 4G, or MB); memory is only used as it fills.";
        };
      };
    };

    services.redis = {
      instance = mkOption {
        type = types.enum [
          "unique"
          "shared"
        ];
        default = "unique";
        description = "A redis-server of its own per checkout (`unique`), or one for every checkout of the project (`shared`).";
      };
      start = startOptions "demand";
    };

    lazyCowTree = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Run and use the lazy-cow-tree daemon.";
      };
      package = mkOption {
        type = types.package;
        default = pinnedPkgs.callPackage ../package.nix { };
        defaultText = lib.literalMD "built with lazy-cow-tree's pinned nixpkgs (flake.lock), substituted from lazy-cow-tree.cachix.org";
        example = lib.literalExpression "pkgs.callPackage (inputs.lazy-cow-tree + \"/package.nix\") { }";
        description = "The lazy-cow-tree package. The default is the one lazy-cow-tree's CI builds and pushes to lazy-cow-tree.cachix.org; a package built with other nixpkgs is compiled locally.";
      };
      cachix.enable = mkOption {
        type = types.bool;
        default = true;
        description = "Pull the default package from lazy-cow-tree.cachix.org (`cachix.pull`). A multi-user Nix only uses it for trusted users, or when it's in the daemon's own substituters.";
      };
      # Daemon-wide, like the ports: the project whose `devenv up` starts the daemon
      # decides; the others share its PostgreSQL.
      postgres = {
        package = mkOption {
          type = types.package;
          default = pkgs.postgresql_18;
          defaultText = lib.literalExpression "pkgs.postgresql_18";
          description = "PostgreSQL the daemon runs (18+ for copy-on-write CREATE DATABASE).";
        };
        extensions = mkOption {
          type = types.nullOr (types.functionTo (types.listOf types.package));
          default = null;
          example = lib.literalExpression "extensions: [ extensions.postgis extensions.pgvector ]";
          description = "Extensions to install, as in devenv's services.postgres.extensions (`package.withPackages`). Checkout roles aren't superusers: they can CREATE EXTENSION trusted ones (pgcrypto, citext, ...); list the others in createExtensions.";
        };
        databases = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "cms" ];
          description = "More databases every checkout gets next to its main one, e.g. for a second Ecto repo: `<project>_<name>_dev`, `_test` and test partitions per checkout (worktrees' cloned from `<project>_<name>_template`, refreshed with the main template), in `<NAME>_DATABASE_URL` and `<NAME>_TEST_DATABASE_URL`.";
        };
        createExtensions = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [
            "postgis"
            "vector"
          ];
          description = "Extensions the daemon creates as superuser in template1 (so in every database made afterwards) and the primaries' databases: the untrusted ones checkout roles can't create. Migrations' CREATE EXTENSION IF NOT EXISTS is then a no-op.";
        };
        settings = mkOption {
          type = types.attrsOf (
            types.oneOf [
              types.bool
              types.int
              types.str
            ]
          );
          default = { };
          example = {
            shared_preload_libraries = "pg_stat_statements";
            log_min_duration_statement = 250;
          };
          description = "Extra postgresql.conf settings (passed as `-c name=value`), as in devenv's services.postgres.settings.";
        };
        ramdiskMB = mkOption {
          type = types.ints.positive;
          default = 4096;
          description = "Size of the APFS RAM disk PostgreSQL runs on, in MB (memory is only used as it fills). The daemon that creates the RAM disk sizes it; `lazy-cow-tree down --eject` and a restart apply a new size (and empty every database).";
        };
      };
      redis = mkOption {
        type = types.package;
        default = pkgs.redis;
        description = "Redis the daemon runs, one redis-server per checkout.";
      };
      project = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Project name (hostnames, database prefix); default: the checkout's directory name.";
      };
      tls = {
        domain = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "dev.example.com";
          description = "Domain whose `*` A record points at 127.0.0.1: hostnames become https://<worktree>.<service>.<project>.<domain> with the Let's Encrypt certificate github.com/onnimonni/trusted-https-certificate-to-artifacts-action keeps as the GitHub repository's https-certificate Actions artifact (read only while the repository is private), instead of .localhost ones with the local CA. Until it has one, the local CA serves them. `lazy-cow-tree cert show` prints the names it needs.";
        };
        githubRepository = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "my-org/dev-certificates";
          description = "GitHub repository (`owner/repo`, `host/owner/repo` or a URL) whose https-certificate artifact has the domain's certificates, when it isn't the checkout's own remote (e.g. one private repository issuing them for several projects). Same rules: read with gh's token, only while it's private.";
        };
        services = mkOption {
          type = types.nullOr (types.listOf types.str);
          default = null;
          example = [ "web" "api" ];
          description = "Service names the certificate covers (`<name>.<project>.<domain>` and its worktrees') besides the project's own http services, so adding one needs no new certificate. Default: common ones (app, web, www, api, backend, frontend, admin, dashboard, auth, docs, storybook, simulator, mobile, cms, mail, assets, vite, ws).";
        };
      };
      port = mkOption {
        type = types.port;
        default = 4000;
        description = "Base port of the primary checkout's services (worktrees get 20000-28990 by hash); each service adds its offset.";
      };
      migrate = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "mix do ecto.migrate + run priv/repo/seeds.exs";
        description = "Migrate command for the checkout's database: run in the primary checkout whenever the base branch moves (then the template database is refreshed from it) and in every worktree the base branch was merged into.";
      };
      seed = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "mix run priv/repo/seeds.exs";
        description = "Seed command: run in the primary checkout after `migrate` when its database was just created (a fresh RAM disk); worktrees get the seeded data through the template.";
      };
      setup = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "mix deps.get";
        description = "Setup command run once in every new checkout (made by lazy-cow-tree, git, git-cow or Claude Code), with its env, before its services start; again before a restartOnChange restart when dependency files changed, so keep it idempotent (mix deps.get, not an alias that seeds).";
      };
      services = mkOption {
        type = types.attrsOf service;
        default = { };
        description = "Processes of every checkout, started on demand. Names become hostnames: https://<worktree>.<service>.<project>.localhost.";
      };
      server = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "mix phx.server";
        description = "Shorthand for `lazyCowTree.services.web.exec`.";
      };
      worktreesDir = mkOption {
        type = types.str;
        default = ".claude/worktrees";
        example = "../myapp-worktrees";
        description = "Where `lazy-cow-tree worktree new` and Claude Code's WorktreeCreate hook put worktrees, relative to the primary checkout. Outside it (`../<name>`), language servers and indexers of the primary don't see the worktrees' files.";
      };
      autoRemoveMerged = mkOption {
        type = types.bool;
        default = true;
        description = "Remove a worktree (under the worktrees dir, unlocked, clean) once its branch's GitHub pull request merged with nothing newer in it.";
      };
      httpsPort = mkOption {
        type = types.nullOr types.port;
        default = null;
        example = 8443;
        description = "HTTPS proxy port (default: 443 where unprivileged processes may bind it, i.e. macOS or Linux with net.ipv4.ip_unprivileged_port_start <= 443, else 8443).";
      };
      httpPort = mkOption {
        type = types.nullOr types.port;
        default = null;
        example = 0;
        description = "Plain HTTP port that redirects to HTTPS; 0 disables (default: 80 where unprivileged processes may bind it, else off).";
      };
      home = mkOption {
        type = types.nullOr types.str;
        default =
          config.env.LAZY_COW_TREE_HOME or (
            if envHome != "" then
              envHome
            else if userHome != "" then
              "${userHome}/.local/state/lazy-cow-tree"
            else
              null
          );
        defaultText = lib.literalExpression ''env.LAZY_COW_TREE_HOME, else $LAZY_COW_TREE_HOME, else "$HOME/.local/state/lazy-cow-tree"'';
        description = "The daemon's state directory (absolute), where its CA lives (`<home>/ca/ca.pem`); only read here, set LAZY_COW_TREE_HOME to move it.";
      };
      shellHook.enable = mkOption {
        type = types.bool;
        default = true;
        description = "Give every bash and zsh started from the devenv shell (Claude Code's and Codex's tool shells, their subagents') the environment of the checkout it runs in (DATABASE_URL, REDIS_URL, PORT, PHX_HOST, ...), again after cd/pushd/popd; restored or unset outside the project. Sets BASH_ENV and ZDOTDIR (your own files still run).";
      };
      processMarkerShim.enable = mkOption {
        type = types.bool;
        default = true;
        description = "macOS: load a small library (DYLD_INSERT_LIBRARIES) into what the devenv shell starts that keeps the worktree process marker open in children of node, bun, python and erlang, which close inherited fds, so `git worktree remove` lists (and FORCE_KILL_PROCESSES=1 kills) them too. Only acts in processes started in a worktree. Ignored by hardened-runtime binaries that don't allow DYLD environment variables (most notarized apps). Linux reads DEVENV_ROOT from /proc instead.";
      };
      git.enable = mkOption {
        type = types.bool;
        default = true;
        description = "Replace `git` in the shell with a wrapper whose `git worktree add` (by you, scripts or agents) fills the worktree like `lazy-cow-tree worktree new`: copy-on-write clones of the primary checkout, build caches included. Everything else is the real git. Replaces git-cow's devenv module (don't import both).";
      };
      protectPrimary = {
        enable = mkOption {
          type = types.bool;
          default = false;
          description = "Keep the primary checkout for `git pull`; all work happens in worktrees. In it (not in its worktrees) the shells started from the devenv shell run only `allow`ed commands, typed or given to `bash -c` / `zsh -c` (agents' tool shells), and point to `git worktree add` otherwise. Claude Code's Edit/Write tools refuse its files (a PreToolUse hook), and with a git-hooks input pre-commit/pre-merge-commit/pre-rebase hooks refuse there too, for git outside the shell. A guardrail against mistakes, not a security boundary. Needs shellHook.enable.";
        };
        allow = mkOption {
          type = types.listOf types.str;
          default = [
            "cd"
            "pushd"
            "popd"
            "git pull"
            "git fetch"
            "git status"
            "git log"
            "git diff"
            "git worktree"
            "gh"
            "lazy-cow-tree"
            "devenv"
            "claude"
            "codex"
            "pi"
            "exit"
          ];
          example = [
            "cd"
            "git pull"
            "git worktree add"
          ];
          description = "Commands allowed in the primary checkout: an entry allows a command starting with exactly its words ('git pull' allows `git pull --rebase`, not `git push`). Setting it replaces the default.";
        };
      };
      git.package = mkOption {
        type = types.package;
        default = pkgs.git;
        defaultText = lib.literalExpression "pkgs.git";
        description = "The real git the wrapper runs.";
      };
      gh.enable = mkOption {
        type = types.bool;
        default = true;
        description = "Replace `gh` in the shell with a wrapper: after `gh pr merge` or `gh pr close` leaves the pull request merged or closed, the worktree with its branch is removed (`lazy-cow-tree worktree rm`; one with uncommitted changes stays). Everything else is the real gh.";
      };
      gh.package = mkOption {
        type = types.package;
        default = pkgs.gh;
        defaultText = lib.literalExpression "pkgs.gh";
        description = "The real gh the wrappers run.";
      };
      codex = {
        lsp = mkOption {
          type = types.bool;
          default = true;
          description = "Give Codex (which has no LSP client) each supported language server of the enabled `languages.*` (dexter, typescript-language-server, pyright, rust-analyzer) as an MCP server `lsp-<name>` in the project's `.codex/config.toml` (read for trusted projects): mcp-language-server (definition, references, diagnostics, hover, rename, edit tools) on the server's wrapper, so it runs behind `lazy-cow-tree lsp` too. Add your own Codex settings through `files.\".codex/config.toml\".toml`.";
        };
        mcpLanguageServer = mkOption {
          type = types.package;
          default = pkgs.mcp-language-server;
          defaultText = lib.literalExpression "pkgs.mcp-language-server";
          description = "The LSP-to-MCP bridge `codex.lsp` and `pi.lsp` use (isaacphi/mcp-language-server).";
        };
        noDaemon = mkOption {
          type = types.bool;
          default = true;
          description = "Wrap `codex` to run with --no-daemon: a shared `codex app-server` started elsewhere runs commands with its own environment, not this shell's.";
        };
      };
      pi.lsp = mkOption {
        type = types.bool;
        default = true;
        description = "Give pi (which has no LSP client) each supported language server of the enabled `languages.*` (dexter, typescript-language-server, pyright, rust-analyzer) as an MCP server `lsp-<name>` in the project's `.pi/mcp.json` (read once the project is trusted): mcp-language-server on the server's wrapper, so it runs behind `lazy-cow-tree lsp` too. Add your own pi servers through `files.\".pi/mcp.json\".json.mcpServers`.";
      };
      claude.lsp = mkOption {
        type = types.bool;
        default = true;
        description = "Give Claude Code the supported language servers of the enabled `languages.*` (dexter, typescript-language-server, pyright, rust-analyzer) as plugin `lazy-cow-tree-lsp`, from a local marketplace set up in `.claude/settings.local.json`, each behind `lazy-cow-tree lsp`.";
      };
      claude.trustCa = mkOption {
        type = types.bool;
        default = true;
        description = "Set NODE_EXTRA_CA_CERTS to the local CA in `.claude/settings.local.json`, so Claude Code reaches MCP servers on https://*.localhost. Node reads one file only: to trust other CAs too, set `files.\".claude/settings.local.json\".json.env.NODE_EXTRA_CA_CERTS` to a bundle yourself.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    lazyCowTree = {
      services = lib.mkMerge [
        (lib.mkIf (cfg.server != null) { web.exec = lib.mkDefault cfg.server; })
        # Each field a default, so an explicit lazyCowTree.services.<name> wins.
        (lib.mapAttrs (_: lib.mapAttrs (_: lib.mkDefault)) derived)
      ];
      postgres = lib.mkIf pgCfg.enable {
        package = lib.mkDefault pgCfg.package;
        extensions = lib.mkDefault pgCfg.extensions;
        settings = lib.mkDefault (
          lib.mapAttrs (_: v: if builtins.isFloat v then toString v else v) (
            removeAttrs pgCfg.settings pgOwnSettings
          )
        );
        ramdiskMB = lib.mkIf pgCfg.dangerouslyDisableDurabilityForSpeed.enable (
          lib.mkDefault (megabytes pgCfg.dangerouslyDisableDurabilityForSpeed.ramdiskSize)
        );
      };
      redis = lib.mkIf redisCfg.enable (lib.mkDefault redisCfg.package);
    };

    # One proxy, one CA: lazy-cow-tree serves the hostnames.
    process.proxy.enable = lib.mkIf (derivedProcs != { }) (lib.mkForce false);

    assertions =
      lib.mapAttrsToList (name: p: {
        assertion =
          derived.${name}.start != "demand"
          || derived.${name}.http
          || lib.any (other: builtins.elem name other.dependsOn) (
            lib.attrValues (removeAttrs derived [ name ])
          );
        message = ''processes.${name}: start.on = "demand" but nothing can start it: give it an http port (ports.http.allocate), make another process depend on it (after = [ "devenv:processes:${name}" ]), or use start.on = "up".'';
      }) derivedProcs
      ++ [
        {
          assertion =
            !(
              pgCfg.instance == "unique"
              && pgCfg.copyOnWrite.enable
              && options.services.postgres.copyOnWrite.enable.highestPrio < 1500
            );
          message = ''services.postgres: copyOnWrite clones databases inside one shared cluster; set copyOnWrite.enable = false or instance = "shared".'';
        }
        {
          # TODO: a PostgreSQL cluster per checkout (shares machinery with a cluster per
          # durability setting).
          assertion = !(pgCfg.enable && pgCfg.instance == "unique");
          message = ''services.postgres.instance = "unique" isn't supported yet: every checkout shares lazy-cow-tree's cluster (with databases of its own). Remove it or set instance = "shared".'';
        }
      ];

    warnings =
      lib.optional (cfg.protectPrimary.enable && !cfg.shellHook.enable) "lazyCowTree.protectPrimary needs lazyCowTree.shellHook.enable: shells don't guard the primary checkout."
      ++ lib.optional (cfg.protectPrimary.enable && !(inputs ? git-hooks)) "lazyCowTree.protectPrimary: no git-hooks input, so git outside the devenv shell (IDEs, GUIs) can still commit in the primary checkout. Add it: devenv inputs add git-hooks github:cachix/git-hooks.nix --follows nixpkgs"
      # Accepted so configs don't need changing later, but the daemon doesn't act on them yet.
      ++ lib.optional (pgCfg.enable && pgCfg.start.on == "demand") ''services.postgres.start.on = "demand" isn't supported yet: lazy-cow-tree starts its PostgreSQL with the daemon.''
      ++ lib.optional (pgCfg.enable && pgCfg.start.idleTimeout != null) "services.postgres.start.idleTimeout isn't supported yet: lazy-cow-tree's PostgreSQL runs until the daemon stops."
      ++ lib.optional (pgCfg.enable && pgCfg.initialDatabases != [ ]) "services.postgres.initialDatabases: lazy-cow-tree gives every checkout <project>_dev and <project>_test (DATABASE_URL, TEST_DATABASE_URL) instead of these names for now.";

    cachix.pull = lib.mkIf cfg.cachix.enable [ "lazy-cow-tree" ];

    packages = [
      cfg.package
      postgres
      cfg.redis
    ]
    ++ lib.optionals cfg.git.enable [
      (lib.hiPrio gitWrapper)
      cfg.git.package
    ]
    ++ lib.optionals cfg.gh.enable [
      (lib.hiPrio ghWrapper)
      cfg.gh.package
    ]
    ++ lib.mapAttrsToList (_: s: lib.hiPrio (lspWrapper s.cmd)) lspServers;

    env = {
      LAZY_COW_TREE_PORT = toString cfg.port;
      LAZY_COW_TREE_SERVICES = builtins.toJSON cfg.services;
      LAZY_COW_TREE_NO_AUTO_REMOVE = lib.boolToString (!cfg.autoRemoveMerged);
      LAZY_COW_TREE_WORKTREES_DIR = cfg.worktreesDir;
      LAZY_COW_TREE_RAMDISK_MB = toString cfg.postgres.ramdiskMB;
      LAZY_COW_TREE_POSTGRES_BIN = "${postgres}/bin";
      LAZY_COW_TREE_POSTGRES_SETTINGS = builtins.toJSON cfg.postgres.settings;
      LAZY_COW_TREE_POSTGRES_EXTENSIONS = lib.concatStringsSep "," cfg.postgres.createExtensions;
      LAZY_COW_TREE_REDIS_SERVER = lib.getExe' cfg.redis "redis-server";
      LAZY_COW_TREE_POSTGRES_DURABLE =
        if pgCfg.dangerouslyDisableDurabilityForSpeed.enable then "0" else "1";
      LAZY_COW_TREE_POSTGRES_INSTANCE = pgCfg.instance;
      LAZY_COW_TREE_POSTGRES_START = pgCfg.start.on;
      LAZY_COW_TREE_POSTGRES_COW = if pgCfg.copyOnWrite.enable then "1" else "0";
      LAZY_COW_TREE_POSTGRES_TEMPLATE_REFRESH = pgCfg.copyOnWrite.refresh;
      LAZY_COW_TREE_REDIS_INSTANCE = redisCfg.instance;
      LAZY_COW_TREE_REDIS_START = redisCfg.start.on;
    }
    // lib.optionalAttrs (pgCfg.start.idleTimeout != null) {
      LAZY_COW_TREE_POSTGRES_IDLE_TIMEOUT = toString (seconds pgCfg.start.idleTimeout);
    }
    // lib.optionalAttrs (redisCfg.start.idleTimeout != null) {
      LAZY_COW_TREE_REDIS_IDLE_TIMEOUT = toString (seconds redisCfg.start.idleTimeout);
    }
    // lib.optionalAttrs (pgCfg.enable && pgCfg.initialDatabases != [ ]) {
      LAZY_COW_TREE_POSTGRES_INITIAL_DATABASES = builtins.toJSON (map (d: d.name) pgCfg.initialDatabases);
    }
    // lib.optionalAttrs config.devenv.isTesting { LAZY_COW_TREE_NO_SYNC = "1"; }
    // lib.optionalAttrs (cfg.postgres.databases != [ ]) {
      LAZY_COW_TREE_DATABASES = lib.concatStringsSep "," cfg.postgres.databases;
    }
    // lib.optionalAttrs (cfg.httpsPort != null) { LAZY_COW_TREE_HTTPS_PORT = toString cfg.httpsPort; }
    // lib.optionalAttrs (cfg.httpPort != null) { LAZY_COW_TREE_HTTP_PORT = toString cfg.httpPort; }
    // lib.optionalAttrs (cfg.project != null) { LAZY_COW_TREE_PROJECT = cfg.project; }
    // lib.optionalAttrs (cfg.tls.domain != null) {
      LAZY_COW_TREE_TLS_DOMAIN = cfg.tls.domain;
    }
    // lib.optionalAttrs (cfg.tls.domain != null && cfg.tls.githubRepository != null) {
      LAZY_COW_TREE_TLS_GITHUB_REPOSITORY = cfg.tls.githubRepository;
    }
    // lib.optionalAttrs (cfg.tls.domain != null && cfg.tls.services != null) {
      LAZY_COW_TREE_TLS_SERVICES = lib.concatStringsSep "," cfg.tls.services;
    }
    # Under `devenv test` the checkout under test migrates itself.
    // lib.optionalAttrs (cfg.migrate != null && !config.devenv.isTesting) {
      LAZY_COW_TREE_MIGRATE = cfg.migrate;
    }
    // lib.optionalAttrs (cfg.seed != null && !config.devenv.isTesting) { LAZY_COW_TREE_SEED = cfg.seed; }
    // lib.optionalAttrs (cfg.setup != null && !config.devenv.isTesting) {
      LAZY_COW_TREE_SETUP = cfg.setup;
    };

    # A daemon started in the background can't read gh's token from the macOS keychain
    # (only `security`, which gh uses, may); gh can. It registers with the project, so the
    # daemon uses each project's own token, whichever project started it.
    processes.lazy-cow-tree.exec = ''
      GH_TOKEN="''${GH_TOKEN:-$(${lib.getExe pkgs.gh} auth token 2>/dev/null)}" exec ${exe} serve
    '';

    # For git outside the shell (IDEs, GUIs); worktrees share the hooks, so they
    # check where they run.
    git-hooks.hooks.lazy-cow-tree-protect-primary = lib.mkIf (cfg.protectPrimary.enable && inputs ? git-hooks) {
      enable = true;
      name = "not in the primary checkout (lazyCowTree.protectPrimary)";
      entry = toString (
        pkgs.writeShellScript "lazy-cow-tree-protect-primary" ''
          [ "$(git rev-parse --absolute-git-dir)" = "$(git rev-parse --path-format=absolute --git-common-dir)" ] || exit 0
          echo "$(git rev-parse --show-toplevel) is the primary checkout (lazyCowTree.protectPrimary): work in a worktree (git worktree add ${cfg.worktreesDir}/<name>)." >&2
          exit 1
        ''
      );
      stages = [
        "pre-commit"
        "pre-merge-commit"
        "pre-rebase"
      ];
      pass_filenames = false;
      always_run = true;
    };

    enterShell =
      if cfg.shellHook.enable then
        ''
          # env.BASH_ENV is filtered out by devenv, so exported here. Keeps the user's own
          # BASH_ENV / ZDOTDIR (the hook and zdotdir run them), unless already ours, from
          # this build or an older one (a shell started before the module changed): then
          # the user's stays the one saved then.
          case "''${BASH_ENV:-}" in
            *-lazy-cow-tree-shell-hook.sh) ;;
            *) export LAZY_COW_TREE_BASH_ENV=''${BASH_ENV:-} ;;
          esac
          export BASH_ENV=${shellHook}
          case "''${ZDOTDIR:-}" in
            *-lazy-cow-tree-zdotdir) ;;
            *) export LAZY_COW_TREE_ZDOTDIR=''${ZDOTDIR:-$HOME} ;;
          esac
          export ZDOTDIR=${zdotdir}
          . ${shellHook}
        ''
      else
        ''
          ${shimEnv}
          eval "$(${exe} shell-hook)"
        '';

    # First codex in PATH that isn't this wrapper (its `$0` is the inner script).
    scripts.codex = lib.mkIf cfg.codex.noDaemon {
      exec = ''
        for c in $(type -ap codex); do
          grep -qs codex-script "$c" || exec "$c" --no-daemon "$@"
        done
        echo "codex not found in PATH" >&2
        exit 127
      '';
      description = "codex --no-daemon (lazy-cow-tree.codex.noDaemon)";
    };

    files = {
      # Codex and pi have no LSP client: each language server as an MCP server.
      ".codex/config.toml".toml.mcp_servers = lib.mkIf (cfg.codex.lsp && lspServers != { }) lspMcpServers;
      ".pi/mcp.json".json.mcpServers = lib.mkIf (cfg.pi.lsp && lspServers != { }) lspMcpServers;
      # pi writes /mcp changes (enable, exposure) back to the file defining the server.
      ".pi/mcp.json".copyMode = lib.mkIf (cfg.pi.lsp && lspServers != { }) "copy";

      ".claude/settings.local.json".json = {
        # Claude Code runs language servers from plugins only: ours, from a local marketplace.
        extraKnownMarketplaces = lib.mkIf claudeLsp {
          lazy-cow-tree.source = {
            source = "directory";
            path = "${claudeMarketplace}";
          };
        };
        enabledPlugins = lib.mkIf claudeLsp {
          "lazy-cow-tree-lsp@lazy-cow-tree" = true;
        };

        # Node (Claude Code) ignores the keychain `lazy-cow-tree trust` writes to.
        env = lib.mkIf (cfg.claude.trustCa && cfg.home != null) {
          NODE_EXTRA_CA_CERTS = lib.mkDefault "${cfg.home}/ca/ca.pem";
        };

        hooks = {
          PreToolUse = lib.mkIf cfg.protectPrimary.enable [
            {
              matcher = "Edit|MultiEdit|Write|NotebookEdit";
              hooks = [
                {
                  type = "command";
                  command = "${exe} hook guard-primary ${lib.escapeShellArg config.devenv.root} ${lib.escapeShellArg cfg.worktreesDir}";
                }
              ];
            }
          ];
          WorktreeCreate = [
            {
              hooks = [
                {
                  type = "command";
                  command = "${exe} hook worktree-create";
                  timeout = 120;
                }
              ];
            }
          ];
          WorktreeRemove = [
            {
              hooks = [
                {
                  type = "command";
                  command = "${exe} hook worktree-remove";
                }
              ];
            }
          ];
        };
      };
    };
  };
}
