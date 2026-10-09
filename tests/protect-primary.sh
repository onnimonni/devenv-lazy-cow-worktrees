#!/usr/bin/env bash
# lazyCowTree.protectPrimary's shell guard (devenv-module/protect-primary.sh) in each
# given shell (bash 3.2 and 5, zsh): refused and allowed commands in the primary
# checkout, its subdirectories and a worktree inside it; `-c` and interactive shells;
# startup files left alone.
#
#   tests/protect-primary.sh /bin/bash "$(command -v bash)" zsh
set -uo pipefail

src=$(cd "$(dirname "$0")/.." && pwd)
work=$(cd "$(mktemp -d)" && pwd -P)
trap 'rm -rf "$work"' EXIT
cd "$work" || exit 1
git init -q r && git -C r -c user.name=t -c user.email=t@t commit -q --allow-empty -m init &&
  git -C r worktree add -q .claude/worktrees/x && mkdir r/sub || exit 1
sed -e "s|@root@|'$work/r'|" -e "s|@allowed@|cd 'git status' echo|" -e "s|@worktreesDir@|.claude/worktrees|" \
  "$src/devenv-module/protect-primary.sh" >guard.sh
printf '. %s/guard.sh\ntouch startup-ok\n' "$work" >.zshenv
printf '. %s/guard.sh\ntouch rc-ok\n' "$work" >rc
cd r || exit 1

fails=0
fail() { echo "FAIL - $*"; fails=1; }
# exists <file> yes|no <what>
exists() {
  if [[ -e $1 ]]; then [[ $2 == yes ]] || fail "$3: $1 created"
  else [[ $2 == no ]] || fail "$3: $1 missing"; fi
}

for sh in "$@"; do
  # shellcheck disable=SC2016 # expanded by $sh
  n=$("$sh" -c 'echo "${BASH_VERSION:+bash-$BASH_VERSION}${ZSH_VERSION:+zsh-$ZSH_VERSION}"')
  rm -f -- *-ok bad* sub/bad* ibad .claude/worktrees/x/*
  if [[ $n == zsh-* ]]; then
    c() { ZDOTDIR=$work "$sh" -c "$1"; }
    i() { ZDOTDIR=$work "$sh" -i; }
  else
    c() { BASH_ENV=$work/guard.sh "$sh" -c "$1"; }
    # bash 5 reads no --rcfile with stdin not a terminal: sourced as the first line.
    i() { (echo ". $work/rc"; cat) | BASH_ENV='' "$sh" --norc -i; }
  fi

  c 'touch bad1; echo ran' >/dev/null 2>"$work/err"
  exists bad1 no "$n -c"
  grep -q "git worktree add" "$work/err" || fail "$n: no hint: $(cat "$work/err")"
  ! grep -q bashdb "$work/err" || fail "$n: bash looked for its debugger: $(cat "$work/err")"
  c 'git status >/dev/null && touch bad2' 2>/dev/null
  exists bad2 no "$n -c after an allowed command"
  c 'cd .claude/worktrees/x; cd ../../..; touch bad3' 2>/dev/null
  exists bad3 no "$n -c back from a worktree"
  c 'cd sub && touch bad4' 2>/dev/null
  exists sub/bad4 no "$n -c in a subdirectory"
  c 'echo "a;b|c" 2>&1 && cd ".claude/worktrees/x" && touch wt-ok' >/dev/null 2>&1
  exists .claude/worktrees/x/wt-ok yes "$n -c into a worktree"
  c "cd '$work' && touch outside-ok" 2>/dev/null
  exists "$work/outside-ok" yes "$n -c outside the primary"
  [[ $n == zsh-* ]] && exists startup-ok yes "$n startup file"

  printf 'touch ibad\ncd .claude/worktrees/x\ntouch i-ok\nexit\n' | i >/dev/null 2>&1
  exists ibad no "$n interactive"
  exists .claude/worktrees/x/i-ok yes "$n interactive, in a worktree"
  [[ $n == bash-* ]] && exists rc-ok yes "$n startup file"
  ((fails)) || echo "ok - $n"
done
exit $fails
