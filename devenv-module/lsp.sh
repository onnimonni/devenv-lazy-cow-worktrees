#!/usr/bin/env bash
# `@name@` in the devenv shell (a supported language server): started as a language
# server (with the entry's arguments first, e.g. Claude Code's LSP plugins, which run
# their server by name from PATH), it runs behind `lazy-cow-tree lsp`: one server per
# worktree, each request answered by the worktree of the file it's about. Any other
# use (`@name@ --version`) is the real one.
#
#   LAZY_COW_TREE_LSP_DISABLE=1  always the real one

real=@real@
lazy_cow_tree=@lazyCowTree@
lsp_args=(@lspArgs@)

# The next @name@ in PATH after this wrapper.
if [[ -z $real ]]; then
  IFS=: read -ra dirs <<<"$PATH"
  for dir in "${dirs[@]}"; do
    if [[ -x $dir/@name@ && ! $dir/@name@ -ef $0 ]]; then
      real=$dir/@name@
      break
    fi
  done
  [[ -n $real ]] || { echo "lazy-cow-tree: no @name@ in PATH besides this wrapper" >&2; exit 127; }
fi

# A language server start: the entry's arguments first (no arguments: exactly none).
lsp=1
if ((${#lsp_args[@]} == 0)); then
  (($# == 0)) || lsp=
else
  for i in "${!lsp_args[@]}"; do
    [[ ${*:i+1:1} == "${lsp_args[i]}" ]] || lsp=
  done
fi
if [[ -n $lsp && -z ${LAZY_COW_TREE_LSP_DISABLE:-} ]]; then
  exec "$lazy_cow_tree" lsp -- "$real" "$@"
fi
exec "$real" "$@"
