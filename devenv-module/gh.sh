#!/usr/bin/env bash
# `gh` in the devenv shell (lazyCowTree.gh.enable): after `gh pr merge` or `gh pr close`
# leaves the pull request merged or closed, the worktree that has its branch checked
# out goes (`lazy-cow-tree worktree rm`). One with uncommitted changes stays; so does a
# closed one whose branch has commits that aren't pushed or merged anywhere (`--force`
# keeps them as `<name>-kept-<sha>`, then removes it). Every other command is the real gh.
#
# `gh pr merge` refuses while the PR has unresolved review conversations, or while that
# worktree has uncommitted changes.
#
#   LAZY_COW_TREE_GH_DISABLE=1  plain gh
#   FORCE_ALLOW_UNRESOLVED=1    `gh pr merge` even with unresolved review conversations
#   FORCE_ALLOW_DIRTY_MERGE=1   `gh pr merge` even with uncommitted changes in its worktree

real_gh=@gh@
real_git=@git@
lazy_cow_tree=@lazyCowTree@

# FORCE_* variables: set to 1 (true, yes); 0, false or empty is off.
truthy() { case ${1:-} in 1 | [Tt]rue | TRUE | [Yy]es | YES) return 0 ;; esac; return 1; }

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

# Unresolved review conversations: as GitHub's own merge box, any repository. Unknown
# (offline, not logged in): merged as usual.
if [[ $2 == merge ]] && ! truthy "${FORCE_ALLOW_UNRESOLVED:-}" &&
  url=$("$real_gh" pr view "${selector[@]}" ${repo_name:+--repo "$repo_name"} --json url -q .url 2>/dev/null) &&
  [[ $url == http*://*/*/*/pull/* ]]; then
  p=${url#*://}
  host=${p%%/*} p=${p#*/}
  owner=${p%%/*} p=${p#*/}
  name=${p%%/*}
  number=${p##*/}
  # shellcheck disable=SC2016 # GraphQL variables
  unresolved=$("$real_gh" api graphql --hostname "$host" -F owner="$owner" -F name="$name" -F number="$number" \
    -f query='query($owner: String!, $name: String!, $number: Int!) {
      repository(owner: $owner, name: $name) { pullRequest(number: $number) {
        reviewThreads(first: 100) { nodes { isResolved path line
          comments(first: 1) { nodes { author { login } body } } } } } } }' \
    --jq '.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved | not)
      | "\(.path // "")\(if .line then ":\(.line)" else "" end) \(.comments.nodes[0].author.login // "?"): \((.comments.nodes[0].body // "") | split("\n")[0] | .[0:100])"' \
    2>/dev/null)
  if [[ -n $unresolved ]]; then
    echo "error: a conversation must be resolved before this pull request can be merged ($url):" >&2
    while IFS= read -r l; do echo "  $l"; done <<<"$unresolved" >&2
    echo "resolve them on GitHub, or set FORCE_ALLOW_UNRESOLVED=1" >&2
    exit 1
  fi
fi

# Another repository's PR: its branch name says nothing about this one's worktrees.
if [[ -n $repo_name ]]; then
  repo=(--repo "$repo_name")
  this=$("$real_gh" repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null)
  that=$("$real_gh" repo view "$repo_name" --json nameWithOwner -q .nameWithOwner 2>/dev/null)
  # gh prints the repository as GitHub spells it, whatever case -R had.
  [[ -n $this && $this == "$that" ]] || exec "$real_gh" "$@"
fi

pr() {
  "$real_gh" pr view "${selector[@]}" "${repo[@]}" --json state,headRefName \
    -q '"\(.state) \(.headRefName)"' 2>/dev/null
}
read -r state branch < <(pr)
[[ -n $branch ]] || exec "$real_gh" "$@"

# The worktree (not the primary checkout) with that branch.
wt='' path='' first=''
while read -r key value; do
  case $key in
    worktree) path=$value; [[ -z $first ]] && first=$path ;;
    branch) [[ $value == "refs/heads/$branch" && $path != "$first" ]] && wt=$path ;;
  esac
done < <("$real_git" worktree list --porcelain 2>/dev/null)
[[ -n $wt ]] || exec "$real_gh" "$@"
dirty=$("$real_git" -C "$wt" status --porcelain 2>/dev/null)

# Merging what's pushed while edits wait in the worktree: they'd be left behind.
if [[ $2 == merge && -n $dirty ]] && ! truthy "${FORCE_ALLOW_DIRTY_MERGE:-}"; then
  echo "warning: not merging: worktree $wt has uncommitted changes:" >&2
  while IFS= read -r l; do echo "  $l"; done <<<"$dirty" | head -20 >&2
  echo "commit and push them (or discard them) first, or set FORCE_ALLOW_DIRTY_MERGE=1" >&2
  exit 1
fi

"$real_gh" "$@"
status=$?

# `--delete-branch` may fail on the local branch (checked out in a worktree) after
# merging: what the PR is now decides, not gh's exit status.
read -r state branch < <(pr)
[[ $state == MERGED || $state == CLOSED ]] || exit "$status"

echo "PR $state: removing worktree $wt" >&2
if [[ -n $dirty ]]; then
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
