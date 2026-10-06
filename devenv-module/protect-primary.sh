# shellcheck shell=bash disable=SC1003,SC2154,SC2296
# (zsh's parameter flags and variables, in its branch; '\' is a backslash.)
# lazyCowTree.protectPrimary: sourced by the shell hook (bash and zsh) when enabled.
# In the primary checkout (subdirectories too, not worktrees nor other repositories
# inside it) a command typed at the prompt or given to `bash -c` / `zsh -c` (agents'
# tool shells; bash 3.2 and 5) runs only if it starts with the words of an allowed entry: 'git pull'
# allows `git pull --rebase`, not `git push`. Scripts run as files (git hooks, tools'
# scripts) aren't checked. A guardrail against mistakes, not a security boundary.

__lct_root=$(cd @root@ 2>/dev/null && pwd -P)
__lct_allowed=(@allowed@)

# __lct_main: the primary's root when <dir> is in it, cached per <dir>. The nearest
# .git above it is the root's own directory: a worktree's, a submodule's or a nested
# repository's comes first.
__lct_refresh() {
  [[ $1 == "${__lct_dir-}" ]] && return
  __lct_dir=$1 __lct_main=
  local d
  d=$(builtin cd -- "$1" 2>/dev/null && pwd -P) || return 0
  while [[ -n $d && ! -e $d/.git ]]; do d=${d%/*}; done
  [[ $d == "$__lct_root" && -d $d/.git ]] && __lct_main=$d
}

# Succeeds when the command (words after <dir>, where it runs) may run there.
__lct_allowed_cmd() {
  __lct_refresh "$1"
  shift
  [[ -z $__lct_main ]] && return 0
  local entry list=
  for entry in "${__lct_allowed[@]}"; do
    [[ "$* " == "$entry "* ]] && return 0
    list+="${list:+, }$entry"
  done
  echo "'$*' is not allowed in the primary checkout $__lct_main (allowed: $list)." >&2
  echo "Work in a worktree: \`git worktree add @worktreesDir@/<name>\`, then cd into it." >&2
  return 1
}

# A command line, checked before it runs: split at ; & && || | and newlines (not
# inside quotes nor in redirections like 2>&1), each command where it runs,
# following its cds. Plain bash 3.2.
__lct_check_line() {
  local s=$1 dir=$PWD seg='' q='' c i n=${#1}
  for ((i = 0; i <= n; i++)); do
    c=${s:i:1}
    if [[ -n $q ]]; then
      [[ $c == '\' && $q == '"' ]] && { seg+=$c${s:i+1:1}; ((i++)); continue; }
      [[ $c == "$q" ]] && q=
      seg+=$c
      continue
    fi
    case $c in
      "'" | '"') q=$c; seg+=$c ;;
      '\') seg+=$c${s:i+1:1}; ((i++)) ;;
      '&' | '|')
        if [[ ${s:i-1:1} == [\<\>] || ${s:i+1:1} == '>' ]]; then seg+=$c; else
          __lct_check_segment || return 1
          seg=
        fi ;;
      ';' | $'\n' | '')
        __lct_check_segment || return 1
        seg= ;;
      *) seg+=$c ;;
    esac
  done
}

# One command of __lct_check_line (its `seg`, `dir`).
__lct_check_segment() {
  local -a w
  local t
  read -ra w <<<"$seg"
  ((${#w[@]})) || return 0
  __lct_allowed_cmd "$dir" "${w[@]}" || return 1
  if [[ ${w[0]} == cd ]]; then
    t=${w[1]:-$HOME}
    t=${t//\"/}
    t=${t//\'/}
    [[ $t == '~'* ]] && t=$HOME${t#'~'}
    [[ $t == /* ]] && dir=$t || dir=$dir/$t
  fi
}

if [[ -n ${BASH_VERSION:-} ]]; then
  if [[ -n ${BASH_EXECUTION_STRING:-} ]]; then
    # bash -c: the whole line before any of it runs. (A DEBUG trap can only skip
    # commands with extdebug on, and set while bash starts it makes bash 5 look
    # for its debugger, bashdb, and turn debugging mode off.)
    __lct_check_line "$BASH_EXECUTION_STRING" || exit 1
  elif [[ $- == *i* ]]; then
    __lct_check() {
      # Only commands typed: not those of functions (completion, prompt helpers,
      # the shell hook's cd), of startup files (`source`) nor of PROMPT_COMMAND.
      ((${#FUNCNAME[@]} > 1)) && return 0
      [[ $BASH_COMMAND == '__lct_in_prompt=1' || -n ${__lct_in_prompt-} ]] && return 0
      local -a words
      read -ra words <<<"$BASH_COMMAND"
      __lct_allowed_cmd "$PWD" "${words[@]}"
    }
    # With extdebug, a DEBUG trap failing skips the command (interactive shells
    # never start bash's debugger).
    shopt -s extdebug
    PROMPT_COMMAND="__lct_in_prompt=1; ${PROMPT_COMMAND:+${PROMPT_COMMAND%;}; }__lct_in_prompt="
    trap '__lct_check' DEBUG
  fi
elif [[ -n ${ZSH_VERSION:-} ]]; then
  if [[ -o interactive || -n ${ZSH_EXECUTION_STRING:-} ]]; then
    # zsh's trap gets a whole list (`cd x && make`): each command is checked where
    # it runs, following its cds. Setting ERR_EXIT in a DEBUG trap skips the list
    # (DEBUG_BEFORE_CMD, the default).
    TRAPDEBUG() {
      # Only commands typed (toplevel) or given to -c (cmdarg): not those of
      # functions nor of startup files (zsh_eval_context: file).
      [[ ${#zsh_eval_context} == 2 ]] || return 0
      [[ ${zsh_eval_context[1]} == toplevel || ${zsh_eval_context[1]} == cmdarg ]] || return 0
      local dir=$PWD w
      local -a cmd
      for w in ${(z)ZSH_DEBUG_CMD} ';'; do
        case $w in
          ('&&' | '||' | '|' | '|&' | ';' | '&' | $'\n')
            if ((${#cmd})); then
              __lct_allowed_cmd "$dir" "${cmd[@]}" || { setopt ERR_EXIT; return 0; }
              if [[ ${cmd[1]} == cd ]]; then
                w=${cmd[2]:-$HOME}
                [[ $w == '~'* ]] && w=$HOME${w#'~'}
                [[ $w == /* ]] && dir=$w || dir=$dir/$w
              fi
            fi
            cmd=() ;;
          (*) cmd+=("${(Q)w}") ;;
        esac
      done
    }
  fi
fi
