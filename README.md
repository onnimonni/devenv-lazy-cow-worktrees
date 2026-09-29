# devenv-lazy-cow-worktrees

Local dev infrastructure for many git worktrees at once, in one binary. Made for
coding agents (Claude Code) that give every task its own worktree: each worktree
gets its own services, databases, Redis and HTTPS hostnames, created in
milliseconds, started on demand and cleaned up when its PR merges.

```console
$ lazy-cow-tree worktree new fix-login
https://fix-login.web.myapp.localhost
/src/myapp/.claude/worktrees/fix-login
$ curl https://fix-login.web.myapp.localhost   # starts web (and the worker it depends on)
$ curl https://fix-login.api.myapp.localhost   # starts api (same DATABASE_URL and REDIS_URL)
$ cd .claude/worktrees/fix-login && eval "$(lazy-cow-tree env)"
$ echo $DATABASE_URL $REDIS_URL
postgres://myapp--fix-login:4c1f…@127.0.0.1:55432/myapp_dev_fix_login redis://:myapp--fix-login@127.0.0.1:6380/0
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
PostgreSQL, hashed ports, `worktree-new` / `worktree-rm`) and became lazy-cow-tree:
**devenv is evaluated once, in the primary checkout, and worktrees don't run devenv
at all.** The daemon it starts gives every worktree what devenv would have: a
database (a copy-on-write clone, in milliseconds), Redis, ports, HTTPS hostnames and
the project's services, started on demand; the environment comes from
`lazy-cow-tree env` or the worktree's `.env`.

## What it does

| | |
|---|---|
| **Worktrees** | `git worktree add` without a checkout, then filled with copy-on-write clones of the primary checkout, build caches included ([git-cow](https://github.com/onnimonni/git-cow)): ~0 disk, nothing to recompile. In `.claude/worktrees/<name>`, where Claude Code puts its own. A worktree's name (hostname, databases, role, Redis, port) is its git admin dir's (`.git/worktrees/<name>`, unique per repository); one that isn't a DNS label of at most 32 characters is shortened and gets a hash suffix, so two names never share a worktree. Database and role names are kept within PostgreSQL's 63 bytes the same way (test partitions included). |
| **File watcher** | Watches `.git/worktrees`: a worktree made any way (plain `git`, git-cow, Claude Code) is provisioned; one deleted by hand is cleaned up. |
| **Services** | Every checkout runs the project's services (`lazyCowTree.services`), each with its own port and `https://<worktree>.<service>.<project>.localhost` (`<service>.<project>.localhost` in the primary). The first request to a service starts it, after the services it depends on; workers without http run as dependencies. |
| **PostgreSQL** | One PostgreSQL 18 on an APFS RAM disk, `fsync=off`. lazy-cow-tree is a proxy in front of it: the user in the connection picks the checkout, a database is created on first connect as a copy-on-write clone of its template (always `SET file_copy_method = clone` + `STRATEGY FILE_COPY`: 200 MB in ~40 ms instead of ~450 ms), and a checkout can only open its own databases; users that are no checkout's role are refused. Every checkout has its own role and password. |
| **Redis** | One port; the password picks the checkout's own `redis-server`, on a private unix socket, started on first use and killed with the worktree. Real Redis: pub/sub, Lua, streams, `FLUSHALL` only touch that one. |
| **HTTPS** | Local CA, websockets included. `https://lazy-cow-tree.localhost` lists everything. |
| **GitHub** | Webhook websocket (polling without repo admin rights): pushes pull every branch and merge the base branch into worktrees (conflict-free merges only, dirty worktrees skipped). A worktree whose PR merged is removed unless it has newer work or wasn't made at least 5 minutes before the merge (a new task reusing the branch name; `worktree rm` without `--force` refuses it too). |
| **Migrations** | When the base branch moves: the migrate commands run in the primary checkout, then the template is refreshed from its database; they also run in every worktree the base branch was merged into, and once in each new worktree after its database is cloned (its branch may carry migrations the template lacks; done is recorded per database, so one recreated after a reboot is migrated again). A worktree's services start only once its migrations succeeded; a failure shows in `lazy-cow-tree status` and on its 502 page and is retried with a growing backoff. Migrations don't block creating, syncing or removing other worktrees. A freshly created primary database (first start, after a reboot) is migrated and seeded whatever branch the primary is on, but the template is only made from an up-to-date base branch. Until it exists, worktrees clone the primary's database instead, so they do get the primary's feature-branch migrations (then their own on top). |
| **Gone pages** | A removed worktree's hostnames answer 503 with why it's gone (pull request merged, removed, deleted), links to the PR, branch and commit on GitHub / GitLab / Bitbucket / Gitea, and a button that recreates it as a preview. |
| **LSP proxy** | `lazy-cow-tree lsp -- <server>` runs one language server per worktree, routes each request by file, and drops results from other worktrees. |

Removing a worktree SIGKILLs everything running in it (each service's process group,
plus any process whose working directory or executable is inside it, with all
descendants: the BEAM, esbuild, tailwind, node, …; never the process asking for the
removal or its ancestors, such as the Claude Code session), stops its redis-server, drops
its databases and role, deletes its branch (even `--force` keeps one with commits
that aren't on the base branch, pushed, or in its merged PR, renamed to
`<name>-kept-<sha>`; uncommitted files it deletes are listed) and moves the files away
for background deletion. Gitignored files it deletes that are neither build caches
(including whatever it carried in or `.worktreeinclude` names), its own `.env`, copies of
the primary checkout's, nor unchanged since setup finished are printed as warnings and
listed on its gone page.

## Install

Binaries for macOS (arm64) and Linux (x86_64) are on the
[releases page](https://github.com/onnimonni/devenv-lazy-cow-worktrees/releases); or
`nix profile install github:onnimonni/devenv-lazy-cow-worktrees`, or
`cargo install --git https://github.com/onnimonni/devenv-lazy-cow-worktrees`. lazy-cow-tree runs
`postgres`/`initdb` (18+) and `redis-server` from PATH unless told where they are
(the devenv module does).

## Use with devenv

**1. Import the module** in the primary checkout, as a flake input:

```yaml
# devenv.yaml
inputs:
  lazy-cow-tree:
    url: github:onnimonni/devenv-lazy-cow-worktrees   # don't make it follow your nixpkgs
```

```nix
# devenv.nix
{ inputs, ... }:
{
  imports = [ inputs.lazy-cow-tree.devenvModules.default ];
}
```

or without the flake: `flake: false` on the input and `imports: [ lazy-cow-tree/devenv-module ]`
in `devenv.yaml` (the module then evaluates lazy-cow-tree's pinned nixpkgs itself, a
second nixpkgs evaluation per shell).

Either way lazy-cow-tree is the exact derivation CI pushes to
[lazy-cow-tree.cachix.org](https://lazy-cow-tree.cachix.org), built with its own pinned
nixpkgs, so it is downloaded, not compiled. The module adds the cache with `cachix.pull`; a multi-user Nix (the
default on macOS) only uses it if you are in `trusted-users` (`nix store info` shows
`Trusted: 1`), or add it to the daemon's `nix.conf` yourself:

```text
extra-substituters = https://lazy-cow-tree.cachix.org
extra-trusted-public-keys = lazy-cow-tree.cachix.org-1:gpaT7lT1g7NZo5Na9qKQ5d6g4attS53Lm3A8rtZOjuw=
```

In CI, let [cachix-action](https://github.com/cachix/cachix-action) configure it:

```yaml
- uses: cachix/install-nix-action@v31
- uses: cachix/cachix-action@v16
  with:
    name: lazy-cow-tree          # or your own cache, plus `extraPullNames: lazy-cow-tree`
```

To build it yourself instead: `lazyCowTree.cachix.enable = false;` and
`lazyCowTree.package = pkgs.callPackage (inputs.lazy-cow-tree + "/package.nix") { };`
(your nixpkgs; compiled locally).

**2. Describe the project** in plain devenv. The module reads `processes`,
`services.postgres`, `services.redis` and `dotenv`, and lazy-cow-tree runs them in every
checkout: each process gets its own port and `https://[<worktree>.]<hostname>`, PostgreSQL
and Redis come from the daemon, and devenv doesn't start its own copies (nor its proxy:
lazy-cow-tree serves the hostnames). It also adds lazy-cow-tree to the shell, runs `lazy-cow-tree
serve` as a devenv process, exports the checkout's environment in `enterShell`, and wires
Claude Code's worktree hooks and its trust in the local CA.

```nix
# devenv.nix
{ pkgs, config, inputs, ... }:
{
  imports = [ inputs.lazy-cow-tree.devenvModules.default ];

  services.postgres = {
    enable = true;
    package = pkgs.postgresql_18;                 # 18+ for copy-on-write databases
    initialDatabases = [ { name = "myapp_dev"; } { name = "myapp_test"; } ];
  };
  services.redis.enable = true;

  processes.web = {
    exec = "mix phx.server";
    ports.http.allocate = 4000;
    env.PORT = toString config.processes.web.ports.http.value;   # per checkout
    proxy.hostname = "web.myapp.localhost";
    after = [ "devenv:processes:worker@started" ];
    ready.http.get = { port = config.processes.web.ports.http.value; path = "/"; };
  };
  processes.worker.exec = "mix run --no-halt";

  lazyCowTree.migrate = "mix ecto.migrate";
  lazyCowTree.setup = "mix deps.get";
}
```

How devenv's options map:

| devenv | lazy-cow-tree |
|---|---|
| `processes.<name>.exec` | run as a bash script, in the checkout (a path under the primary's root is the checkout's) |
| `cwd` | relative to the checkout |
| `ports.http` (or the only port) | `$PORT`, the service's hostname; env entries holding a port's value are replaced by the checkout's port |
| other `ports.<p>` | named port, in the variable that held its value (else `<P>_PORT`); `http` if it has its own `proxy.hostname` |
| `proxy.hostname` | hostname in the primary; worktrees get `<worktree>.` in front |
| `after = [ "devenv:processes:<x>" ]` | `dependsOn`, when lazy-cow-tree runs `<x>` too; other entries are ignored with a warning |
| `ready.http.get.path`, `ready.timeout` | a first request waits for this probe (200–399), default 60 s |
| `restart.on`, `watch.paths` | `restart`, `restartOnChange` |
| `services.postgres.{package,extensions,settings}` | the daemon's PostgreSQL |
| `services.postgres.initialDatabases` | databases per checkout |
| `services.redis.package` | the daemon's Redis |
| `dotenv.filename` | the checkout's own env files (see below) |

When to start them, per process and for PostgreSQL / Redis:

```nix
processes.web.start = {
  on = "demand";            # "up" | "demand" (default) | "manual"
  idleTimeout = "15m";      # stop after 15 min without open connections; null (default) = never
};
services.postgres.start = { on = "demand"; idleTimeout = null; };   # default on = "up"
services.redis.start = { on = "demand"; idleTimeout = "30m"; };     # default on = "demand"
```

`up` starts it with its checkout, `demand` on the first request (or connection, or as a
dependency), `manual` only with `lazy-cow-tree service start`. A `demand` process nothing
can start (no http port, nothing depends on it) is an evaluation error. Websockets count
as open connections, so an open LiveView tab keeps its server up.

Databases:

```nix
services.postgres = {
  instance = "shared";                  # one cluster, databases per checkout (default)
                                        # "unique": a cluster of its own per checkout
  copyOnWrite = {
    enable = true;                      # default: instance == "shared"
    refresh = "on-base-change";         # or "manual" (`lazy-cow-tree snapshot`)
  };
  dangerouslyDisableDurabilityForSpeed = { enable = false; ramdiskSize = "4G"; };
};
services.redis.instance = "unique";     # a redis-server per checkout (default), or "shared"
```

With `copyOnWrite`, a worktree's databases are copy-on-write clones of the primary's
(milliseconds, next to no disk; any filesystem that clones files: APFS, btrfs, XFS),
refreshed from the primary when the base branch moves and its migrations ran. Without
it (and always with `instance = "unique"`) they are created empty, then migrated and
seeded. Setting both `instance = "unique"` and `copyOnWrite.enable = true` is an error.

`dangerouslyDisableDurabilityForSpeed` runs PostgreSQL on a RAM disk with `fsync`,
`synchronous_commit` and `full_page_writes` off. **Every database is lost on a reboot,
on `lazy-cow-tree down --eject`, and on a crash, which can also corrupt the cluster.**
Only for data you can recreate (migrations and seeds). Off by default: the cluster is
then on disk with PostgreSQL's normal durability.

A checkout's own variables go in `.env.local`, or devenv's `dotenv.filename` when
`dotenv.enable` is on: the file is read from the checkout on every start of anything
in it and applied last. A new worktree starts with a copy of the primary's; `.env`
itself is lazy-cow-tree's output (below).

Escape hatches, all optional:

```nix
processes.legacy.lazyCowTree.enable = false;   # leave this process to devenv (primary only)
processes.web.lazyCowTree = { migrate = "mix ecto.migrate"; restartOnPull = true; };
lazyCowTree.services.web.portOffset = 0;       # any lazyCowTree.services.<name> field wins
lazyCowTree.envFiles = [ ".env.local" ".env.secrets" ];
```

The `lazyCowTree.services` vocabulary below still works on its own, for processes
devenv doesn't know about.

**3. Start it** with `devenv up` in the primary checkout. That starts the daemon, or
registers the project with the one already running: one daemon serves every
project, and another project's `devenv up` takes over if it stops.

**4. Trust the local CA** once: `lazy-cow-tree trust` (macOS keychain, asks for your
password), for `https://*.localhost`. Node ignores the keychain, so the module also
sets `NODE_EXTRA_CA_CERTS` to the CA (`$LAZY_COW_TREE_HOME/ca/ca.pem`, default
`~/.local/state/lazy-cow-tree/ca/ca.pem`) in `.claude/settings.local.json`'s `env`:
Claude Code then connects to MCP servers behind lazy-cow-tree (e.g. Tidewave at
`https://web.<project>.localhost/tidewave/mcp`) instead of failing with
`SELF_SIGNED_CERT_IN_CHAIN`. Node takes a single file there: to trust another CA too,
set `files.".claude/settings.local.json".json.env.NODE_EXTRA_CA_CERTS` to a bundle
of both (yours wins), or turn it off with `lazyCowTree.claude.trustCa = false`.
Codex uses the system roots, so `lazy-cow-tree trust` covers it; elsewhere point
`CODEX_CA_CERTIFICATE` (added to the system roots) at the CA, not `SSL_CERT_FILE`
(replaces them).

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
- `lazy-cow-tree worktree new fix-login` (prints the path)
- plain `git worktree add .claude/worktrees/fix-login` (the build caches are
  cloned in afterwards)

**7. Work in a worktree without devenv.** Run commands from a shell that has the
project's tools (the primary's `devenv shell`, or the agent's session started in
it) and take the worktree's environment from `lazy-cow-tree env`:

```console
$ cd .claude/worktrees/fix-login && eval "$(lazy-cow-tree env)"
$ mix test
$ open "$LAZY_COW_TREE_URL"      # https://fix-login.web.myapp.localhost, starts web
```

or let the app read the worktree's `.env`. Services started by lazy-cow-tree already
get the project's `PATH` and the worktree's environment.

**8. Language servers for agents:** `lazyCowTree.lsp.elixir = [ "dexter" "lsp" ];`
adds `lazy-cow-tree-lsp-elixir`; use it as the command of Claude Code's `lspServers`,
so one session gets answers from the worktree each file belongs to.

| option | default | |
|---|---|---|
| `lazyCowTree.project` | directory name | hostnames, database prefix, role name |
| `lazyCowTree.port` | `4000` | base port of the primary checkout's services |
| `lazyCowTree.migrate` | none | migrate command: primary when the base branch moves (then the template is refreshed) or its database was just created, new worktrees once, worktrees the base branch was merged into |
| `lazyCowTree.seed` | none | seed command: primary, after `migrate`, when its database was just created; worktrees get seeded data via the template |
| `lazyCowTree.setup` | none | runs once in every new checkout (lazy-cow-tree, `git worktree add`, Claude Code), e.g. `mix deps.get`; in the primary checkout too (a fresh clone has no `deps/`), before its first migrate, seed or service start. Done is a `lazy-cow-tree-setup` marker in the checkout's git dir; a failure in the primary shows in `lazy-cow-tree status` and is retried with the migrations' backoff; again before a `restartOnChange` restart for changed dependency files, so keep it idempotent |
| `lazyCowTree.services.<name>` | none | see below |
| `lazyCowTree.server` | none | shorthand for `lazyCowTree.services.web.exec` |
| `lazyCowTree.previewTtlHours` | `48` | close previews after this many hours without activity; `0` keeps them |
| `lazyCowTree.httpsPort` | `null` | HTTPS proxy port; unset: 443 where unprivileged processes may bind it, else 8443 |
| `lazyCowTree.httpPort` | `null` | HTTP port redirecting to HTTPS, 0 disables; unset: 80 where unprivileged processes may bind it, else off |
| `lazyCowTree.lsp.<name>` | none | adds `lazy-cow-tree-lsp-<name>` for Claude Code's `lspServers` |
| `lazyCowTree.package` | built with lazy-cow-tree's pinned nixpkgs | lazy-cow-tree build; the default is on lazy-cow-tree.cachix.org |
| `lazyCowTree.cachix.enable` | `true` | `cachix.pull = [ "lazy-cow-tree" ]` |
| `lazyCowTree.claude.trustCa` | `true` | sets `NODE_EXTRA_CA_CERTS` to the local CA in `.claude/settings.local.json` (a value you set there wins) |
| `lazyCowTree.home` | `$LAZY_COW_TREE_HOME`, else `~/.local/state/lazy-cow-tree` | where the module looks for the daemon's CA (read from devenv's environment at evaluation; `env.LAZY_COW_TREE_HOME` too) |
| `lazyCowTree.postgres.package` | `pkgs.postgresql_18` | PostgreSQL build (18+ for copy-on-write databases) |
| `lazyCowTree.postgres.extensions` | none | as in devenv: `extensions: [ extensions.postgis extensions.pgvector ]`; trusted ones: enable with `CREATE EXTENSION` |
| `lazyCowTree.postgres.createExtensions` | `[]` | created as superuser in `template1` (so every database made afterwards) and the primaries' databases, for untrusted extensions checkout roles can't create, e.g. `[ "postgis" "vector" ]`; migrations' `CREATE EXTENSION IF NOT EXISTS` is then a no-op |
| `lazyCowTree.postgres.settings` | `{}` | extra postgresql.conf settings, e.g. `shared_preload_libraries` |
| `lazyCowTree.postgres.ramdiskMB` | `4096` | RAM disk size (used as it fills); resizing needs `lazy-cow-tree down --eject`, which empties every database |
| `lazyCowTree.redis` | `pkgs.redis` | Redis build |
| `lazyCowTree.envFiles` | `dotenv.filename` if `dotenv.enable`, else `[ ".env.local" ]` | the checkout's own env files, relative to it |

Service options:

| | default | |
|---|---|---|
| `exec` | | command |
| `cwd` | checkout root | working directory, relative to the checkout |
| `http` | `true` | listens on `$PORT`, gets a hostname, started by its first request; `false`: only started as a dependency |
| `default` | `web`, else first http service | what `lazy-cow-tree env` / `lazy-cow-tree service` pick without a name |
| `portOffset` | position by name | `PORT` = checkout base port + offset (0–9) |
| `migrate` | none | migrate/seed command for the checkout's database, run in its `cwd` after `lazyCowTree.migrate` |
| `dependsOn` | `[]` | started first |
| `env` | `{}` | extra environment |
| `restart` | `"no"` | when it exits on its own: `"no"`, `"on-failure"` (non-zero exit) or `"always"`; backs off 1–30 s, `lazy-cow-tree service stop` keeps it down |
| `restartOnPull` | `false` | restart it (if running) after the base branch was pulled into its checkout and migrated; for servers without a code reloader |
| `restartOnChange` | Mix: `[ "mix.exs" "mix.lock" "config/*.exs" ]`, else `[]` | files (relative to `cwd`, `*` / `?` in the file name) whose content changing restarts it if running; see below |
| `ports.<name>` | `{}` | further ports it listens on; see below |
| `start` | `"demand"` | `"up"`, `"demand"` or `"manual"` |
| `idleTimeout` | `null` | seconds without open connections before it's stopped |
| `ready` | `null` | `{ path; timeout; }`: HTTP probe a first request waits for |
| `hostname` | `<service>.<project>.localhost` | hostname in the primary; worktrees prefix `<worktree>.` |

Commands are split like a shell would, then run directly (no shell) with the
service's environment and the project's `PATH`. Logs: `lazy-cow-tree service log -s <name>`.

A running service is restarted when the content of a `restartOnChange` file changes
(a pull, a dependency update, an agent's edit), once nothing in its checkout changed for
a second, and not while the checkout pulls or migrates (then after). When a dependency
manifest or lockfile changed (`mix.exs`, `mix.lock`, `Gemfile(.lock)`, `package.json`,
`bun.lock`, `Cargo.lock`, `go.mod`, `pyproject.toml`, `uv.lock`, `composer.lock`, …),
`lazyCowTree.setup` runs in the checkout first, in the background, once for all its
services, so it should be idempotent (`mix deps.get`, not an alias that also seeds). For
a command running `mix` (`mix phx.server`, `iex -S mix …`, `sh -c '… mix phx.server'`)
it defaults to `mix.exs`, `mix.lock` and `config/*.exs`: Phoenix's code reloader
refuses to compile after those change until the server restarts. Others opt in, e.g.
Rails with `restartOnChange = [ "Gemfile.lock" "config/*.rb" ]`; `[ ]` turns it off.

Named ports, for a service that listens on more than `$PORT` (a debugger, a test
endpoint), so the app reads a variable instead of computing an offset:

```nix
lazyCowTree.services.web = {
  exec = "mix phx.server";
  ports = {
    debugger = { env = "LIVE_DEBUGGER_PORT"; http = true; };  # https://[<worktree>.]debugger.<project>.localhost
    test.env = "TEST_PORT";                                   # no hostname
  };
};
```

| | default | |
|---|---|---|
| `env` | `<NAME>_PORT` | variable with the port, in every environment of the checkout (plus `LAZY_COW_TREE_<SERVICE>_<NAME>_PORT`, and `_URL` with `http`) |
| `http` | `false` | gets `https://[<worktree>.]<name>.<project>.localhost`; its first request starts the service and waits for this port |
| `offset` | highest free | port = checkout base port + offset (0–9) |

They share the checkout's 10-port block with the services: services keep their
offsets, the rest are filled from the top down (9, 8, …) by service and port name.
With only `web` on base 4000: web 4000, debugger 4009, test 4008. Adding a port or
service can shift the others, so give `offset` to any port whose number is written
down anywhere instead of read from its variable. `env` may not name a variable
lazy-cow-tree sets (`PORT`, `DATABASE_URL`, `PG*`, `REDIS_URL`, `PHX_HOST`, …,
`LAZY_COW_TREE_*`). An `http` port's first request waits 5 s for it once the service's
main port listens, then answers 502.

## Environment

`lazy-cow-tree env [-s <service>]` prints (all derived from the checkout's path, no daemon
needed), e.g. for services `web` (default), `api` and
`worker`:

| | primary | worktree `fix-login` |
|---|---|---|
| `PORT` | base (`lazyCowTree.port`) + offset | base (20000–28990: hashed from the name, else the next slot no other worktree has; recorded in its git admin dir as `lazy-cow-tree-port`) + offset |
| `LAZY_COW_TREE_URL` | `web.myapp.localhost` | `fix-login.web.myapp.localhost` |
| `DATABASE_URL`, `PG*` | `myapp_dev` as role `myapp` | `myapp_dev_fix_login` as role `myapp--fix-login` |
| `TEST_DATABASE_URL` | `myapp_test` | `myapp_test_fix_login` |
| `REDIS_URL` | password `myapp` | password `myapp--fix-login` |
| `LAZY_COW_TREE_<SERVICE>_URL`, `_PORT` | every service's | every service's |
| named ports' `env`, `LAZY_COW_TREE_<SERVICE>_<NAME>_PORT`, `_URL` | every service's | every service's |
| `LAZY_COW_TREE_SERVICE`, `LAZY_COW_TREE_WORKTREE`, `LAZY_COW_TREE_PROJECT` | | |
| `NODE_EXTRA_CA_CERTS` | the local CA | |

Worktree roles were `<project>-<worktree>` before; the daemon renames an old role to
the new name on its next start (or, if that name was shared by two checkouts, makes
the new role a member of it). The Redis password and the role changed, so restart
anything a worktree runs by hand with an old `.env` / `lazy-cow-tree env`.

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
works through the proxy); only the dev database is cloned from the template. A checkout
owns its dev and test databases and partitions named `<test db><N>` (Ecto's usual
`System.get_env("TEST_DATABASE_URL") <> System.get_env("MIX_TEST_PARTITION", "")`:
`myapp_test_fix_login2`) or `<prefix>_test<N>_<worktree>` (`myapp_test2_fix_login`).
`<test db><N>` is ambiguous when a worktree is named like another plus digits (`x2`
vs `x` + 2): the exact name, then the longest test database, wins among existing
checkouts. An equal claim (project `shop` worktree `dev-x` and project `shop-dev`
worktree `x` both get `shop_dev_dev_x`) is nobody's: refused, never dropped. Removing
a checkout drops only databases its role created.

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
is left alone. Then `lazyCowTree.setup` runs once in it.

## Removed worktrees and previews

lazy-cow-tree remembers removed worktrees in the main checkout's git dir
(`.git/lazy-cow-tree/worktrees.json`): branch, commit, when, and why. A request to one of
their hostnames gets a 503 page saying so (e.g. "Its pull request #42 was merged"),
with links to the pull request, branch and commit on the remote's forge.

Its **Recreate worktree to preview** button (a POST from the page itself) brings the
worktree back at the commit it was at (fetched from the remote or the pull request's
head if it's no longer local), with a fresh copy of the template database, and sends
you back to the page, whose service then starts on demand. A preview isn't
auto-removed for its merged pull request; it closes after `lazyCowTree.previewTtlHours`
(48) without requests or database / Redis connections, unless it has uncommitted
changes or commits that are neither pushed nor in its merged pull request (a pushed
branch loses nothing, merged or not), and the page shows the
original reason again.

## Commands

```sh
lazy-cow-tree serve                        # daemon (devenv process)
lazy-cow-tree env [-s <service>] [--json]  # this checkout's environment
lazy-cow-tree worktree new <name> [--base <ref>]
lazy-cow-tree worktree rm <name> [--force] # without --force only when nothing would be lost
lazy-cow-tree worktree list
lazy-cow-tree service start|stop|restart|log [-s <service>] [<worktree> | .]
lazy-cow-tree status                       # projects, worktrees, services, databases
lazy-cow-tree sync                         # pull, merge, remove merged, migrate now
lazy-cow-tree snapshot                     # template := the primary's dev database
lazy-cow-tree trust                        # trust the local CA
lazy-cow-tree down [--eject]               # stop the daemon; --eject drops the RAM disk
lazy-cow-tree lsp -- <server> [args]
```
## Ports

| | |
|---|---|
| 55432 | PostgreSQL proxy (the real server: `~/.local/state/lazy-cow-tree/pg/.s.PGSQL.55433`) |
| 6380 | Redis proxy |
| 443 / 80 | HTTPS proxy / redirect to HTTPS where unprivileged processes may bind them (macOS; Linux, see below), else 8443 / off. Ports below 1024 bind all interfaces and refuse non-loopback peers. |

All configurable (`--pg-port`, `--redis-port`, `--https-port`, `--http-port` or
`LAZY_COW_TREE_*`). State lives in `~/.local/state/lazy-cow-tree` (`LAZY_COW_TREE_HOME`).

## Projects that use devenv's own proxy

A devenv project with `process.proxy.enable` normally starts devenv's shared
`devenv-proxy` on ports 80 and 443, which lazy-cow-tree already holds. The daemon
answers on devenv's control socket instead (`$DEVENV_PROXY_SOCKET`, else
`$XDG_RUNTIME_DIR/devenv/proxy.sock`, else `$TMPDIR/devenv-proxy-<user>.sock`), so
that project's `devenv up` finds a running proxy and registers its hostnames here:
its processes are served over HTTPS with the project's own mkcert certificate and
over plain HTTP, like with devenv's proxy. Nothing changes in that project.

- To have `devenv up` start lazy-cow-tree when nothing runs yet, point devenv at it
  instead of its own proxy: `DEVENV_PROXY_BINARY=<lazy-cow-tree>/bin/lazy-cow-tree-devenv-proxy`
  (e.g. `home.sessionVariables` in home-manager). It runs the daemon with the ports
  and socket devenv asks for, the proxy up before PostgreSQL. Without it, start
  lazy-cow-tree first: a `devenv-proxy` that is already running keeps the socket,
  and the daemon logs that devenv projects keep their own proxy.
- Hostnames lazy-cow-tree serves can't be registered by a devenv project, and the
  other way around.
- devenv expects the proxy on `127.0.0.1:80` (plain HTTP, its health check) and
  `127.0.0.1:443`. With other ports, set `DEVENV_PROXY_LISTEN` and
  `DEVENV_PROXY_HTTPS_LISTEN` for devenv to match; with `--http-port 0` the socket
  stays off.
- `--devenv-proxy-socket <path>` (`LAZY_COW_TREE_DEVENV_PROXY_SOCKET`) picks another
  socket; `off` disables it.
- Routes live in memory: after a daemon restart, run `devenv up` again in those
  projects.

## Notes

- The RAM disk is volatile: a reboot or `lazy-cow-tree down --eject` empties every
  database; the next start seeds the primary again through the migrate command.
- Checkout roles are not superusers: `CREATEDB` only. A checkout owns its
  databases (`mix ecto.create` / `ecto.drop` work) and everything in its cloned dev
  database: the template's objects belong to a no-login role named after it
  (`<prefix>_template`), handed to the worktree's role on clone. It can't drop or alter
  other checkouts' databases or roles, run programs (`COPY ... TO PROGRAM`) or read
  server files; which databases it can open is enforced by the proxy, which also
  refuses a database another checkout's role created under its name. Trusted
  extensions (`pgcrypto`, `citext`, `pg_trgm`, `hstore`, `uuid-ossp`, ...) are created
  by the app as usual; others (`postgis`, `vector`) need
  `lazyCowTree.postgres.createExtensions`. What still needs a superuser: disabling
  constraint triggers (Rails fixtures' `disable_referential_integrity` warns) and
  `COMMENT ON EXTENSION` for pre-created extensions in a `structure.sql`. Keep
  `dblink` / `postgres_fdw` out of `createExtensions`: they connect past the proxy.
- This boundary covers SQL through the proxy only. The real server's socket
  (`~/.local/state/lazy-cow-tree/pg`) trusts `postgres` without a password: any local
  process that finds it is a superuser. It isolates checkouts from each other (agents,
  tools), not from other programs running as you.
- On start the daemon revokes `SUPERUSER` from every role but `postgres` (checkout
  roles used to be superusers), and on provisioning hands a checkout's role the
  databases it owns by name that an unregistered role made (its role under an older
  naming scheme), with their objects; event triggers, which need a superuser owner,
  go to `postgres`. If that fails, provisioning fails and is retried a minute later.
  Handing over locks every object of a database in one transaction: with many
  thousands of objects raise `max_locks_per_transaction` in
  `lazyCowTree.postgres.settings` if it runs out of shared memory.
- macOS has no API for RAM disks, so `hdiutil`/`diskutil` are run for it; PostgreSQL,
  redis-server, the migrate and service commands and language servers are also
  separate processes. Everything else (git, GitHub, certificates, keychain) is
  in-process.
- `lazyCowTree.postgres.settings.file_copy_method = "copy"` turns database cloning off.

## Linux

Everything works on Linux too (CI runs `tests/e2e.sh` on Ubuntu and macOS), with
these differences:

- **Ports 443 and 80.** Linux only lets root bind ports below
  `net.ipv4.ip_unprivileged_port_start` (1024). lazy-cow-tree reads it: with it at 443
  or lower you get `https://…localhost` on 443 (and the redirect on 80 at 80 or
  lower), otherwise 8443. Lower it once:

  ```sh
  echo 'net.ipv4.ip_unprivileged_port_start = 80' | sudo tee /etc/sysctl.d/50-lazy-cow-tree.conf
  sudo sysctl --system
  ```

  or on NixOS `boot.kernel.sysctl."net.ipv4.ip_unprivileged_port_start" = 80;`.
  (`setcap cap_net_bind_service` doesn't survive a read-only Nix store or rebuilds.)
- **No RAM disk:** PostgreSQL lives in `~/.local/state/lazy-cow-tree/pg`. Databases
  and worktrees are cloned copy-on-write on btrfs / XFS (reflinks). On ext4 and
  other filesystems without them, worktrees are regular checkouts and lazy-cow-tree
  copies the build caches in (mtimes kept); databases are plain copies.
- **CA:** `lazy-cow-tree trust` is macOS only; add `~/.local/state/lazy-cow-tree/ca/ca.pem`
  to the system (`sudo cp … /usr/local/share/ca-certificates/lazy-cow-tree.crt &&
  sudo update-ca-certificates`) and the browser's store (`certutil -d sql:$HOME/.pki/nssdb
  -A -t C,, -n lazy-cow-tree -i …/ca.pem`).
- **GitHub token:** from `GH_TOKEN`, the keyring, or `gh`'s `~/.config/gh/hosts.yml`.

## Development

```sh
devenv shell
cargo test          # tests/lsp_worktrees.rs also runs when `dexter` is in PATH
nix build .#lazy-cow-tree
```
