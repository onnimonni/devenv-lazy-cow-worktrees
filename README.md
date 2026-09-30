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
$ cd .claude/worktrees/fix-login          # in the devenv shell: its environment follows
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
the project's services, started on demand; shells started from the devenv shell
get the environment of the worktree they're in.

## What it does

| | |
|---|---|
| **Worktrees** | `git worktree add` without a checkout, then filled with copy-on-write clones of the primary checkout, build caches included ([git-cow](https://github.com/onnimonni/git-cow)): ~0 disk, nothing to recompile. In `.claude/worktrees/<name>`, where Claude Code puts its own. A worktree's name (hostname, databases, role, Redis, port) is its git admin dir's (`.git/worktrees/<name>`, unique per repository); one that isn't a DNS label of at most 32 characters is shortened and gets a hash suffix, so two names never share a worktree. Database and role names are kept within PostgreSQL's 63 bytes the same way (test partitions included). |
| **Two binaries** | `lazy-cow-tree-cow` fills new worktrees (run by the devenv module's `git` wrapper, no daemon needed); `lazy-cow-tree` is the daemon (HTTPS, PostgreSQL and Redis proxies, services, migrations, GitHub sync) and its CLI. |
| **Worktrees, however made** | `lazy-cow-tree worktree new`, `git worktree add` in the devenv shell (the module's `git` wrapper) and Claude Code (its WorktreeCreate hook) all make copy-on-write worktrees. The daemon provisions new worktrees and cleans up removed ones when the wrapper or a hook asks, on start and every minute. |
| **Services** | Every checkout runs the project's services (`lazyCowTree.services`), each with its own port and `https://<worktree>.<service>.<project>.localhost` (`<service>.<project>.localhost` in the primary). The first request to a service starts it, after the services it depends on; workers without http run as dependencies. |
| **PostgreSQL** | One PostgreSQL 18 on an APFS RAM disk, `fsync=off`. lazy-cow-tree is a proxy in front of it: the user in the connection picks the checkout, a database is created on first connect as a copy-on-write clone of its template (always `SET file_copy_method = clone` + `STRATEGY FILE_COPY`: 200 MB in ~40 ms instead of ~450 ms), and a checkout can only open its own databases; users that are no checkout's role are refused. Every checkout has its own role and password. |
| **Redis** | One port; the password picks the checkout's own `redis-server`, on a private unix socket, started on first use and killed with the worktree. Real Redis: pub/sub, Lua, streams, `FLUSHALL` only touch that one. |
| **HTTPS** | Local CA, websockets included. `https://lazy-cow-tree.localhost` lists everything. |
| **GitHub** | Webhook websocket (polling without repo admin rights): pushes pull every branch and merge the base branch into worktrees (conflict-free merges only, dirty worktrees skipped). A worktree whose PR merged is removed unless it has newer work or wasn't made at least 5 minutes before the merge (a new task reusing the branch name; `worktree rm` without `--force` refuses it too); `lazyCowTree.autoRemoveMerged = false` keeps them. |
| **Migrations** | When the base branch moves: the migrate commands run in the primary checkout, then the template is refreshed from its database; they also run in every worktree the base branch was merged into, and once in each new worktree after its database is cloned (its branch may carry migrations the template lacks; done is recorded per database, so one recreated after a reboot is migrated again). A worktree's services start only once its migrations succeeded; a failure shows in `lazy-cow-tree status` and on its 502 page and is retried with a growing backoff. Migrations don't block creating, syncing or removing other worktrees. A freshly created primary database (first start, after a reboot) is migrated and seeded whatever branch the primary is on, but the template is only made from an up-to-date base branch. Until it exists, worktrees clone the primary's database instead, so they do get the primary's feature-branch migrations (then their own on top). |
| **LSP proxy** | `lazy-cow-tree lsp -- <server>` runs one language server per worktree, routes each request by file, and drops results from other worktrees. Wrapped in the devenv shell and given to Claude Code, Codex and pi (dexter, typescript-language-server, pyright and rust-analyzer for now). |

Removing a worktree SIGKILLs everything running in it (each service's process group,
plus any process whose working directory or executable is inside it, with all
descendants: the BEAM, esbuild, tailwind, node, …; never the process asking for the
removal or its ancestors, such as the Claude Code session), stops its redis-server, drops
its databases and role, deletes its branch (even `--force` keeps one with commits
that aren't on the base branch, pushed, or in its merged PR, renamed to
`<name>-kept-<sha>`; uncommitted files it deletes are listed) and moves the files away
for background deletion. Gitignored files it deletes that are neither build caches
(including whatever it carried in or `.worktreeinclude` names), copies of the
primary checkout's, nor unchanged since setup finished are printed as warnings.

## Install

Binaries (`lazy-cow-tree` and `lazy-cow-tree-cow`) for macOS (arm64) and Linux (x86_64,
arm64) are on the
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
`services.postgres` and `services.redis`, and lazy-cow-tree runs them in every
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

When to start them, per process and for PostgreSQL / Redis:

```nix
processes.web.start = {
  on = "demand";            # "up" | "demand" | "manual"; default: "demand" with an http
                            # port or another process's `after` on it, else "up"
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

Escape hatches, all optional:

```nix
processes.legacy.lazyCowTree.enable = false;   # leave this process to devenv (primary only)
processes.web.lazyCowTree = { migrate = "mix ecto.migrate"; restartOnPull = true; };
lazyCowTree.services.web.portOffset = 0;       # any lazyCowTree.services.<name> field wins
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
- `git worktree add .claude/worktrees/fix-login` in the devenv shell (the module's
  `git` wrapper makes it copy-on-write)

**7. Work in a worktree without devenv.** Start the agent from the primary's
`devenv shell` (`devenv shell -- claude`, `devenv shell -- codex`, `devenv shell -- pi`).
Every bash and zsh started from it, including Claude Code's, Codex's and pi's tool
shells and their subagents',
gets the environment of the checkout it runs in, and again after `cd`/`pushd`/`popd`,
with nothing to tell the agent:

```console
$ cd .claude/worktrees/fix-login && mix test   # myapp_test_fix_login
$ echo $PHX_HOST $DATABASE_URL                 # fix-login.web.myapp.localhost …/myapp_dev_fix_login
$ open "$LAZY_COW_TREE_URL"                    # https://fix-login.web.myapp.localhost, starts web
```

Another project the daemon registered gets its own; outside them, values it
overrode are restored and the rest unset. In a worktree, DEVENV_ROOT, DEVENV_STATE and every other variable
of the devenv shell that names a path in the primary point into the worktree instead, as
for the services run there, so scripts using `$DEVENV_ROOT` act on the checkout you're in. How: `BASH_ENV` (every non-interactive
bash) and `ZDOTDIR` (every zsh; Codex runs commands with `zsh -lc`) source a hook
that defines the `cd`/`pushd`/`popd` wrappers and runs `lazy-cow-tree shell-hook`
(hidden command, ~10 ms), which also records in `LAZY_COW_TREE_SHELL` what it set. Your own
`BASH_ENV` and zsh startup files (from your `ZDOTDIR`, else `$HOME`) still run.
`codex` is wrapped to run with `--no-daemon`: a shared `codex app-server` started
elsewhere would run commands with its own environment. Not covered: `builtin cd`,
fish. Turn it off with `lazyCowTree.shellHook.enable = false` (then `enterShell`
only exports the environment of the checkout it starts in) and
`lazyCowTree.codex.noDaemon = false`.

Services started by lazy-cow-tree already get the project's `PATH` and the
worktree's environment.

**8. Language servers for agents.** One agent session edits files in many worktrees; a
single language server rooted at the primary would answer from the wrong one. For now
four are supported, taken from the enabled `languages.*` with their LSP on:

```nix
languages.elixir = { enable = true; lsp.package = pkgs.dexter; };  # dexter lsp
languages.typescript.enable = true;                                 # typescript-language-server --stdio
languages.python.enable = true;                                     # pyright-langserver --stdio (pyright)
languages.rust.enable = true;                                       # the toolchain's rust-analyzer
```

Like the `git` wrapper, each becomes a same-named wrapper in the devenv shell: started
as a server, it runs behind `lazy-cow-tree lsp` (one server per worktree, rooted there;
each request goes to the worktree of its file, results from other worktrees dropped);
other uses (`--version`) run the real binary. `LAZY_COW_TREE_LSP_DISABLE=1` always
runs the real one.

- **Claude Code** runs language servers from plugins only: the module adds a local
  plugin marketplace (in the Nix store) and enables its plugin `lazy-cow-tree-lsp` in
  `.claude/settings.local.json` (`lazyCowTree.claude.lsp`), whose servers are the
  wrappers. Claude Code's own plugins that start the same server by name from `PATH`
  reach the wrapper too.
- **Codex** has no LSP client: with `lazyCowTree.codex.lsp` each server is also an MCP
  server `lsp-<binary>` in the project's `.codex/config.toml` (read for trusted
  projects) through [mcp-language-server](https://github.com/isaacphi/mcp-language-server)
  (definition, references, diagnostics, hover, rename, edits). The wrapper is given by
  path: Codex starts MCP servers with only `PATH`, `HOME` and a few more variables. Your
  own Codex settings go in `files.".codex/config.toml".toml`.
- **pi** has no LSP client either: with `lazyCowTree.pi.lsp` the same MCP servers go in
  the project's `.pi/mcp.json` (read once the project is trusted). It's a writable copy
  that `devenv shell` rewrites, since pi saves `/mcp` changes (enable, exposure) into it;
  your own pi servers go in `files.".pi/mcp.json".json.mcpServers`.

FIXME: other language servers. devenv's `languages.*.lsp` has only `enable` and
`package`, not the arguments a server starts with nor its file extensions
(cachix/devenv#3202); Helix's `languages.toml` has both for most servers.

| option | default | |
|---|---|---|
| `lazyCowTree.project` | directory name | hostnames, database prefix, role name |
| `lazyCowTree.port` | `4000` | base port of the primary checkout's services; taken by another project or process, the next free block of 10 above it (recorded in `.git/lazy-cow-tree-primary-port`), or an error with devenv's `strict_ports: true` |
| `lazyCowTree.migrate` | none | migrate command: primary when the base branch moves (then the template is refreshed) or its database was just created, new worktrees once, worktrees the base branch was merged into |
| `lazyCowTree.seed` | none | seed command: primary, after `migrate`, when its database was just created; worktrees get seeded data via the template |
| `lazyCowTree.setup` | none | runs once in every new checkout (lazy-cow-tree, `git worktree add`, Claude Code), e.g. `mix deps.get`; in the primary checkout too (a fresh clone has no `deps/`), before its first migrate, seed or service start. Done is a `lazy-cow-tree-setup` marker in the checkout's git dir; a failure in the primary shows in `lazy-cow-tree status` and is retried with the migrations' backoff; again before a `restartOnChange` restart for changed dependency files, so keep it idempotent |
| `lazyCowTree.services.<name>` | none | see below |
| `lazyCowTree.server` | none | shorthand for `lazyCowTree.services.web.exec` |
| `lazyCowTree.worktreesDir` | `.claude/worktrees` | where new worktrees go, relative to the primary; `../myapp-worktrees` keeps them out of the primary's language servers and indexers |
| `lazyCowTree.autoRemoveMerged` | `true` | remove worktrees whose GitHub PR merged (see GitHub above) |
| `lazyCowTree.httpsPort` | `null` | HTTPS proxy port; unset: 443 where unprivileged processes may bind it, else 8443 |
| `lazyCowTree.httpPort` | `null` | HTTP port redirecting to HTTPS, 0 disables; unset: 80 where unprivileged processes may bind it, else off |
| `lazyCowTree.package` | built with lazy-cow-tree's pinned nixpkgs | lazy-cow-tree build; the default is on lazy-cow-tree.cachix.org |
| `lazyCowTree.cachix.enable` | `true` | `cachix.pull = [ "lazy-cow-tree" ]` |
| `lazyCowTree.shellHook.enable` | `true` | every bash/zsh from the devenv shell (agents' tool shells) gets its checkout's environment, again after `cd` (step 7) |
| `lazyCowTree.git.enable` | `true` | `git` in the shell is a wrapper: `git worktree add` fills the worktree like `lazy-cow-tree worktree new` (copy-on-write clones of the primary, build caches included; locked as initializing meanwhile); `LAZY_COW_TREE_GIT_DISABLE=1` for plain git. Don't also import git-cow's module |
| `lazyCowTree.git.package` | `pkgs.git` | the real git it runs |
| `lazyCowTree.claude.lsp` | `true` | the language servers as Claude Code plugin `lazy-cow-tree-lsp` from a local marketplace in `.claude/settings.local.json` (step 8) |
| `lazyCowTree.codex.lsp` | `true` | each supported language server as Codex MCP server `lsp-<binary>` (mcp-language-server) in `.codex/config.toml` (step 8) |
| `lazyCowTree.codex.mcpLanguageServer` | `pkgs.mcp-language-server` | the LSP-to-MCP bridge (Codex and pi) |
| `lazyCowTree.pi.lsp` | `true` | each supported language server as pi MCP server `lsp-<binary>` in `.pi/mcp.json` (step 8) |
| `lazyCowTree.codex.noDaemon` | `true` | wraps `codex` with `--no-daemon`, so it runs commands with this shell's environment |
| `lazyCowTree.claude.trustCa` | `true` | sets `NODE_EXTRA_CA_CERTS` to the local CA in `.claude/settings.local.json` (a value you set there wins) |
| `lazyCowTree.home` | `$LAZY_COW_TREE_HOME`, else `~/.local/state/lazy-cow-tree` | where the module looks for the daemon's CA (read from devenv's environment at evaluation; `env.LAZY_COW_TREE_HOME` too) |
| `lazyCowTree.postgres.package` | `pkgs.postgresql_18` | PostgreSQL build (18+ for copy-on-write databases) |
| `lazyCowTree.postgres.extensions` | none | as in devenv: `extensions: [ extensions.postgis extensions.pgvector ]`; trusted ones: enable with `CREATE EXTENSION` |
| `lazyCowTree.postgres.databases` | `[]` | more databases per checkout next to the main one, e.g. `[ "cms" ]` for a second Ecto repo: `<NAME>_DATABASE_URL`, `<NAME>_TEST_DATABASE_URL`, cloned from their own template like the main one |
| `lazyCowTree.postgres.createExtensions` | `[]` | created as superuser in `template1` (so every database made afterwards) and the primaries' databases, for untrusted extensions checkout roles can't create, e.g. `[ "postgis" "vector" ]`; migrations' `CREATE EXTENSION IF NOT EXISTS` is then a no-op |
| `lazyCowTree.postgres.settings` | `{}` | extra postgresql.conf settings, e.g. `shared_preload_libraries` |
| `lazyCowTree.postgres.ramdiskMB` | `4096` | RAM disk size (used as it fills); resizing needs `lazy-cow-tree down --eject`, which empties every database |
| `lazyCowTree.redis` | `pkgs.redis` | Redis build |

Service options:

| | default | |
|---|---|---|
| `exec` | | command |
| `cwd` | checkout root | working directory, relative to the checkout |
| `http` | `true` | listens on `$PORT`, gets a hostname, started by its first request; `false`: only started as a dependency |
| `default` | `web`, else first http service | what the shell hook and `lazy-cow-tree service` pick without a name |
| `portOffset` | position by name | `PORT` = checkout base port + offset (0–9) |
| `portEnv` | a process's `env.<NAME> = toString ports.http.value` | a variable of its own holding its port (e.g. `WEB_PORT`), set like `PORT` per checkout in every shell and service, so it reaches its port when `PORT` is another service's |
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

The shell hook (step 7) exports (all derived from the checkout's path, no daemon
needed), e.g. for services `web` (default), `api` and `worker`:

| | primary | worktree `fix-login` |
|---|---|---|
| `PORT` | base (`lazyCowTree.port`, else the next free block of 10) + offset | base (20000–28990: hashed from the name, else the next slot no other worktree or process has; recorded in its git admin dir as `lazy-cow-tree-port`) + offset |
| `LAZY_COW_TREE_URL` | `web.myapp.localhost` | `fix-login.web.myapp.localhost` |
| `DATABASE_URL`, `PG*` | `myapp_dev` as role `myapp` | `myapp_dev_fix_login` as role `myapp--fix-login` |
| `TEST_DATABASE_URL` | `myapp_test` | `myapp_test_fix_login` |
| `<NAME>_DATABASE_URL`, `<NAME>_TEST_DATABASE_URL` (`lazyCowTree.postgres.databases`, e.g. `cms`) | `myapp_cms_dev`, `myapp_cms_test` | `myapp_cms_dev_fix_login` (cloned from `myapp_cms_template`), `myapp_cms_test_fix_login` |
| `REDIS_URL` | password `myapp` | password `myapp--fix-login` |
| `LAZY_COW_TREE_<SERVICE>_URL`, `_PORT` | every service's | every service's |
| named ports' `env`, `LAZY_COW_TREE_<SERVICE>_<NAME>_PORT`, `_URL` | every service's | every service's |
| `LAZY_COW_TREE_SERVICE`, `LAZY_COW_TREE_WORKTREE`, `LAZY_COW_TREE_PROJECT` | | |
| `NODE_EXTRA_CA_CERTS` | the local CA | |

Worktree roles were `<project>-<worktree>` before; the daemon renames an old role to
the new name on its next start (or, if that name was shared by two checkouts, makes
the new role a member of it). The Redis password and the role changed, so restart
anything a worktree runs by hand with the old environment.

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

In the devenv shell `git` is the module's wrapper (`lazyCowTree.git.enable`):
`git worktree add` (any options; `--no-checkout` and `--orphan` pass through) makes
the worktree with git, locked as `initializing`, and `lazy-cow-tree-cow populate`
fills it like `lazy-cow-tree worktree new`: copy-on-write clones of the primary,
gitignored caches (`deps/`, `_build/`, `node_modules/`, …) included and a carried Mix
build relocated ([git-cow](https://github.com/onnimonni/git-cow)); where the filesystem
can't clone, the caches are copied. Language server indexes (`.dexter/`,
`.elixir_ls/`, …) are never carried: they name the primary's paths. Then it asks the
daemon to provision it (`lazy-cow-tree reconcile`, in the background), which runs
`lazyCowTree.setup` once in it. `git worktree remove`, `prune` and `move` ask it to
reconcile too, which drops a removed worktree's databases, role and redis-server.
With a GitHub `origin`, `git worktree remove` refuses while the worktree's branch has an
open pull request (checked with `gh`; one error line naming the PR); a merged, closed
or missing PR removes it as usual, and if `gh` can't tell (not logged in, offline) it
removes it too. `FORCE_ALLOW_OPEN_PR=1` skips the check.

Nothing watches the filesystem: a worktree made or deleted outside the wrapper (plain
git elsewhere, `rm -rf`) is picked up by the daemon within a minute, and one made by
plain git has no caches.

## Commands

```sh
lazy-cow-tree serve                        # daemon (devenv process)
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
