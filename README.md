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

## Why

[devenv](https://devenv.sh) is great for one checkout: `devenv up` and you have
PostgreSQL, Redis and your processes. Coding agents changed the shape of the work:
every task gets its own git worktree, often several at once, and devenv treats each
worktree as a new project:

- **Slow Nix evaluation per worktree.** Every new worktree is a new path, so
  `devenv shell` / `devenv up` evaluates the whole Nix configuration again before
  anything runs, for every task an agent starts. The evaluation is the slowest part
  of spinning up a worktree, and it's repeated for work that is identical to the
  primary checkout's.
- **One stack per worktree.** Each worktree's `devenv up` wants its own PostgreSQL,
  Redis and processes on the same ports as the primary's, with its own empty
  database to migrate and seed.
- **Shared git hooks.** Worktrees share `.git/hooks`; a worktree's devenv installs
  a pre-commit hook pointing at *its* `.pre-commit-config.yaml`, and commits break
  everywhere once that worktree is removed.

This started as shell scripts in a Phoenix project's devenv (a shared RAM-disk
PostgreSQL, hashed ports, `worktree-new` / `worktree-rm`) and became localforest:
**devenv is evaluated once, in the primary checkout, and worktrees don't run devenv
at all.** The daemon it starts gives every worktree what devenv would have: a
database (a copy-on-write clone, in milliseconds), Redis, ports, HTTPS hostnames and
the project's services, started on demand; the environment comes from
`localforest env` or the worktree's `.env`.

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

## Use with devenv

**1. Import the module** in the primary checkout's `devenv.yaml`:

```yaml
# devenv.yaml
inputs:
  localforest:
    url: github:onnimonni/localforest
    flake: false
imports:
  - localforest/devenv-module
```

**2. Describe the project** in `devenv.nix`. The module adds localforest,
PostgreSQL 18 and Redis to the shell, runs `localforest serve` as a devenv process,
exports the checkout's environment in `enterShell`, and wires Claude Code's
worktree hooks. Replace your own `services.postgres` / `services.redis` with it:

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

**3. Start it** with `devenv up` in the primary checkout. That starts the daemon, or
registers the project with the one already running: one daemon serves every
project, and another project's `devenv up` takes over if it stops.

**4. Trust the local CA** once: `localforest trust` (macOS keychain, asks for your
password), for `https://*.localhost`.

**5. Point the app at the environment** instead of hard-coded settings:
`DATABASE_URL`, `TEST_DATABASE_URL`, `REDIS_URL`, `PORT` (`PHX_HOST` is set when
Phoenix is detected). For Ecto:

```elixir
# config/dev.exs
config :myapp, MyApp.Repo, url: System.fetch_env!("DATABASE_URL"), pool_size: 10
config :myapp, MyAppWeb.Endpoint, http: [port: String.to_integer(System.get_env("PORT", "4000"))]
# config/test.exs
config :myapp, MyApp.Repo, url: System.fetch_env!("TEST_DATABASE_URL"), pool: Ecto.Adapters.SQL.Sandbox
```

**6. Make worktrees** any way you like; each is provisioned the same:

- Claude Code: `claude --worktree`, or subagents with `isolation: worktree` (the
  module's `WorktreeCreate` / `WorktreeRemove` hooks)
- `localforest worktree new fix-login` (prints the path)
- plain `git worktree add .claude/worktrees/fix-login` (the build caches are
  cloned in afterwards)

**7. Work in a worktree without devenv.** Run commands from a shell that has the
project's tools (the primary's `devenv shell`, or the agent's session started in
it) and take the worktree's environment from `localforest env`:

```console
$ cd .claude/worktrees/fix-login && eval "$(localforest env)"
$ mix test
$ open "$LOCALFOREST_URL"      # https://fix-login.web.myapp.localhost, starts web
```

or let the app read the worktree's `.env`. Services started by localforest already
get the project's `PATH` and the worktree's environment.

**8. Language servers for agents:** `localforest.lsp.elixir = [ "dexter" "lsp" ];`
adds `localforest-lsp-elixir`; use it as the command of Claude Code's `lspServers`,
so one session gets answers from the worktree each file belongs to.

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
out). Values are single-quoted, which dotenvy, Ruby/Node dotenv, docker compose, direnv and `set -a; . .env` read literally; a value holding `'` or a line break is double-quoted instead, with `\\ \" \$ \n` escaped and backticks as single-quoted pieces so sourcing it runs nothing (loaders differ there: Node dotenv keeps the backslashes, Ruby dotenv and docker compose don't join quoted pieces, sh reads `\n` literally). Bun expands `$VAR` even in single quotes, so a value containing `$` is logged as a warning. If `.env` isn't gitignored it's added to `.git/info/exclude`; a tracked `.env`
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
| 443 / 80 | HTTPS proxy / redirect to HTTPS where unprivileged processes may bind them (macOS; Linux, see below), else 8443 / off. Ports below 1024 bind all interfaces and refuse non-loopback peers. |

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
- `localforest.postgres.settings.file_copy_method = "copy"` turns database cloning off.

## Linux

Everything works on Linux too (CI runs `tests/e2e.sh` on Ubuntu and macOS), with
these differences:

- **Ports 443 and 80.** Linux only lets root bind ports below
  `net.ipv4.ip_unprivileged_port_start` (1024). localforest reads it: with it at 443
  or lower you get `https://…localhost` on 443 (and the redirect on 80 at 80 or
  lower), otherwise 8443. Lower it once:

  ```sh
  echo 'net.ipv4.ip_unprivileged_port_start = 80' | sudo tee /etc/sysctl.d/50-localforest.conf
  sudo sysctl --system
  ```

  or on NixOS `boot.kernel.sysctl."net.ipv4.ip_unprivileged_port_start" = 80;`.
  (`setcap cap_net_bind_service` doesn't survive a read-only Nix store or rebuilds.)
- **No RAM disk:** PostgreSQL lives in `~/.local/state/localforest/pg`. Databases
  and worktrees are cloned copy-on-write on btrfs / XFS (reflinks). On ext4 and
  other filesystems without them, worktrees are regular checkouts and localforest
  copies the build caches in (mtimes kept); databases are plain copies.
- **CA:** `localforest trust` is macOS only; add `~/.local/state/localforest/ca/ca.pem`
  to the system (`sudo cp … /usr/local/share/ca-certificates/localforest.crt &&
  sudo update-ca-certificates`) and the browser's store (`certutil -d sql:$HOME/.pki/nssdb
  -A -t C,, -n localforest -i …/ca.pem`).
- **GitHub token:** from `GH_TOKEN`, the keyring, or `gh`'s `~/.config/gh/hosts.yml`.

## Development

```sh
devenv shell
cargo test          # tests/lsp_worktrees.rs also runs when `dexter` is in PATH
nix build .#localforest
```
