# devenv module: runs `localforest serve` for this project and wires every checkout to it.
#
#   # devenv.yaml                              # devenv.nix
#   inputs:                                    { inputs, ... }: {
#     localforest:                               imports = [ inputs.localforest.devenvModules.default ];
#       url: github:onnimonni/localforest      }
#
#   (or `flake: false` on the input and `imports: [ localforest/devenv-module ]`)
#
# It derives localforest's services from devenv's own `processes` (exec, cwd, ports,
# proxy.hostname, after, ready, restart, watch, plus the new `start.on` /
# `start.idleTimeout`), its database from `services.postgres` (plus `instance`,
# `start`, `copyOnWrite`, `dangerouslyDisableDurabilityForSpeed`), Redis from
# `services.redis` (plus `instance`, `start`) and the checkout's env files from `dotenv`,
# and keeps devenv from starting its own copies. `localforest.*` options still work and
# win over what is derived:
#
#   localforest.migrate = "mix ecto.migrate";
#   localforest.seed = "mix run priv/repo/seeds.exs";
#   localforest.setup = "mix deps.get";           # once in every new checkout
#   localforest.services.api = { exec = "bun run dev"; cwd = "api"; };
#   localforest.lsp.elixir = [ "dexter" "lsp" ];
#
# Every checkout (primary and worktrees) gets each service on demand:
# https://<worktree>.<service>.<project>.localhost (https://<service>.<project>.localhost
# in the primary) starts it, after what it depends on, with its own PORT and the
# checkout's DATABASE_URL and REDIS_URL. `localforest env [--service x]` gives every
# shell the same environment. Claude Code's WorktreeCreate/WorktreeRemove
# hooks go through the daemon, so `claude --worktree` and `isolation: worktree`
# subagents get provisioned worktrees too, and its NODE_EXTRA_CA_CERTS trusts the
# local CA (MCP servers on https://*.localhost).
{
  pkgs,
  lib,
  config,
  options,
  ...
}:

let
  cfg = config.localforest;
  exe = lib.getExe cfg.package;
  # localforest's own nixpkgs (flake.lock), so the default package is the exact
  # derivation CI pushes to localforest.cachix.org, whatever nixpkgs the consumer uses.
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
  envHome = builtins.getEnv "LOCALFOREST_HOME";
  userHome = builtins.getEnv "HOME";
  inherit (lib) mkOption types;
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
        description = "Variable holding the port (default: `<NAME>_PORT`); also always LOCALFOREST_<SERVICE>_<NAME>_PORT (and _URL with http).";
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
        throw "localforest: duration ${builtins.toJSON d} is not like 30s, 15m or 1h"
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
        throw "localforest: size ${builtins.toJSON s} is not like 512M or 4G"
      else if lib.toLower (toString (builtins.elemAt m 1)) == "g" then
        n * 1024
      else
        n;
  duration = types.nullOr (types.either types.ints.unsigned types.str);

  # devenv's processes that localforest runs instead: every one but its own, devenv's
  # postgres/redis (localforest provides those) and ones opted out.
  own = [
    "localforest"
    "postgres"
    "redis"
  ];
  derivedProcs = lib.filterAttrs (
    name: p: cfg.enable && !(builtins.elem name own) && p.localforest.enable
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
      # localforest sets PORT and each named port's variable per checkout.
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
      "localforest: processes.${name}.after: ${lib.concatStringsSep ", " ignored} ignored (only other processes localforest runs are started first)"
      {
        exec = toString (pkgs.writeShellScript "localforest-${name}" p.exec);
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
        start = p.start.on;
        idleTimeout = seconds p.start.idleTimeout;
        hostname = p.proxy.hostname;
        inherit (p.localforest) migrate restartOnPull;
      };

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
                type = types.enum [
                  "up"
                  "demand"
                  "manual"
                ];
                default = "demand";
                description = "When localforest starts it: with its checkout (`up`), on the first request or as a dependency (`demand`), or only by `localforest service start` (`manual`).";
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
        localforest = {
          enable = mkOption {
            type = types.bool;
            default = true;
            description = "Run this process in every checkout through localforest; false leaves it to devenv (primary checkout only).";
          };
          migrate = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Migrate command for the checkout's database, run in the process's cwd after `localforest.migrate`.";
          };
          restartOnPull = mkOption {
            type = types.bool;
            default = false;
            description = "Restart it (if running) after the base branch was pulled into its checkout and migrations ran.";
          };
        };
      };
      # localforest runs it (and provides postgres/redis): `devenv up` doesn't too.
      config.start.enable = lib.mkIf (
        cfg.enable
        && (
          (builtins.elem name own && name != "localforest")
          || (!(builtins.elem name own) && config.localforest.enable)
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
  # devenv's postgres module sets these itself; localforest's cluster decides them.
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
        description = "The service `localforest env` and `localforest service` pick without a name (default: `web`, else the first http service).";
      };
      portOffset = mkOption {
        type = types.nullOr (types.ints.between 0 9);
        default = null;
        description = "PORT = the checkout's base port + this (default: position by name).";
      };
      # FIXME: all services of a checkout share its DATABASE_URL and REDIS_URL. Add
      # `postgres` / `redis` options for a database / redis-server of their own.
      migrate = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Migrate/seed command for the checkout's database, run in its cwd after `localforest.migrate` when the base branch moves (primary) or was merged in (worktrees).";
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
        description = "When it exits on its own: leave it down, start it again after a non-zero exit, or after any exit (backing off 1-30 s; `localforest service stop` keeps it down).";
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
        description = "Files (relative to cwd, `*` / `?` in the file name) whose content changing restarts it if running (after localforest.setup when a dependency manifest or lockfile such as mix.lock, Gemfile.lock or package.json changed; not while its checkout pulls or migrates). Default: for a command running `mix`, mix.exs, mix.lock and config/*.exs (Phoenix's code reloader refuses to compile after those change); `[ ]` for others and to turn it off.";
      };
      start = mkOption {
        type = types.enum [
          "up"
          "demand"
          "manual"
        ];
        default = "demand";
        description = "Start it with its checkout (`up`), on the first request or as a dependency (`demand`), or only with `localforest service start` (`manual`).";
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
        description = "Hostname in the primary checkout instead of <service>.<project>.localhost; worktrees prefix `<worktree>.`.";
      };
    };
  };
in
{
  # Merged into devenv's own options: `processes.<name>.start.{on,idleTimeout}`,
  # `processes.<name>.localforest.*`, and new `services.postgres` / `services.redis`
  # settings. localforest derives its services and database settings from them.
  options.processes = mkOption { type = types.attrsOf processExtension; };

  options.services.postgres = {
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
        description = "Refresh the template from the primary when the base branch moves (after migrations), or only with `localforest snapshot`.";
      };
    };
    dangerouslyDisableDurabilityForSpeed = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Run PostgreSQL on a RAM disk with fsync, synchronous_commit and
          full_page_writes off. Every database is lost on reboot, on a crash (which
          can also corrupt the cluster) and on `localforest down --eject`. For
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

  options.services.redis = {
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

  options.localforest = {
    enable = mkOption {
      type = types.bool;
      default = true;
      description = "Run and use the localforest daemon.";
    };
    package = mkOption {
      type = types.package;
      default = pinnedPkgs.callPackage ../package.nix { };
      defaultText = lib.literalMD "built with localforest's pinned nixpkgs (flake.lock), substituted from localforest.cachix.org";
      example = lib.literalExpression "pkgs.callPackage (inputs.localforest + \"/package.nix\") { }";
      description = "The localforest package. The default is the one localforest's CI builds and pushes to localforest.cachix.org; a package built with other nixpkgs is compiled locally.";
    };
    cachix.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Pull the default package from localforest.cachix.org (`cachix.pull`). A multi-user Nix only uses it for trusted users, or when it's in the daemon's own substituters.";
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
        description = "Size of the APFS RAM disk PostgreSQL runs on, in MB (memory is only used as it fills). The daemon that creates the RAM disk sizes it; `localforest down --eject` and a restart apply a new size (and empty every database).";
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
      description = "Setup command run once in every new checkout (made by localforest, git, git-cow or Claude Code), with its env, before its services start; again before a restartOnChange restart when dependency files changed, so keep it idempotent (mix deps.get, not an alias that seeds).";
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
      description = "Shorthand for `localforest.services.web.exec`.";
    };
    previewTtlHours = mkOption {
      type = types.ints.unsigned;
      default = 48;
      description = "Close a preview (a removed worktree recreated from its \"gone\" page) after this many hours without requests or database / Redis connections; 0 keeps previews until removed.";
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
        config.env.LOCALFOREST_HOME or (
          if envHome != "" then
            envHome
          else if userHome != "" then
            "${userHome}/.local/state/localforest"
          else
            null
        );
      defaultText = lib.literalExpression ''env.LOCALFOREST_HOME, else $LOCALFOREST_HOME, else "$HOME/.local/state/localforest"'';
      description = "The daemon's state directory (absolute), where its CA lives (`<home>/ca/ca.pem`); only read here, set LOCALFOREST_HOME to move it.";
    };
    claude.trustCa = mkOption {
      type = types.bool;
      default = true;
      description = "Set NODE_EXTRA_CA_CERTS to the local CA in `.claude/settings.local.json`, so Claude Code reaches MCP servers on https://*.localhost. Node reads one file only: to trust other CAs too, set `files.\".claude/settings.local.json\".json.env.NODE_EXTRA_CA_CERTS` to a bundle yourself.";
    };
    envFiles = mkOption {
      type = types.listOf types.str;
      default = if config.dotenv.enable then lib.toList config.dotenv.filename else [ ".env.local" ];
      defaultText = lib.literalExpression ''if dotenv.enable then dotenv.filename else [ ".env.local" ]'';
      description = "Files, relative to each checkout, with the checkout's own variables: read on every start of anything in it and applied last (later files win). A new worktree starts with a copy of the primary's.";
    };
    lsp = mkOption {
      type = types.attrsOf (types.listOf types.str);
      default = { };
      example = {
        elixir = [
          "dexter"
          "lsp"
        ];
      };
      description = "Language servers to run behind `localforest lsp` (one per worktree); adds `localforest-lsp-<name>` commands for Claude Code's lspServers.";
    };
  };

  config = lib.mkIf cfg.enable {
    localforest.services = lib.mkMerge [
      (lib.mkIf (cfg.server != null) { web.exec = lib.mkDefault cfg.server; })
      # Each field a default, so an explicit localforest.services.<name> wins.
      (lib.mapAttrs (_: lib.mapAttrs (_: lib.mkDefault)) derived)
    ];

    # One proxy, one CA: localforest serves the hostnames.
    process.proxy.enable = lib.mkIf (derivedProcs != { }) (lib.mkForce false);

    localforest.postgres = lib.mkIf pgCfg.enable {
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
    localforest.redis = lib.mkIf redisCfg.enable (lib.mkDefault redisCfg.package);

    assertions =
      lib.mapAttrsToList (name: p: {
        assertion =
          p.start.on != "demand"
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
          message = ''services.postgres.instance = "unique" isn't supported yet: every checkout shares localforest's cluster (with databases of its own). Remove it or set instance = "shared".'';
        }
      ];

    # Accepted so configs don't need changing later, but the daemon doesn't act on them yet.
    warnings =
      lib.optional (pgCfg.enable && pgCfg.start.on == "demand") ''services.postgres.start.on = "demand" isn't supported yet: localforest starts its PostgreSQL with the daemon.''
      ++ lib.optional (pgCfg.enable && pgCfg.start.idleTimeout != null) "services.postgres.start.idleTimeout isn't supported yet: localforest's PostgreSQL runs until the daemon stops."
      ++ lib.optional (pgCfg.enable && pgCfg.initialDatabases != [ ]) "services.postgres.initialDatabases: localforest gives every checkout <project>_dev and <project>_test (DATABASE_URL, TEST_DATABASE_URL) instead of these names for now.";

    cachix.pull = lib.mkIf cfg.cachix.enable [ "localforest" ];

    packages = [
      cfg.package
      postgres
      cfg.redis
    ]
    ++ lib.mapAttrsToList (
      name: cmd:
      pkgs.writeShellScriptBin "localforest-lsp-${name}" ''
        exec ${exe} lsp -- ${lib.escapeShellArgs cmd} "$@"
      ''
    ) cfg.lsp;

    env = {
      LOCALFOREST_PORT = toString cfg.port;
      LOCALFOREST_SERVICES = builtins.toJSON cfg.services;
      LOCALFOREST_PREVIEW_TTL_HOURS = toString cfg.previewTtlHours;
      LOCALFOREST_RAMDISK_MB = toString cfg.postgres.ramdiskMB;
      LOCALFOREST_POSTGRES_BIN = "${postgres}/bin";
      LOCALFOREST_POSTGRES_SETTINGS = builtins.toJSON cfg.postgres.settings;
      LOCALFOREST_POSTGRES_EXTENSIONS = lib.concatStringsSep "," cfg.postgres.createExtensions;
      LOCALFOREST_REDIS_SERVER = lib.getExe' cfg.redis "redis-server";
      LOCALFOREST_POSTGRES_DURABLE =
        if pgCfg.dangerouslyDisableDurabilityForSpeed.enable then "0" else "1";
      LOCALFOREST_POSTGRES_INSTANCE = pgCfg.instance;
      LOCALFOREST_POSTGRES_START = pgCfg.start.on;
      LOCALFOREST_POSTGRES_COW = if pgCfg.copyOnWrite.enable then "1" else "0";
      LOCALFOREST_POSTGRES_TEMPLATE_REFRESH = pgCfg.copyOnWrite.refresh;
      LOCALFOREST_REDIS_INSTANCE = redisCfg.instance;
      LOCALFOREST_REDIS_START = redisCfg.start.on;
      LOCALFOREST_ENV_FILES = builtins.toJSON cfg.envFiles;
    }
    // lib.optionalAttrs (pgCfg.start.idleTimeout != null) {
      LOCALFOREST_POSTGRES_IDLE_TIMEOUT = toString (seconds pgCfg.start.idleTimeout);
    }
    // lib.optionalAttrs (redisCfg.start.idleTimeout != null) {
      LOCALFOREST_REDIS_IDLE_TIMEOUT = toString (seconds redisCfg.start.idleTimeout);
    }
    // lib.optionalAttrs (pgCfg.enable && pgCfg.initialDatabases != [ ]) {
      LOCALFOREST_POSTGRES_INITIAL_DATABASES = builtins.toJSON (map (d: d.name) pgCfg.initialDatabases);
    }
    // lib.optionalAttrs config.devenv.isTesting { LOCALFOREST_NO_SYNC = "1"; }
    // lib.optionalAttrs (cfg.httpsPort != null) { LOCALFOREST_HTTPS_PORT = toString cfg.httpsPort; }
    // lib.optionalAttrs (cfg.httpPort != null) { LOCALFOREST_HTTP_PORT = toString cfg.httpPort; }
    // lib.optionalAttrs (cfg.project != null) { LOCALFOREST_PROJECT = cfg.project; }
    # Under `devenv test` the checkout under test migrates itself.
    // lib.optionalAttrs (cfg.migrate != null && !config.devenv.isTesting) {
      LOCALFOREST_MIGRATE = cfg.migrate;
    }
    // lib.optionalAttrs (cfg.seed != null && !config.devenv.isTesting) { LOCALFOREST_SEED = cfg.seed; }
    // lib.optionalAttrs (cfg.setup != null && !config.devenv.isTesting) {
      LOCALFOREST_SETUP = cfg.setup;
    };

    processes.localforest.exec = "${exe} serve";

    enterShell = ''
      eval "$(${exe} env)"
    '';

    # Node (Claude Code) ignores the keychain `localforest trust` writes to.
    files.".claude/settings.local.json".json.env = lib.mkIf (cfg.claude.trustCa && cfg.home != null) {
      NODE_EXTRA_CA_CERTS = lib.mkDefault "${cfg.home}/ca/ca.pem";
    };

    files.".claude/settings.local.json".json.hooks = {
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
}
