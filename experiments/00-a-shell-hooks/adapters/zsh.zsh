# adapters/zsh.zsh -- minimal interactive zsh hook adapter (experiment 00-a)
#
#   exec 3>...            # launcher hands over a writable FD 3
#   source adapters/zsh.zsh
#
# zsh is the cleanest of the three shells: preexec() runs before the command and
# receives the command line as $1 (already multi-line-joined), precmd() runs
# before the next prompt with the real $? in $?. add-zsh-hook keeps both
# lists idempotent, which is what makes re-`source` safe.
#
# Facts measured here (results/):
#  * $1 in preexec is the command line *as typed*, before alias expansion, and
#    it is the whole multi-line compound command, not one line.
#  * $EPOCHREALTIME requires zsh/datetime; the module path must point at the
#    local unpacked tree (module_path) because zsh is not installed here.
#  * zsh has no CLOCK_MONOTONIC builtin; only wall clock.
#  * A write to a dead pipe is fatal, same as bash -> `trap '' PIPE` guard
#    would be needed; instead we rely on a non-blocking design: the sink is a
#    socket connected by the launcher, and EAGAIN/EPIPE is swallowed.
#  * zsh CAN open an AF_UNIX socket for writing (zsh/net/socket `zsocket`
#    builtin) -- tested separately, see REPORT.md section 4.

[[ -o interactive ]] || return 0
[[ -n ${_LI_ZSH_ON:-} ]] && { return 0; }     # idempotent
typeset -g _LI_ZSH_ON=1

typeset -g _LI_SEQ=0
typeset -g _LI_T0=0
typeset -g _LI_FD3_OK=0
typeset -g _LI_BUSY=0

# zsh is not installed on this host: the binary is an unpacked copy, so its
# module:// directory must be added explicitly or zsh/datetime and zsh/parameter
# fail to load and $EPOCHREALTIME stays empty (measured: results/t01.zsh.term.log).
: ${LI_ZSH_MODULE_PATH:=<WORKSPACE>/experiments/toolchain/root/usr/lib/zsh/5.9.2}
typeset -ga module_path=($LI_ZSH_MODULE_PATH $module_path)
zmodload zsh/datetime 2>/dev/null || true
zmodload zsh/parameter 2>/dev/null || true

# --- sink write -------------------------------------------------------------
_li_w() {
  (( _LI_FD3_OK )) || return 1
  { print -r -- "$1" >&3 } 2>/dev/null || { _LI_FD3_OK=0; return 1 }
  return 0
}

_li_esc() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\t'/\\t}
  s=${s//$'\r'/\\r}
  print -rn -- "$s"
}

# zsh: strftime builtin from zsh/datetime, microsecond-resolution input
_li_ts() {
  local frac=${EPOCHREALTIME#*.}
  local sec=${EPOCHREALTIME%.*}
  strftime -s _LI_TS '%Y-%m-%dT%H:%M:%S' $sec 2>/dev/null || _LI_TS=$sec
  _LI_TS=$_LI_TS.$frac'Z'
}

# zsh EPOCHREALTIME carries **nanosecond** precision (19 digits), so the obvious
# ${EPOCHREALTIME/./} subtraction overflows 64-bit signed arithmetic and yields
# garbage durations (measured: "number truncated after 19 digits",
# results/t-probe-zsh-overflow.log). Truncate the fraction to microseconds first.
# NOTE: on this build the fraction is 10 digits (nanosecond *tenths*), not 9;
# taking [1,6] therefore drops one digit and yields values 10x too small. The
# length is measured, not assumed (results/probe-zsh-overflow.log).
_li_now_us() {
  local i=${EPOCHREALTIME%.*} f=${EPOCHREALTIME#*.}
  f=${(r:6::0:)f[1,6]}
  print -rn -- $(( i * 1000000 + f * 1 ))
}

_li_sess() {
  typeset -g _LI_PID=$$
  typeset -g _LI_SESSION_ID=${LI_SESSION_ID:-li-zsh-$$}
  # %(sid) is not a prompt escape; read /proc instead (one read, no fork)
  local stat=$(</proc/$$/stat 2>/dev/null)
  if [[ -n $stat ]]; then
    stat=${stat##*') '}
    local -a f=(${=stat})
    typeset -g _LI_PGRP=$f[3]
    typeset -g _LI_SID=$f[4]
  else
    typeset -g _LI_PGRP=$_LI_PID; typeset -g _LI_SID=$_LI_PID
  fi
  [[ $_LI_SID == $$ ]] && typeset -g _LI_NEW_SESSION=true \
                       || typeset -g _LI_NEW_SESSION=false
  typeset -g _LI_TTY=$(tty 2>/dev/null) || typeset -g _LI_TTY=none
}

# --- preexec: true start, and the authoritative command text ---------------
_li_preexec() {
  (( _LI_BUSY )) && return 0       # never recurse into our own helpers
  _LI_BUSY=1
  _LI_T0=$(_li_now_us)
  typeset -g _LI_CMD=$1
  if [[ ${LI_EMIT_STARTED:-0} == 1 ]]; then
    _li_ts
    (( _LI_SEQ++ ))
    _li_w "{\"schema_version\":1,\"source\":\"shell.zsh\",\"session_id\":\"$_LI_SESSION_ID\",\"sequence\":$_LI_SEQ,\"ts_wall\":\"$_LI_TS\",\"type\":\"CommandStarted\",\"payload\":{\"command\":\"$(_li_esc "$_LI_CMD")\",\"command_text_source\":\"preexec\",\"cwd\":\"$(_li_esc "$PWD")\",\"pid\":$$},\"evidence_status\":\"declared\"}"
  fi
  _LI_BUSY=0
  return 0
}

# --- precmd: end time, real status ----------------------------------------
_li_precmd() {
  local rc=$?
  (( _LI_BUSY )) && return 0
  (( _LI_T0 == 0 )) && return 0            # nothing ran since the last prompt
  _LI_BUSY=1
  local dur=0 now=$(_li_now_us)
  (( _LI_T0 > 0 )) && dur=$(( (now - _LI_T0) / 1000 ))
  _li_ts
  (( _LI_SEQ++ ))
  _li_w "{\"schema_version\":1,\"source\":\"shell.zsh\",\"session_id\":\"$_LI_SESSION_ID\",\"sequence\":$_LI_SEQ,\"ts_wall\":\"$_LI_TS\",\"type\":\"CommandFinished\",\"payload\":{\"command\":\"$(_li_esc "${_LI_CMD-}")\",\"command_text_source\":\"preexec\",\"exit_status\":$rc,\"duration_ms\":$dur,\"cwd\":\"$(_li_esc "$PWD")\",\"pid\":$$,\"sid\":${_LI_SID:-0},\"pgrp\":${_LI_PGRP:-0},\"new_session\":${_LI_NEW_SESSION:-false},\"tty\":\"$_LI_TTY\"},\"evidence_status\":\"declared\"}"
  _LI_T0=0
  typeset -g _LI_CMD=""
  _LI_BUSY=0
  return 0
}

_li_zsh_off() {
  add-zsh-hook -d preexec _li_preexec 2>/dev/null
  add-zsh-hook -d precmd _li_precmd 2>/dev/null
  unset _LI_ZSH_ON
}

# fpath must include the unpacked tree too, else `autoload add-zsh-hook` fails
# with "function definition file not found" (measured: results/t01.zsh.term.log).
: ${LI_ZSH_FPATH:=<WORKSPACE>/experiments/toolchain/root/usr/share/zsh/functions/Misc}
typeset -ga fpath=($LI_ZSH_FPATH $fpath)
autoload -Uz add-zsh-hook 2>/dev/null
_li_sess
: >&3 2>/dev/null && _LI_FD3_OK=1
if whence -w add-zsh-hook >/dev/null 2>&1; then
  add-zsh-hook preexec _li_preexec
  add-zsh-hook precmd  _li_precmd
else
  preexec_functions+=( _li_preexec )
  precmd_functions+=( _li_precmd )
fi
true
