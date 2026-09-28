# devenv module: runs `localforest serve` for this project and wires every checkout to it.
#
#   # devenv.yaml
#   inputs:
#     localforest:
#       url: github:onnimonni/localforest   # or a path: input
#       flake: false
#   imports:
#     - localforest/devenv-module
#
#   # devenv.nix
#   localforest.port = 4000;                  # base port of the primary checkout's services
#   localforest.migrate = "mix ecto.migrate";
#   localforest.seed = "mix run priv/repo/seeds.exs";
#   localforest.setup = "mix deps.get";           # once in every new checkout
#   localforest.services = {
#     web = { exec = "mix phx.server"; dependsOn = [ "worker" ]; };
#     api = { exec = "bun run dev"; cwd = "api"; migrate = "bun run migrate"; };
#     worker = { exec = "mix run --no-halt"; http = false; restart = "on-failure"; };
#   };
#   localforest.lsp.elixir = [ "dexter" "lsp" ];
#
# Every checkout (primary and worktrees) gets each service on demand:
# https://<worktree>.<service>.<project>.localhost (https://<service>.<project>.localhost
# in the primary) starts it, after what it depends on, with its own PORT and the
# checkout's DATABASE_URL and REDIS_URL. `localforest env [--service x]` gives every
# shell the same environment. Claude Code's WorktreeCreate/WorktreeRemove
# hooks go through the daemon, so `claude --worktree` and `isolation: worktree`
# subagents get provisioned worktrees too.
{
  pkgs,
  lib,
  config,
  ...
}:

let
  cfg = config.localforest;
  exe = lib.getExe cfg.package;
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
        example = [ "Gemfile.lock" "config/*.rb" ];
        description = "Files (relative to cwd, `*` / `?` in the file name) whose content changing restarts it if running (after localforest.setup when a dependency manifest or lockfile such as mix.lock, Gemfile.lock or package.json changed; not while its checkout pulls or migrates). Default: for a command running `mix`, mix.exs, mix.lock and config/*.exs (Phoenix's code reloader refuses to compile after those change); `[ ]` for others and to turn it off.";
      };
    };
  };
in
{
  options.localforest = {
    enable = mkOption {
      type = types.bool;
      default = true;
      description = "Run and use the localforest daemon.";
    };
    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ../package.nix { };
      description = "The localforest package.";
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
    localforest.services = lib.mkIf (cfg.server != null) { web.exec = lib.mkDefault cfg.server; };

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
    }
    // lib.optionalAttrs (cfg.httpsPort != null) { LOCALFOREST_HTTPS_PORT = toString cfg.httpsPort; }
    // lib.optionalAttrs (cfg.httpPort != null) { LOCALFOREST_HTTP_PORT = toString cfg.httpPort; }
    // lib.optionalAttrs (cfg.project != null) { LOCALFOREST_PROJECT = cfg.project; }
    // lib.optionalAttrs (cfg.migrate != null) { LOCALFOREST_MIGRATE = cfg.migrate; }
    // lib.optionalAttrs (cfg.seed != null) { LOCALFOREST_SEED = cfg.seed; }
    // lib.optionalAttrs (cfg.setup != null) { LOCALFOREST_SETUP = cfg.setup; };

    processes.localforest.exec = "${exe} serve";

    enterShell = ''
      eval "$(${exe} env)"
    '';

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
