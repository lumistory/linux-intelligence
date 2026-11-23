# adapters/bash.sh -- minimal interactive bash hook adapter (experiment 00-a)
#
#   exec 3>...   # launcher must hand over a writable FD 3 first
#   . adapters/bash.sh
#
# Emits one-line JSON to FD 3. Every design choice below is backed by a log in
# results/ (filenames cited):
#
#  * SIGPIPE: a write to a dead *pipe* terminates the shell (results/
#    t-probe-bash-sigpipe.log). All writes are guarded by `trap '' PIPE`
#    installed and removed around the write, so the ignore flag is not
#    inherited by user commands.
#  * bash cannot open an AF_UNIX stream socket by redirection: any socket path
#    yields ENXIO (results/t-probe-bash-unixsock.log). FD 3 therefore must be
#    connected by the launcher; the adapter never opens the sink.
#  * DEBUG trap rejected: fires once per *simple* command, so `a | b` and
#    `a && b` emit more than one event for one command line, it fires inside
#    functions and subshells, and it is suppressed inside $( ) -- see
#    results/t07.bash.debugtrap.log and REPORT.md section 5.
#  * PS0 is a *subshell-free* start tick, but assignments written as
#    ${v:=...} inside a plain expansion do NOT persist while an assignment
#    inside an arithmetic subscript does (results/t-probe-bash-ps0sub.log).
#    Hence PS0 is: ${_LI_DUMMY[$((_LI_PEND+=1,_LI_T0=${EPOCHREALTIME/./}))]}
#    which yields the empty string, so the prompt stays visually clean.
#  * Command text comes from `history 1`, the only place bash keeps the
#    multi-line-joined line. That costs exactly one fork per command (~190 us
#    measured, results/t-probe-bash-costs.log); no other fork is used.
#
# Deliberate side effects: PROMPT_COMMAND is prepended, PS0 is set, PIPE is
# briefly ignored during writes. Undo: _li_bash_off

[[ $- == *i* ]] || { return 0 2>/dev/null || exit 0; }     # interactive only
[[ -n ${_LI_BASH_ON:-} ]] && { return 0; }                 # idempotent re-source

_LI_BASH_ON=1
_LI_VER=${BASH_VERSION%%(*}
_LI_SEQ=0
_LI_PSEQ=0
_LI_PEND=0            # incremented by PS0 before each command
_LI_SEEN=0
_LI_T0=0              # microseconds, from PS0
_LI_FD3_OK=0
_LI_NOSINK=0

# --- sink write, never fatal ------------------------------------------------
_li_w() {  # $1 = complete JSON line
  [[ $_LI_FD3_OK == 1 ]] || return 1
  trap '' PIPE
  if ! printf '%s\n' "$1" >&3 2>/dev/null; then
    _LI_FD3_OK=0          # give up once, stay silent afterwards
    trap - PIPE
    return 1
  fi
  trap - PIPE
  return 0
}

_li_esc() {  # JSON string escape, no fork
  local s=$1
  s=${s//\\/\\\\}; s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}; s=${s//$'\t'/\\t}; s=${s//$'\r'/\\r}
  printf '%s' "$s"
}

_li_ts() {   # ISO-8601 UTC, strftime builtin, no fork
  local sec=${EPOCHREALTIME%.*} frac=${EPOCHREALTIME#*.}
  printf -v _LI_TS '%(%Y-%m-%dT%H:%M:%S)T' "$sec" 2>/dev/null || _LI_TS=$sec
  _LI_TS=$_LI_TS.$frac'Z'
}

_li_sess() { # facts that never change during the session
  local stat
  _LI_PID=$$
  stat=$(</proc/$$/stat 2>/dev/null) || stat=""
  if [[ -n $stat ]]; then
    stat=${stat##*') '}
    set -- $stat
    _LI_PGRP=$3
    _LI_SID=$4
  else
    _LI_PGRP=$_LI_PID; _LI_SID=$_LI_PID
  fi
  [[ $_LI_SID == "$$" ]] && _LI_NEW_SESSION=true || _LI_NEW_SESSION=false
  _LI_SESSION_ID=${LI_SESSION_ID:-li-bash-$$}
  # `tty` (no -s) opens the terminal by name and BLOCKS forever when an
  # inherited FD (3) is not a tty -- it hung the shell during adapter load.
  # `tty -s` only queries and returns a status, so it never blocks.
  if tty -s 2>/dev/null; then _LI_TTY=$(tty 2>/dev/null || echo tty); else
    _LI_TTY=none; fi
}

# --- finish side: authoritative --------------------------------------------
_li_finish() {
  local rc=$? hc=${HISTCMD:-0} raw text dur=0 have_text=true
  (( _LI_PEND == _LI_SEEN )) && return 0   # no PS0 => blank line / ^C at prompt
  _LI_SEEN=$_LI_PEND

  if [[ $hc != "${_LI_LAST_HC:-none}" ]]; then
    _LI_LAST_HC=$hc
    raw=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)   # the only fork
    text=${raw#"${raw%%[![:space:]]*}"}
    text=${text#"${text%%[!0-9]*}"}       # drop history number
    text=${text#"${text%%[![:space:]]*}"} # and the gap after it
  else
    have_text=false                        # history disabled: text unknown
    text=""
  fi

  (( _LI_T0 > 0 )) && dur=$(( (${EPOCHREALTIME/./} - _LI_T0) / 1000 ))
  _li_ts
  _LI_SEQ=$(( _LI_SEQ + 1 ))
  if $have_text; then
    _li_w "$(printf '{"schema_version":1,"source":"shell.bash","session_id":"%s","sequence":%d,"ts_wall":"%s","type":"CommandFinished","payload":{"command":"%s","command_text_source":"history","exit_status":%d,"duration_ms":%d,"cwd":"%s","pid":%d,"sid":%s,"pgrp":%s,"new_session":%s,"tty":"%s","history_cmd":%d},"evidence_status":"declared"}' \
      "$_LI_SESSION_ID" "$_LI_SEQ" "$_LI_TS" "$(_li_esc "$text")" "$rc" "$dur" \
      "$(_li_esc "$PWD")" "$$" "${_LI_SID:-0}" "${_LI_PGRP:-0}" \
      "$_LI_NEW_SESSION" "$_LI_TTY" "$hc")"
  else
    _li_w "$(printf '{"schema_version":1,"source":"shell.bash","session_id":"%s","sequence":%d,"ts_wall":"%s","type":"CommandFinished","payload":{"command":null,"command_text_source":"unavailable","exit_status":%d,"duration_ms":%d,"cwd":"%s","pid":%d,"sid":%s,"pgrp":%s,"new_session":%s,"tty":"%s","history_cmd":%d},"evidence_status":"declared"}' \
      "$_LI_SESSION_ID" "$_LI_SEQ" "$_LI_TS" "$rc" "$dur" \
      "$(_li_esc "$PWD")" "$$" "${_LI_SID:-0}" "${_LI_PGRP:-0}" \
      "$_LI_NEW_SESSION" "$_LI_TTY" "$hc")"
  fi
  return 0
}

# --- start side: PS0, only when the caller wants an explicit start event ----
_li_ps0_emit() {
  local prev_rc=$? prev_cmd=$BASH_COMMAND
  _li_ts
  _LI_PSEQ=$(( _LI_PSEQ + 1 ))
  _LI_SEQ=$(( _LI_SEQ + 1 ))
  _li_w "$(printf '{"schema_version":1,"source":"shell.bash","session_id":"%s","sequence":%d,"seq_start":%d,"ts_wall":"%s","type":"CommandStarted","payload":{"cwd":"%s","pid":%d,"prev_status":%d,"prev_command":"%s"},"evidence_status":"declared"}' \
    "$_LI_SESSION_ID" "$_LI_SEQ" "$_LI_PSEQ" "$_LI_TS" "$(_li_esc "$PWD")" \
    "$$" "$prev_rc" "$(_li_esc "$prev_cmd")")"
}

_li_bash_off() {
  PROMPT_COMMAND=${PROMPT_COMMAND//_li_finish}
  PROMPT_COMMAND=${PROMPT_COMMAND//;/}
  PS0=''
  unset _LI_BASH_ON
}

# --- install -----------------------------------------------------------------
_li_sess
: >&3 2>/dev/null && _LI_FD3_OK=1 || _LI_NOSINK=1
# start tick + pending counter, zero bytes on screen, no fork
PS0='${_LI_DUMMY[$((_LI_PEND+=1,_LI_T0=${EPOCHREALTIME/./}))]}'
[[ ${LI_EMIT_STARTED:-0} == 1 ]] && PS0='${_LI_DUMMY[$((_LI_PEND+=1,_LI_T0=${EPOCHREALTIME/./}))]}$(_li_ps0_emit)'
PROMPT_COMMAND="_li_finish${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
true
