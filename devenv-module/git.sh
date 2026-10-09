#!/usr/bin/env bash
# `git` in the devenv shell (lazyCowTree.git.enable): `git worktree add` (by you,
# scripts or agents) makes the worktree with git (`--no-checkout`, locked as
# "initializing" so the daemon waits) and fills it like `lazy-cow-tree worktree new`
# does (`lazy-cow-tree-cow populate`): copy-on-write clones of the primary checkout,
# build caches included. After `worktree add/remove/prune/move` the daemon, if
# running, provisions or cleans up (`lazy-cow-tree reconcile`). `worktree remove` refuses
# while the branch has an open pull request on a GitHub origin (gh) or something started
# in the worktree still runs (`lazy-cow-tree worktree procs`). Every other command
# is the real git. Adapted from git-cow's wrapper.
#
#   LAZY_COW_TREE_GIT_DISABLE=1  plain git
#   FORCE_ALLOW_OPEN_PR=1        `git worktree remove` even while its branch has an open PR
#   FORCE_KILL_PROCESSES=1       `git worktree remove` SIGKILLs what still runs in it
#                                (else it refuses and lists them)

real_git=@git@
cow=@cow@
gh=@gh@
lazy_cow_tree=@lazyCowTree@

# FORCE_* variables: set to 1 (true, yes); 0, false or empty is off.
truthy() { case ${1:-} in 1 | [Tt]rue | TRUE | [Yy]es | YES) return 0 ;; esac; return 1; }

# Tell the daemon (if running) to provision new worktrees / clean up removed ones.
reconcile() { ("$lazy_cow_tree" reconcile --path "$1" >/dev/null 2>&1 &) }

# fast path: only `git worktree` is changed
if [[ -n ${LAZY_COW_TREE_GIT_DISABLE:-} || " $* " != *" worktree "* ]]; then
  exec "$real_git" "$@"
fi

# global options before the subcommand; -C changes where relative paths resolve
globals=()
base=$PWD
while (($#)) && [[ $1 == -* ]]; do
  case $1 in
    -C)
      [[ $2 == /* ]] && base=$2 || base=$base/$2
      globals+=("$1" "$2"); shift 2 ;;
    -c | --git-dir | --work-tree | --namespace | --config-env | --super-prefix)
      globals+=("$1" "$2"); shift 2 ;;
    *)
      globals+=("$1"); shift ;;
  esac
done
if [[ ${1:-} != worktree ]]; then
  exec "$real_git" "${globals[@]}" "$@"
fi
case ${2:-} in
  add) ;;
  remove)
    # Not while its branch has an open pull request on a GitHub origin.
    target=
    for a in "${@:3}"; do
      [[ $a == -* ]] || { target=$a; break; }
    done
    arg=$target
    [[ -z $target || $target == /* ]] || target=$base/$target
    if [[ -n $target ]] && ! truthy "${FORCE_ALLOW_OPEN_PR:-}" &&
      branch=$("$real_git" -C "$target" symbolic-ref -q --short HEAD 2>/dev/null) &&
      [[ $("$real_git" -C "$target" remote get-url origin 2>/dev/null) == *github* ]]; then
      # gh failing (not logged in, offline): removed as usual.
      if prs=$(cd "$target" && "$gh" pr list --head "$branch" --state open \
        --json url --jq '.[].url' 2>/dev/null) && [[ -n $prs ]]; then
        echo "error: worktree not removed: branch '$branch' has an open pull request (${prs//$'\n'/, }); merge or close it first, or set FORCE_ALLOW_OPEN_PR=1" >&2
        exit 1
      fi
    fi
    # Not while something started in it still runs (it would keep writing into a
    # deleted directory), unless asked to kill it. The daemon's services it stops itself.
    # Only a linked worktree git would remove: an exact path or, like git, a unique
    # suffix of one (`git worktree remove foo` for .claude/worktrees/foo).
    wt=
    if [[ -n $target ]]; then
      want=$(cd "$target" 2>/dev/null && pwd -P) || want=
      matches=()
      primary=1
      # Like git: case-insensitive where the filesystem is (core.ignorecase).
      [[ $("$real_git" "${globals[@]}" config --bool core.ignorecase 2>/dev/null) == true ]] &&
        shopt -s nocasematch
      while IFS= read -r l; do
        [[ $l == "worktree "* ]] || continue
        p=${l#worktree }
        if ((primary)); then primary=0; continue; fi
        if [[ -n $want ]]; then
          [[ $(cd "$p" 2>/dev/null && pwd -P) == "$want" ]] && matches=("$p")
        elif [[ $p == */"$arg" ]]; then
          matches+=("$p")
        fi
      done < <("$real_git" "${globals[@]}" worktree list --porcelain 2>/dev/null)
      shopt -u nocasematch
      ((${#matches[@]} == 1)) && wt=${matches[0]}
    fi
    if [[ -n $wt && -d $wt ]]; then
      if truthy "${FORCE_KILL_PROCESSES:-}"; then
        RUST_LOG=lazy_cow_tree=warn "$lazy_cow_tree" worktree procs --kill "$wt" | while IFS= read -r l; do echo "warning: $l" >&2; done
      elif procs=$("$lazy_cow_tree" worktree procs "$wt" 2>/dev/null) && [[ -n $procs ]]; then
        while IFS= read -r l; do echo "warning: $l" >&2; done <<<"$procs"
        printf -v again ' %q' "${globals[@]}" "$@"
        echo "error: worktree not removed: stop them, or kill them with: FORCE_KILL_PROCESSES=1 git$again" >&2
        exit 1
      fi
    fi
    "$real_git" "${globals[@]}" "$@"
    status=$?
    reconcile "$base"
    exit $status ;;
  prune | move)
    "$real_git" "${globals[@]}" "$@"
    status=$?
    reconcile "$base"
    exit $status ;;
  *) exec "$real_git" "${globals[@]}" "$@" ;;
esac
shift 2

# `git worktree add` options: find <path>, drop --checkout (we add --no-checkout),
# keep the caller's --lock / --reason for after the fill
args=()
path=
quiet=
lock=
reason=
while (($#)); do
  case $1 in
    --no-checkout | --orphan) # nothing to clone: let git do it
      exec "$real_git" "${globals[@]}" worktree add "${args[@]}" "$@" ;;
    --checkout) shift ;;
    --lock) lock=1; shift ;;
    --reason) reason=$2; shift 2 ;;
    --reason=*) reason=${1#--reason=}; shift ;;
    -q | --quiet) quiet=1; args+=("$1"); shift ;;
    -b | -B) args+=("$1" "$2"); shift 2 ;;
    --) args+=("$@"); [[ -z $path ]] && path=${2:-}; break ;;
    -*) args+=("$1"); shift ;;
    *) [[ -z $path ]] && path=$1; args+=("$1"); shift ;;
  esac
done

"$real_git" "${globals[@]}" worktree add --no-checkout \
  --lock --reason "initializing (lazy-cow-tree)" "${args[@]}" || exit $?

[[ $path == /* ]] || path=$base/$path
worktree=$(cd "$path" && pwd -P) || exit 1
# The worktree git just created is ours; don't let the caller's GIT_DIR point elsewhere.
wt_git() { env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE "$real_git" -C "$worktree" "$@"; }

if ! "$cow" populate ${quiet:+-q} "$worktree"; then
  echo "copy-on-write fill failed: falling back to a regular checkout" >&2
  wt_git reset --hard --no-recurse-submodules -q || status=$?
fi

wt_git worktree unlock "$worktree"
if [[ -n $lock ]]; then
  wt_git worktree lock ${reason:+--reason "$reason"} "$worktree"
fi
[[ -z ${status:-} ]] || exit "$status"
reconcile "$worktree"

# what `git worktree add` does after its checkout
if head=$(wt_git rev-parse -q --verify HEAD); then
  [[ -n $quiet ]] || wt_git log -1 --format='HEAD is now at %h %s'
  null=${head//?/0}
  wt_git hook run --ignore-missing post-checkout -- "$null" "$head" 1
fi
