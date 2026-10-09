#!/usr/bin/env bash
# lazy-cow-tree-test-slot: `@name@` in the devenv shell (lazyCowTree.testSlots). A test
# run (`@name@` followed by one of the patterns below) waits for one of N test slots
# shared by every checkout (`lazy-cow-tree slot`), so parallel agents' suites don't
# thrash the CPU into timeouts; it holds the slot until it exits. Everything else, and
# a test run started by another one, is the real @name@ at once.
#
#   LAZY_COW_TREE_TEST_SLOTS=N  slots (default: a quarter of the CPUs, at least 2); 0: off

name=@name@
lazy_cow_tree=@lazyCowTree@
# Argument patterns that run tests, one per line (globs per word; empty: any run).
patterns=@patterns@

# The real one: the next in PATH that isn't a wrapper like this one.
real=''
while IFS= read -r p; do
  grep -qs lazy-cow-tree-test-slot "$p" || { real=$p; break; }
done < <(type -ap "$name")
[[ -n $real ]] || { echo "$name: command not found" >&2; exit 127; }

is_test() {
  local pat i ok
  local -a w
  while IFS= read -r pat; do
    [[ -z $pat ]] && return 0
    read -ra w <<<"$pat"
    ok=1
    for ((i = 0; i < ${#w[@]}; i++)); do
      # shellcheck disable=SC2053 # a glob on purpose
      [[ ${args[i]:-} == ${w[i]} ]] || { ok=0; break; }
    done
    ((ok)) && return 0
  done <<<"$patterns"
  return 1
}

args=("$@")
# shellcheck disable=SC2034 # dir: used in the evals
if [[ -z ${LAZY_COW_TREE_SLOT:-} ]] && is_test &&
  read -r n dir < <("$lazy_cow_tree" slot info 2>/dev/null) && ((n > 0)); then
  # fds 150..199 (bash's marker uses 213; macOS shells allow 256).
  fds=()
  for ((i = 0; i < n; i++)); do
    fd=$((150 + i))
    eval "exec $fd<>\"\$dir/$i.lock\"" && fds+=("$fd")
  done
  got=$("$lazy_cow_tree" slot acquire --fds "$(IFS=,; echo "${fds[*]}")" -- "$name" "$@") || got=''
  for fd in "${fds[@]}"; do
    [[ $fd == "$got" ]] || eval "exec $fd>&-"
  done
  export LAZY_COW_TREE_SLOT=1
fi
exec "$real" "$@"
