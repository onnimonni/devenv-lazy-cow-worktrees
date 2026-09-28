#!/usr/bin/env bash
# End to end, on macOS (APFS RAM disk) and Linux: a real daemon with PostgreSQL 18
# and Redis from PATH, a project with a remote, worktrees made by localforest and by
# plain git, their databases, Redis, HTTPS services, .env, and removal.
#
#   tests/e2e.sh [path to the localforest binary]   (default: target/debug/localforest)
#
# Needs in PATH: postgres, initdb, psql, redis-server, redis-cli, git, curl, jq, and
# python3 (or uv).
set -euo pipefail

bin=$(realpath "${1:-target/debug/localforest}")
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
export LOCALFOREST_HOME=$home LOCALFOREST_PROJECT=demo LOCALFOREST_PORT=4100
export LOCALFOREST_PG_PORT=55499 LOCALFOREST_REDIS_PORT=6399
export LOCALFOREST_HTTPS_PORT=8443 LOCALFOREST_HTTP_PORT=0 LOCALFOREST_RAMDISK_MB=512
export LOCALFOREST_MIGRATE="psql -v ON_ERROR_STOP=1 -c 'CREATE TABLE IF NOT EXISTS seeds(x int); INSERT INTO seeds VALUES (1)'"
export LOCALFOREST_SETUP="sh -c 'echo \"\$LOCALFOREST_WORKTREE\" >> $home/setup.log'"
web="sh -c 'exec $python -m http.server \"\$PORT\" --bind 127.0.0.1'"
LOCALFOREST_SERVICES=$(jq -nc --arg web "$web" '{web: {exec: $web}}')
export LOCALFOREST_SERVICES
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

cd "$work"
git init -q --bare -b main origin.git
git clone -q origin.git app 2>/dev/null
cd app
echo primary > index.html
g add -A && g commit -qm init && git push -q origin main

"$bin" serve >"$work/daemon.log" 2>&1 &
daemon=$!
eventually 60 "$bin" status || fail "daemon did not start"
pass "daemon up ($(uname -s))"

template_seeded() {
  [[ $(PGUSER=postgres psql -h "$home/pg" -p 55500 -d demo_template -tAc "select count(*) from seeds") == 1 ]]
}
eventually 60 template_seeded || fail "template not seeded by the migrate command"
pass "primary migrated, template refreshed"

wt=$("$bin" worktree new feat-a 2>/dev/null)
[[ -f $wt/index.html ]] || fail "worktree not created"
pass "worktree new: $wt"

eval "$(cd "$wt" && "$bin" env)"
[[ $(psql -tAc "select count(*) from seeds") == 1 ]] || fail "worktree database not cloned from the template"
[[ $(psql -tAc "select current_database()") == demo_dev_feat_a ]] || fail "wrong database"
pass "worktree database cloned from the template on first connect"
if psql -d demo_dev -tAc "select 1" >/dev/null 2>&1; then fail "worktree could open the primary's database"; fi
pass "other checkouts' databases refused"
[[ $(psql -tAc "select rolsuper::text from pg_roles where rolname = current_user") == false ]] || fail "checkout role is a superuser"
psql -qc "ALTER TABLE seeds ADD COLUMN y int" || fail "worktree can't migrate the template's tables"
for sql in "DROP DATABASE demo_dev" "ALTER ROLE demo SUPERUSER" "COPY (SELECT 1) TO PROGRAM 'true'"; do
  if psql -d postgres -qc "$sql" 2>/dev/null; then fail "worktree role could: $sql"; fi
done
[[ $(admin_psql "select 1 from pg_database where datname = 'demo_dev'") == 1 ]] || fail "primary's database gone"
pass "worktree role owns its clone, can't touch other checkouts"

redis-cli --no-auth-warning -u "$REDIS_URL" set k worktree >/dev/null
primary_redis=$(cd "$work/app" && "$bin" env --json | jq -r .REDIS_URL)
[[ -z $(redis-cli --no-auth-warning -u "$primary_redis" get k) ]] || fail "redis not isolated"
[[ $(redis-cli --no-auth-warning -u "$REDIS_URL" get k) == worktree ]] || fail "redis lost the key"
pass "redis isolated per checkout"

out=$(curl_lf "https://feat-a.web.demo.localhost:8443/" 2>&1) || true
[[ $out == primary ]] || { cat "$home/logs/demo-feat-a.web.log" >&2; fail "https service not started on demand: $out"; }
pass "https://feat-a.web.demo.localhost started its service on demand"

grep -q "DATABASE_URL=" "$wt/.env" || fail ".env not written"
git -C "$wt" check-ignore -q .env || fail ".env not gitignored"
[[ -z $(git -C "$wt" status --porcelain) ]] || fail "worktree not clean"
pass ".env written and gitignored"

git worktree add -q -b manual .claude/worktrees/manual
eventually 30 test -f .claude/worktrees/manual/.env || fail "plain git worktree not provisioned"
eventually 30 grep -q manual "$home/setup.log" || fail "setup did not run"
pass "plain git worktree add provisioned, setup ran"

port=$(cd "$wt" && "$bin" env --json | jq -r .PORT)
"$bin" worktree rm --force feat-a
[[ ! -e $wt ]] || fail "worktree still there"
if curl -s --max-time 2 "http://127.0.0.1:$port/" >/dev/null; then fail "its server survived"; fi
[[ -z $(admin_psql "select 1 from pg_database where datname = 'demo_dev_feat_a'") ]] || fail "its database survived"
pass "rm killed its service and dropped its database"

code=$(curl_lf -o "$work/gone.html" -w '%{http_code}' "https://feat-a.web.demo.localhost:8443/")
[[ $code == 503 ]] && grep -q "Recreate worktree" "$work/gone.html" || fail "no gone page ($code)"
pass "gone page (503) for the removed worktree"

echo "all passed"
