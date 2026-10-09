#!/usr/bin/env bash
# `gh` in the devenv shell (lazyCowTree.gh.enable): after `gh pr merge` or `gh pr close`
# leaves the pull request merged or closed, the worktree that has its branch checked
# out goes (`lazy-cow-tree worktree rm`). One with uncommitted changes stays; so does a
# closed one whose branch has commits that aren't pushed or merged anywhere (`--force`
# keeps them as `<name>-kept-<sha>`, then removes it). Every other command is the real gh.
#
#   LAZY_COW_TREE_GH_DISABLE=1  plain gh

real_gh=@gh@
real_git=@git@
lazy_cow_tree=@lazyCowTree@

if [[ -n ${LAZY_COW_TREE_GH_DISABLE:-} || ${1:-} != pr || (${2:-} != merge && ${2:-} != close) ]]; then
  exec "$real_gh" "$@"
fi

# The PR it acts on: the first argument that is no option (number, URL or branch),
# else the current branch's. Options taking a value are skipped with it.
selector=() repo=() repo_name=''
args=("${@:3}")
for ((i = 0; i < ${#args[@]}; i++)); do
  a=${args[i]}
  case $a in
    -R | --repo) repo_name=${args[i + 1]:-}; ((i++)) ;;
    --repo=*) repo_name=${a#--repo=} ;;
    -b | --body | -F | --body-file | -t | --subject | -c | --comment | -A | --author-email | --match-head-commit) ((i++)) ;;
    -*) ;;
    *) [[ ${#selector[@]} -eq 0 ]] && selector=("$a") ;;
  esac
done

"$real_gh" "$@"
status=$?

# Another repository's PR: its branch name says nothing about this one's worktrees.
if [[ -n $repo_name ]]; then
  repo=(--repo "$repo_name")
  this=$("$real_gh" repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null)
  that=$("$real_gh" repo view "$repo_name" --json nameWithOwner -q .nameWithOwner 2>/dev/null)
  # gh prints the repository as GitHub spells it, whatever case -R had.
  [[ -n $this && $this == "$that" ]] || exit "$status"
fi

# `--delete-branch` may fail on the local branch (checked out in a worktree) after
# merging: what the PR is now decides, not gh's exit status.
read -r state branch < <("$real_gh" pr view "${selector[@]}" "${repo[@]}" --json state,headRefName \
  -q '"\(.state) \(.headRefName)"' 2>/dev/null)
[[ $state == MERGED || $state == CLOSED ]] || exit "$status"

# The worktree (not the primary checkout) with that branch.
wt='' path='' first=''
while read -r key value; do
  case $key in
    worktree) path=$value; [[ -z $first ]] && first=$path ;;
    branch) [[ $value == "refs/heads/$branch" && $path != "$first" ]] && wt=$path ;;
  esac
done < <("$real_git" worktree list --porcelain 2>/dev/null)
[[ -n $wt ]] || exit "$status"

echo "PR $state: removing worktree $wt" >&2
if [[ -n $("$real_git" -C "$wt" status --porcelain 2>/dev/null) ]]; then
  echo "kept: $wt has uncommitted changes; \`lazy-cow-tree worktree rm --force $wt\` deletes them" >&2
  exit "$status"
fi
# Merged: nothing to lose. Closed: its commits are kept as a branch if needed.
if [[ $state == MERGED ]]; then
  "$lazy_cow_tree" worktree rm "$wt" || exit "$status"
else
  "$lazy_cow_tree" worktree rm --force "$wt" || exit "$status"
fi
[[ $PWD == "$wt" || $PWD == "$wt"/* ]] && echo "you were in it: cd $first" >&2
exit "$status"
