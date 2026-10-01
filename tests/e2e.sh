#!/usr/bin/env bash
# End to end, on macOS (APFS RAM disk) and Linux: a real daemon with PostgreSQL 18
# and Redis from PATH, a project with a remote, worktrees made by lazy-cow-tree and by
# the devenv module's git wrapper, their databases, Redis, HTTPS services, and removal.
#
#   tests/e2e.sh [path to the lazy-cow-tree binary]   (default: target/debug/lazy-cow-tree;
#   lazy-cow-tree-cow next to it)
#
# Needs in PATH: postgres, initdb, psql, redis-server, redis-cli, git, curl, jq, and
# python3 (or uv).
set -euo pipefail

bin=$(realpath "${1:-target/debug/lazy-cow-tree}")
cow=$(dirname "$bin")/lazy-cow-tree-cow
for tool in postgres initdb psql redis-server redis-cli git curl jq; do
  command -v "$tool" >/dev/null || { echo "missing $tool in PATH" >&2; exit 1; }
done
# A Python that runs (macOS's /usr/bin/python3 is a stub without the Xcode tools).
if python3 -c '' 2>/dev/null; then
  python=python3
elif command -v uv >/dev/null; then
  python="uv run --no-project python"
else
  echo "missing a working python3 (or uv)" >&2; exit 1
fi

# Short: unix socket paths are limited to ~104 bytes.
home=$(mktemp -d /tmp/lf.XXXXXX)
work=$(mktemp -d)
export LAZY_COW_TREE_HOME=$home LAZY_COW_TREE_PROJECT=demo LAZY_COW_TREE_PORT=4100
export LAZY_COW_TREE_PG_PORT=55499 LAZY_COW_TREE_REDIS_PORT=6399
export LAZY_COW_TREE_HTTPS_PORT=8443 LAZY_COW_TREE_HTTP_PORT=0 LAZY_COW_TREE_RAMDISK_MB=512
# Fails in the worktree named "broken", and in any checkout (the primary included)
# where setup has not run yet: like `mix ecto.migrate` before `mix deps.get`.
cat >"$home/migrate.sh" <<'EOF'
[[ ${LAZY_COW_TREE_WORKTREE:-} == broken ]] && { echo "migration broke" >&2; exit 1; }
grep -qx "${LAZY_COW_TREE_WORKTREE:-primary}" "$(dirname "$0")/setup.log" || { echo "setup did not run" >&2; exit 1; }
psql -v ON_ERROR_STOP=1 -d "$CMS_DATABASE_URL" -c 'CREATE TABLE IF NOT EXISTS pages(x int); INSERT INTO pages VALUES (1)' >/dev/null
exec psql -v ON_ERROR_STOP=1 -c 'CREATE TABLE IF NOT EXISTS seeds(x int); INSERT INTO seeds VALUES (1)'
EOF
export LAZY_COW_TREE_MIGRATE="bash $home/migrate.sh"
# A second database per checkout (a CMS repo), cloned like the main one.
export LAZY_COW_TREE_DATABASES=cms
export LAZY_COW_TREE_SETUP="sh -c 'echo \"\${LAZY_COW_TREE_WORKTREE:-primary}\" >> $home/setup.log'"
# Also serves on its named secondary port, like Phoenix's LiveDebugger; never binds `idle`.
web="sh -c '$python -m http.server \"\$DEBUGGER_PORT\" --bind 127.0.0.1 & exec $python -m http.server \"\$PORT\" --bind 127.0.0.1'"
# A Mix server: restarted when mix.lock changes. Logs each start.
mkdir "$home/bin"
cat >"$home/bin/mix" <<EOF
#!/usr/bin/env bash
echo started >> "$home/mix-starts.log"
exec $python -m http.server "\$PORT" --bind 127.0.0.1
EOF
chmod +x "$home/bin/mix"
# The devenv module's git wrapper (`git worktree add` via lazy-cow-tree-cow).
sed -e "s|@git@|$(command -v git)|" -e "s|@cow@|$cow|" -e "s|@gh@|$(command -v false)|" -e "s|@lazyCowTree@|$bin|" \
  "$(dirname "$0")/../devenv-module/git.sh" >"$home/bin/git"
chmod +x "$home/bin/git"
wgit() { "$home/bin/git" "$@"; }
LAZY_COW_TREE_SERVICES=$(jq -nc --arg web "$web" --arg phx "$home/bin/mix phx.server" \
  '{web: {exec: $web, portOffset: 0, ports: {debugger: {http: true}, test: {env: "TEST_PORT"}, idle: {http: true, offset: 5}}},
    phx: {exec: $phx, portOffset: 3}}')
export LAZY_COW_TREE_SERVICES
unset GH_TOKEN GITHUB_TOKEN

daemon=
cleanup() {
  status=$?
  "$bin" down --eject >/dev/null 2>&1 || true
  [[ -n $daemon ]] && kill "$daemon" 2>/dev/null || true
  if ((status)); then
    echo "--- daemon log" >&2
    sed 's/\x1b\[[0-9;]*m//g' "$work/daemon.log" >&2 || true
  fi
  rm -rf "$work" "$home"
}
trap cleanup EXIT

pass() { echo "ok - $*"; }
fail() {
  echo "FAIL - $*" >&2
  sed "s/\x1b\[[0-9;]*m//g" "$work/daemon.log" | tail -25 >&2
  exit 1
}
# eventually <seconds> <command...>: retry until it succeeds.
eventually() {
  local deadline=$((SECONDS + $1)); shift
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || return 1
    sleep 0.3
  done
}
curl_lf() { curl -sS --max-time 90 --cacert "$home/ca/ca.pem" "$@"; }
# Straight to the real server (the daemon's own superuser path).
admin_psql() { PGUSER=postgres psql -h "$home/pg" -p 55500 -d postgres -tAc "$1"; }
g() { git -c user.name=t -c user.email=t@t "$@"; }
# `lazy-cow-tree shell-hook`'s value of <var> in <dir>.
env_of() { (cd "$1" && unset LAZY_COW_TREE_SHELL && eval "$("$bin" shell-hook)" && printenv "$2"); }

cd "$work"
git init -q --bare -b main origin.git
git clone -q origin.git app 2>/dev/null
cd app
echo primary > index.html
echo '%{}' > mix.lock
g add -A && g commit -qm init && git push -q origin main

"$bin" serve >"$work/daemon.log" 2>&1 &
daemon=$!
eventually 60 "$bin" status || fail "daemon did not start"
pass "daemon up ($(uname -s))"

template_seeded() {
  [[ $(PGUSER=postgres psql -h "$home/pg" -p 55500 -d demo_template -tAc "select count(*) from seeds") == 1 ]]
}
eventually 60 template_seeded || fail "template not seeded by the migrate command"
[[ -f $(git rev-parse --absolute-git-dir)/lazy-cow-tree-setup ]] || fail "primary's setup not marked done"
[[ $(grep -cx primary "$home/setup.log") == 1 ]] || fail "setup did not run once in the primary"
pass "fresh primary: setup ran before migrate; migrated, template refreshed"

wt=$("$bin" worktree new feat-a 2>/dev/null)
[[ -f $wt/index.html ]] || fail "worktree not created"
pass "worktree new: $wt"

eval "$(cd "$wt" && "$bin" shell-hook)"
# A template refresh racing the first connect: the clone still gets the template.
"$bin" snapshot & snap=$!
# Row counts depend on how the template is made; the marker says it was migrated.
(($(psql -tAc "select count(*) from seeds") >= 1)) || fail "worktree database not cloned from the template"
wait "$snap" || fail "snapshot failed"
[[ -f $(git -C "$wt" rev-parse --absolute-git-dir)/lazy-cow-tree-migrated ]] || fail "new worktree not migrated"
template_seeded || fail "template lost by the snapshot"
[[ -z $(admin_psql "select 1 from pg_database where datname = 'demo_template_next'") ]] || fail "demo_template_next left behind"
[[ $(psql -tAc "select current_database()") == demo_dev_feat_a ]] || fail "wrong database"
pass "worktree database cloned from the template and migrated"
if psql -d demo_dev -tAc "select 1" >/dev/null 2>&1; then fail "worktree could open the primary's database"; fi
pass "other checkouts' databases refused"
(( $(psql -d "$CMS_DATABASE_URL" -tAc "select count(*) from pages") >= 1 )) || fail "extra database not cloned from its template"
[[ $CMS_DATABASE_URL == */demo_cms_dev_feat_a ]] || fail "CMS_DATABASE_URL=$CMS_DATABASE_URL"
if psql -d demo_cms_dev -tAc "select 1" >/dev/null 2>&1; then fail "worktree could open the primary's cms database"; fi
psql -d postgres -qc "CREATE DATABASE demo_cms_test_feat_a3" || fail "could not create a cms partition database"
pass "extra database (cms): cloned from its template, partitions, others' refused"
psql -d postgres -qc "CREATE DATABASE demo_test_feat_a2" || fail "could not create a partition database"
[[ $(psql -d demo_test_feat_a2 -tAc "select 1") == 1 ]] || fail "MIX_TEST_PARTITION database <test db>2 refused"
pass "MIX_TEST_PARTITION database opened"
if out=$(PGUSER=stranger PGPASSWORD=x psql -d postgres -tAc "select 1" 2>&1); then fail "unknown user got through"; fi
[[ $out == *"not the role of a checkout"* ]] || fail "unknown user not refused by the proxy: $out"
pass "users that are no checkout's role refused"
[[ $(psql -tAc "select rolsuper::text from pg_roles where rolname = current_user") == false ]] || fail "checkout role is a superuser"
psql -qc "ALTER TABLE seeds ADD COLUMN y int" || fail "worktree can't migrate the template's tables"
for sql in "DROP DATABASE demo_dev" "ALTER ROLE demo SUPERUSER" "COPY (SELECT 1) TO PROGRAM 'true'"; do
  if psql -d postgres -qc "$sql" 2>/dev/null; then fail "worktree role could: $sql"; fi
done
[[ $(admin_psql "select 1 from pg_database where datname = 'demo_dev'") == 1 ]] || fail "primary's database gone"
pass "worktree role owns its clone, can't touch other checkouts"

redis-cli --no-auth-warning -u "$REDIS_URL" set k worktree >/dev/null
primary_redis=$(env_of "$work/app" REDIS_URL)
[[ -z $(redis-cli --no-auth-warning -u "$primary_redis" get k) ]] || fail "redis not isolated"
[[ $(redis-cli --no-auth-warning -u "$REDIS_URL" get k) == worktree ]] || fail "redis lost the key"
pass "redis isolated per checkout"

base=$(env_of "$wt" PORT)
[[ $DEBUGGER_PORT == $((base + 9)) && $TEST_PORT == $((base + 8)) ]] ||
  fail "named ports: DEBUGGER_PORT=$DEBUGGER_PORT TEST_PORT=$TEST_PORT (base $base)"
[[ $LAZY_COW_TREE_WEB_DEBUGGER_URL == https://feat-a.debugger.demo.localhost:8443 ]] ||
  fail "LAZY_COW_TREE_WEB_DEBUGGER_URL=$LAZY_COW_TREE_WEB_DEBUGGER_URL"
pass "named ports in env: DEBUGGER_PORT=$DEBUGGER_PORT TEST_PORT=$TEST_PORT"
wt_root=$(DEVENV_ROOT=$(pwd -P) env_of "$wt" DEVENV_ROOT)
[[ $wt_root == "$(cd "$wt" && pwd -P)" ]] || fail "shell hook in a worktree: DEVENV_ROOT=$wt_root"
pass "shell hook in a worktree of the devenv project: its own DEVENV_ROOT"

web_log=$(cd "$wt" && "$bin" service log -s web)
out=$(curl_lf "https://feat-a.debugger.demo.localhost:8443/" 2>&1) || true
[[ $out == primary ]] || { cat "$web_log" >&2; fail "named http port did not start its service: $out"; }
pass "https://feat-a.debugger.demo.localhost started web on demand"

out=$(curl_lf "https://feat-a.web.demo.localhost:8443/" 2>&1) || true
[[ $out == primary ]] || { cat "$web_log" >&2; fail "https service not served: $out"; }
pass "https://feat-a.web.demo.localhost served"

start=$SECONDS
code=$(curl_lf -o "$work/idle.txt" -w '%{http_code}' "https://feat-a.idle.demo.localhost:8443/")
[[ $code == 502 ]] && grep -q "doesn't listen on its idle port" "$work/idle.txt" && ((SECONDS - start < 20)) ||
  fail "unbound named port: $code in $((SECONDS - start)) s: $(cat "$work/idle.txt")"
pass "unbound named port answers 502 after a short grace"

[[ -z $(git -C "$wt" status --porcelain) ]] || fail "worktree not clean"
pass "worktree clean"

"$bin" worktree new broken >/dev/null 2>&1 || fail "worktree new failed on a failing migration"
[[ $("$bin" status) == *"migrations failed"* ]] || fail "failed migration not in status"
out=$(curl_lf "https://broken.web.demo.localhost:8443/" 2>&1) || true
[[ $out == *"migrations failed"* ]] || fail "failed migration not on its page: $out"
[[ ! -f $(git -C .claude/worktrees/broken rev-parse --absolute-git-dir)/lazy-cow-tree-migrated ]] ||
  fail "failed migration marked done"
"$bin" worktree rm --force broken
pass "failed migration: in status and on the 502 page, services not started"

wgit worktree add -q -b manual .claude/worktrees/manual
[[ -f $(git -C .claude/worktrees/manual rev-parse --absolute-git-dir)/lazy-cow-tree-populated ]] ||
  fail "git wrapper did not populate the worktree"
[[ -z $(git -C .claude/worktrees/manual status --porcelain) ]] || fail "wrapper's worktree not clean"
eventually 30 grep -q manual "$home/setup.log" || fail "setup did not run"
pass "git wrapper: worktree add populated, provisioned, setup ran"
eventually 30 test -f "$(git -C .claude/worktrees/manual rev-parse --absolute-git-dir)/lazy-cow-tree-setup" ||
  fail "setup not marked done"
out=$("$bin" worktree rm --force manual 2>&1) || fail "rm of a fresh worktree failed: $out"
[[ $out != *"deleting gitignored"* ]] || fail "fresh worktree's removal warned: $out"
pass "fresh worktree removed without a gitignored-files warning"

port=$(env_of "$wt" PORT)
"$bin" worktree rm --force feat-a
[[ ! -e $wt ]] || fail "worktree still there"
for p in "$port" "$((port + 9))"; do
  if curl -s --max-time 2 "http://127.0.0.1:$p/" >/dev/null; then fail "its server on $p survived"; fi
done
[[ -z $(admin_psql "select 1 from pg_database where datname in ('demo_dev_feat_a', 'demo_test_feat_a2', 'demo_cms_dev_feat_a', 'demo_cms_test_feat_a3')") ]] || fail "its databases survived"
pass "rm killed its service and dropped its database"

code=$(curl_lf -o /dev/null -w '%{http_code}' "https://feat-a.web.demo.localhost:8443/")
[[ $code == 404 ]] || fail "removed worktree's host answered $code"
pass "removed worktree's host: 404"

wgit worktree add -q -b manual2 .claude/worktrees/manual2
role_exists() { [[ -n $(admin_psql "select 1 from pg_roles where rolname = 'demo--manual2'") ]]; }
eventually 30 role_exists || fail "wrapper's worktree not provisioned"
wgit worktree remove --force .claude/worktrees/manual2
role_gone() { ! role_exists; }
eventually 30 role_gone || fail "git worktree remove: its role survived"
pass "git wrapper: worktree remove cleaned up"

# Something a shell in a worktree started, detached (setsid, cd /, parent gone): found
# through the shell hook's marker; `worktree remove` refuses until asked to kill it.
wgit worktree add -q -b procs .claude/worktrees/procs
(cd .claude/worktrees/procs && bash -c 'eval "$("$1" shell-hook)"
  perl -e "use POSIX; fork and exit; POSIX::setsid(); chdir q(/); exec q(sleep), q(4242)" </dev/null >/dev/null 2>&1' _ "$bin")
eventually 5 pgrep -f 'sleep 4242$' || fail "detached process did not start"
procs_debug() {
  local p; p=$(pgrep -f 'sleep 4242$' | head -1)
  echo "procs: $("$bin" worktree procs .claude/worktrees/procs 2>&1)"
  echo "sleep $p: ppid $(ps -o ppid= -p "$p") $(lsof -p "$p" 2>/dev/null | grep -E 'cwd|DIR' | tr -s ' ' | cut -d' ' -f4-)"
  git worktree list --porcelain
}
dbg=$(procs_debug 2>&1)
if out=$(wgit worktree remove procs 2>&1); then fail "removed with a process left: $out
$dbg"; fi
[[ $out == *'with command "sleep 4242" was launched from this worktree'* ]] || fail "not listed: $out"
[[ -d .claude/worktrees/procs ]] || fail "worktree removed anyway"
FORCE_KILL_PROCESSES=1 wgit worktree remove procs || fail "FORCE_KILL_PROCESSES=1 did not remove"
gone() { ! pgrep -f 'sleep 4242$' >/dev/null; }
eventually 5 gone || fail "detached process survived"
pass "git wrapper: worktree remove lists, then kills, what a shell there started"

mix_starts() { [[ $(wc -l <"$home/mix-starts.log") -eq $1 ]]; }
setups() { [[ $(wc -l <"$home/setup.log") -eq $1 ]]; }
curl_lf -o /dev/null "https://phx.demo.localhost:8443/" || fail "mix service did not start"
mix_starts 1 || fail "mix service started $(wc -l <"$home/mix-starts.log") times"
echo '%{}' > mix.lock   # rewritten, same content
sleep 3
mix_starts 1 || fail "mix service restarted for an unchanged mix.lock"
before=$(wc -l <"$home/setup.log")
echo '%{"x" => 1}' > mix.lock
eventually 30 mix_starts 2 || fail "mix service not restarted after mix.lock changed"
setups $((before + 1)) || fail "setup did not run before the restart"
mkdir -p config && echo 'import Config' > config/dev.exs
eventually 30 mix_starts 3 || fail "mix service not restarted after config/dev.exs appeared"
setups $((before + 1)) || fail "setup ran for a config-only change"
rm -r config
eventually 30 mix_starts 4 || fail "mix service not restarted after config/dev.exs was removed"
eventually 30 curl_lf -f -o /dev/null "https://phx.demo.localhost:8443/" || fail "mix service down after restart"
git checkout -q mix.lock
eventually 30 mix_starts 5 || fail "mix service not restarted after mix.lock was restored"
pass "mix service restarted when mix.lock (after setup) or config changed"

# A fresh primary database (as after a reboot) while the primary is on a feature
# branch: migrated anyway, but the template is not made from the feature branch.
g checkout -qb primary-feat
admin_psql "DROP DATABASE demo_template WITH (FORCE)" >/dev/null
admin_psql "DROP DATABASE demo_dev WITH (FORCE)" >/dev/null
wgit worktree add -q -b fresh .claude/worktrees/fresh
fresh_marker=$(git -C .claude/worktrees/fresh rev-parse --absolute-git-dir)/lazy-cow-tree-migrated
eventually 60 test -f "$fresh_marker" || fail "worktree not migrated after the fresh primary"
(($(PGUSER=postgres psql -h "$home/pg" -p 55500 -d demo_dev -tAc "select count(*) from seeds") >= 1)) ||
  fail "fresh primary database on a feature branch not migrated"
[[ -z $(admin_psql "select 1 from pg_database where datname = 'demo_template'") ]] ||
  fail "template made from a feature branch"
pass "fresh primary on a feature branch migrated, template left alone"

echo "all passed"
