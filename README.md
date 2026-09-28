# localforest

Local dev infrastructure for many git worktrees at once, in one binary. Made for
coding agents (Claude Code) that give every task its own worktree: each worktree
gets its own services, databases, Redis and HTTPS hostnames, created in
milliseconds, started on demand and cleaned up when its PR merges.

```console
$ localforest worktree new fix-login
https://fix-login.web.myapp.localhost
/src/myapp/.claude/worktrees/fix-login
$ curl https://fix-login.web.myapp.localhost   # starts web (and the worker it depends on)
$ curl https://fix-login.api.myapp.localhost   # starts api (same DATABASE_URL and REDIS_URL)
$ cd .claude/worktrees/fix-login && eval "$(localforest env)"
$ echo $DATABASE_URL $REDIS_URL
postgres://myapp-fix-login:4c1f…@127.0.0.1:55432/myapp_dev_fix_login redis://:myapp-fix-login@127.0.0.1:6380/0
```

## What it does

| | |
|---|---|
| **Worktrees** | `git worktree add` without a checkout, then filled with copy-on-write clones of the primary checkout, build caches included ([git-cow](https://github.com/onnimonni/git-cow)): ~0 disk, nothing to recompile. In `.claude/worktrees/<name>`, where Claude Code puts its own. |
| **File watcher** | Watches `.git/worktrees`: a worktree made any way (plain `git`, git-cow, Claude Code) is provisioned; one deleted by hand is cleaned up. |
| **Services** | Every checkout runs the project's services (`localforest.services`), each with its own port and `https://<worktree>.<service>.<project>.localhost` (`<service>.<project>.localhost` in the primary). The first request to a service starts it, after the services it depends on; workers without http run as dependencies. |
| **PostgreSQL** | One PostgreSQL 18 on an APFS RAM disk, `fsync=off`. localforest is a proxy in front of it: the user in the connection picks the checkout, a database is created on first connect as a copy-on-write clone of its template (always `SET file_copy_method = clone` + `STRATEGY FILE_COPY`: 200 MB in ~40 ms instead of ~450 ms), and a checkout can only open its own databases. Every checkout has its own role and password. |
| **Redis** | One port; the password picks the checkout's own `redis-server`, on a private unix socket, started on first use and killed with the worktree. Real Redis: pub/sub, Lua, streams, `FLUSHALL` only touch that one. |
| **HTTPS** | Local CA, websockets included. `https://localforest.localhost` lists everything. |
| **GitHub** | Webhook websocket (polling without repo admin rights): pushes pull every branch and merge the base branch into worktrees (conflict-free merges only, dirty worktrees skipped). A worktree whose PR merged is removed unless it has newer work. |
| **Migrations** | When the base branch moves: the migrate commands run in the primary checkout, then the template is refreshed from its database; they also run in every worktree the base branch was merged into. |
| **Gone pages** | A removed worktree's hostnames answer 503 with why it's gone (pull request merged, removed, deleted), links to the PR, branch and commit on GitHub / GitLab / Bitbucket / Gitea, and a button that recreates it as a preview. |
| **LSP proxy** | `localforest lsp -- <server>` runs one language server per worktree, routes each request by file, and drops results from other worktrees. |

Removing a worktree SIGKILLs everything running in it (each service's process group,
plus any process whose working directory or executable is inside it, with all
descendants: the BEAM, esbuild, tailwind, node, …), stops its redis-server, drops
its databases and role, deletes its branch and moves the files away for background
deletion.

## Install

Binaries for macOS (arm64) and Linux (x86_64) are on the
[releases page](https://github.com/onnimonni/localforest/releases); or
`nix profile install github:onnimonni/localforest`, or
`cargo install --git https://github.com/onnimonni/localforest`. localforest runs
`postgres`/`initdb` (18+) and `redis-server` from PATH unless told where they are
(the devenv module does).

## Install with devenv

```yaml
# devenv.yaml
inputs:
  localforest:
    url: github:onnimonni/localforest
    flake: false
imports:
  - localforest/devenv-module
```

```nix
# devenv.nix
{
  localforest.port = 4000; # base port of the primary checkout's services
  localforest.migrate = "mix ecto.migrate";
  localforest.seed = "mix run priv/repo/seeds.exs";
  localforest.setup = "mix deps.get";
  localforest.services = {
    web = { exec = "mix phx.server"; dependsOn = [ "worker" ]; };
    api = {
      exec = "bun run dev";
      cwd = "api";
      migrate = "bun run migrate";
    };
    worker = { exec = "mix run --no-halt"; http = false; restart = "on-failure"; };
  };
  localforest.lsp.elixir = [ "dexter" "lsp" ];
}
```

`devenv up` in the primary checkout starts the daemon (or registers the project with
the one already running; the first project to start it serves them all, and another
takes over if it stops). Every shell, primary or worktree, gets the default
service's environment from `eval "$(localforest env)"` (`localforest env -s api` for
another's). Claude Code's `WorktreeCreate` / `WorktreeRemove` hooks go through the
daemon, so `claude --worktree` and `isolation: worktree` subagents get provisioned
worktrees.

Trust the local CA once: `localforest trust` (macOS keychain, asks for your password).

| option | default | |
|---|---|---|
| `localforest.project` | directory name | hostnames, database prefix, role name |
| `localforest.port` | `4000` | base port of the primary checkout's services |
| `localforest.migrate` | none | migrate command: primary when the base branch moves (then the template is refreshed), worktrees the base branch was merged into |
| `localforest.seed` | none | seed command: primary, after `migrate`, when its database was just created; worktrees get seeded data via the template |
| `localforest.setup` | none | runs once in every new checkout (localforest, `git worktree add`, Claude Code), e.g. `mix deps.get` |
| `localforest.services.<name>` | none | see below |
| `localforest.server` | none | shorthand for `localforest.services.web.exec` |
| `localforest.previewTtlHours` | `48` | close previews after this many hours without activity; `0` keeps them |
| `localforest.httpsPort` | `443` | HTTPS proxy port |
| `localforest.lsp.<name>` | none | adds `localforest-lsp-<name>` for Claude Code's `lspServers` |
| `localforest.postgres.package` | `pkgs.postgresql_18` | PostgreSQL build (18+ for copy-on-write databases) |
| `localforest.postgres.extensions` | none | as in devenv: `extensions: [ extensions.postgis extensions.pgvector ]`; enable with `CREATE EXTENSION` |
| `localforest.postgres.settings` | `{}` | extra postgresql.conf settings, e.g. `shared_preload_libraries` |
| `localforest.postgres.ramdiskMB` | `4096` | RAM disk size (used as it fills); resizing needs `localforest down --eject`, which empties every database |
| `localforest.redis` | `pkgs.redis` | Redis build |

Service options:

| | default | |
|---|---|---|
| `exec` | | command |
| `cwd` | checkout root | working directory, relative to the checkout |
| `http` | `true` | listens on `$PORT`, gets a hostname, started by its first request; `false`: only started as a dependency |
| `default` | `web`, else first http service | what `localforest env` / `localforest service` pick without a name |
| `portOffset` | position by name | `PORT` = checkout base port + offset (0–9) |
| `migrate` | none | migrate/seed command for the checkout's database, run in its `cwd` after `localforest.migrate` |
| `dependsOn` | `[]` | started first |
| `env` | `{}` | extra environment |
| `restart` | `"no"` | when it exits on its own: `"no"`, `"on-failure"` (non-zero exit) or `"always"`; backs off 1–30 s, `localforest service stop` keeps it down |
| `restartOnPull` | `false` | restart it (if running) after the base branch was pulled into its checkout and migrated; for servers without a code reloader |

Commands are split like a shell would, then run directly (no shell) with the
service's environment and the project's `PATH`. Logs: `localforest service log -s <name>`.

## Environment

`localforest env [-s <service>]` prints (all derived from the checkout's path, no daemon
needed), e.g. for services `web` (default), `api` and
`worker`:

| | primary | worktree `fix-login` |
|---|---|---|
| `PORT` | base (`localforest.port`) + offset | base (20000–28990, hashed from the name) + offset |
| `LOCALFOREST_URL` | `web.myapp.localhost` | `fix-login.web.myapp.localhost` |
| `DATABASE_URL`, `PG*` | `myapp_dev` as role `myapp` | `myapp_dev_fix_login` as role `myapp-fix-login` |
| `TEST_DATABASE_URL` | `myapp_test` | `myapp_test_fix_login` |
| `REDIS_URL` | password `myapp` | password `myapp-fix-login` |
| `LOCALFOREST_<SERVICE>_URL`, `_PORT` | every service's | every service's |
| `LOCALFOREST_SERVICE`, `LOCALFOREST_WORKTREE`, `LOCALFOREST_PROJECT` | | |
| `NODE_EXTRA_CA_CERTS` | the local CA | |

Detected from the manifests in the service's `cwd`, set to its hostname so the dev
server accepts it (the service's `env` overrides them):

| | when |
|---|---|
| `PHX_HOST` | `mix.exs` (or an umbrella's `apps/*/mix.exs`) depends on `:phoenix` |
| `RAILS_DEVELOPMENT_HOSTS` | the `Gemfile` has `gem "rails"` (host authorization) |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | `package.json` mentions `vite` (allowed hosts) |

Without services, a checkout's app is at `<worktree>.<project>.localhost` on the
base port.

Point your apps at `DATABASE_URL` / `REDIS_URL` (Ecto: `url: System.fetch_env!("DATABASE_URL")`).
Test databases (and `MIX_TEST_PARTITION` ones) are the app's to create (`mix ecto.create`
works through the proxy); only the dev database is cloned from the template.

All services of a checkout share its `DATABASE_URL` and `REDIS_URL` for now
(FIXME: databases and redis-servers per service).

## New worktrees, however they're made

A worktree made by plain `git worktree add` (or any tool) is seen by the file
watcher. While it's still its fresh checkout (clean, nothing untracked, made in
the last 10 minutes) and lacks the primary's gitignored caches (`deps/`, `_build/`,
`node_modules/`, …), it's redone as a copy-on-write clone of the primary, caches
included and a carried Mix build relocated ([git-cow](https://github.com/onnimonni/git-cow)).
Language server indexes (`.dexter/`, `.elixir_ls/`, …) are never carried: they name
the primary's paths.

Every worktree gets its environment in `.env` too (a marked block at the top,
rewritten on each start; keys of a `.env` cloned from the primary are commented
out). If `.env` isn't gitignored it's added to `.git/info/exclude`; a tracked `.env`
is left alone. Then `localforest.setup` runs once in it.

## Removed worktrees and previews

localforest remembers removed worktrees in the main checkout's git dir
(`.git/localforest/worktrees.json`): branch, commit, when, and why. A request to one of
their hostnames gets a 503 page saying so (e.g. "Its pull request #42 was merged"),
with links to the pull request, branch and commit on the remote's forge.

Its **Recreate worktree to preview** button (a POST from the page itself) brings the
worktree back at the commit it was at (fetched from the remote or the pull request's
head if it's no longer local), with a fresh copy of the template database, and sends
you back to the page, whose service then starts on demand. A preview isn't
auto-removed for its merged pull request; it closes after `localforest.previewTtlHours`
(48) without requests or database / Redis connections, and the page shows the
original reason again.

## Commands

```sh
localforest serve                        # daemon (devenv process)
localforest env [-s <service>] [--json]  # this checkout's environment
localforest worktree new <name> [--base <ref>]
localforest worktree rm <name> [--force] # without --force only when nothing would be lost
localforest worktree list
localforest service start|stop|restart|log [-s <service>] [<worktree> | .]
localforest status                       # projects, worktrees, services, databases
localforest sync                         # pull, merge, remove merged, migrate now
localforest snapshot                     # template := the primary's dev database
localforest trust                        # trust the local CA
localforest down [--eject]               # stop the daemon; --eject drops the RAM disk
localforest lsp -- <server> [args]
```
## Ports

| | |
|---|---|
| 55432 | PostgreSQL proxy (the real server: `~/.local/state/localforest/pg/.s.PGSQL.55433`) |
| 6380 | Redis proxy |
| 443 / 80 | HTTPS proxy / redirect to HTTPS. Ports below 1024 bind all interfaces (unprivileged on macOS) and refuse non-loopback peers. |

All configurable (`--pg-port`, `--redis-port`, `--https-port`, `--http-port` or
`LOCALFOREST_*`). State lives in `~/.local/state/localforest` (`LOCALFOREST_HOME`).

## Notes

- The RAM disk is volatile: a reboot or `localforest down --eject` empties every
  database; the next start seeds the primary again through the migrate command.
- Worktree checkout roles are `SUPERUSER` (dev tooling expects it: extensions,
  `ecto.create`, objects owned by the primary's role in cloned databases); which
  databases they can open is enforced by the proxy.
- macOS has no API for RAM disks, so `hdiutil`/`diskutil` are run for it; PostgreSQL,
  redis-server, the migrate and service commands and language servers are also
  separate processes. Everything else (git, GitHub, certificates, keychain) is
  in-process.
- Linux: works without the RAM disk; databases are still cloned copy-on-write on
  btrfs / XFS (reflinks), plain copies elsewhere. `localforest.postgres.settings.file_copy_method = "copy"`
  turns cloning off.

## Development

```sh
devenv shell
cargo test          # tests/lsp_worktrees.rs also runs when `dexter` is in PATH
nix build .#localforest
```
