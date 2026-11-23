# adapters/bash.sh -- minimal interactive bash hook adapter (experiment 00-a)
# Source from an INTERACTIVE bash:  . adapters/bash.sh
#
# Emits one-line JSON to FD 3. Design constraints proven by experiment 00-a:
#   * a write to a dead *pipe* is fatal for the shell (SIGPIPE) -> trap '' PIPE
#   * bash cannot open a unix stream socket via redirection (ENXIO) -> FD 3 must
#     already be connected by whoever launched the shell
#   * bash has wall clock only (EPOCHREALTIME), no CLOCK_MONOTONIC without a fork
#   * DEBUG trap is NOT used (fires per simple command, breaks pipelines) --
#     PS0 gives the start tick, PROMPT_COMMAND+history gives text/status/end.
#
# Side effects on the shell, all deliberate: PROMPT_COMMAND is prepended,
# PS0 is set, history is cleared at load, `trap '' PIPE` is installed.
# Undo: _li_bash_disable

[[ $- == *i* ]] || return 0 2>/dev/null || exit 0
[[ -n ${_LI_BASH_LOADED:-} ]] && { _li_bash_ret=0; return 0; }
_LI_BASH_LOADED=1
_LI_SEQ=0
_LI_PSEQ=0
_LI_LAST_HISTCMD=-1
_LI_T0=""
_LI_FD_OK=1

# JSON string escaper, pure parameter expansion (no fork per command).
_li_esc() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\t'/\\t}
  s=${s//$'\r'/\\r}
  printf '%s' "$s"
}

# Session facts, computed once: cwd, pid, sid, whether we own a new session.
_li_sess_init() {
  _LI_PID=$$
  local sid new=0
  sid=$(ps -o sid= -p $$ 2>/dev/null | tr -d ' ')
  _LI_SID=${sid:-$PPID}
  [[ ${_LI_SID:-0} == "$$" ]] && new=1
  _LI_NEW_SESSION=$new
  _LI_SESSION_ID=${LI_SESSION_ID:-li-bash-$$}
  _LI_TTY=$(tty 2>/dev/null || echo none)
}

_li_emit() {   # $1=type  $2=payload-json-fragment (already escaped)
  _LI_SEQ=$((_LI_SEQ+1))
  printf '{"schema_version":1,"source":"shell.bash","session_id":"%s","sequence":%d,"ts_wall":"%s","type":"%s","payload":%s,"evidence_status":"declared"}\n' \
    "$_LI_SESSION_ID" "$_LI_SEQ" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$1" "$2" >&3 2>/dev/null || _LI_FD_OK=0
}

_li_ps0() {    # runs BEFORE the command: previous rc + previous command
  _LI_T0=$EPOCHREALTIME
  [[ ${PS0_START:-0} == 1 ]] && _li_ps0_emit
  return 0
}
_li_ps0_emit() {
  _LI_PSEQ=$((_LI_PSEQ+1))
  printf '{"schema_version":1,"source":"shell.bash","session_id":"%s","sequence":%d,"ts_wall":"%s","type":"CommandStarted","payload":{"start_epochrealtime":"%s","prev_status":%s,"prev_bash_command":"%s"},"evidence_status":"declared"}\n' \
    "$_LI_SESSION_ID" "$_LI_PSEQ" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$_LI_T0" "${_LI_PS0_RC:-0}" "$(_li_esc "${_LI_PS0_CMD:-}")" >&3 2>/dev/null || _LI_FD_OK=0
}

_li_finish() {  # runs in PROMPT_COMMAND: real status + command text
  local rc=$? hc=${HISTCMD:-0} text dur_ms=0
  [[ $hc == "$_LI_LAST_HISTCMD" ]] && return 0          # no new command
  _LI_LAST_HISTCMD=$hc
  text=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)
  text=${text#"${text%%[![:space:]]*}"}                  # lstrip
  text=${text#*[0-9] }; text=${text#*[0-9] }; text=${text#*[0-9] }  # "  123  "
  text=${text#"${text%%[![:space:]]*}"}
  [[ -n $_LI_T0 ]] && dur_ms=$(( (${EPOCHREALTIME/./}-${_LI_T0/./})/1000 ))
  _li_emit CommandFinished "$(printf '{"command":"%s","exit_status":%d,"duration_ms":%d,"cwd":"%s","pid":%d,"sid":%s,"new_session":%s,"history_cmd":%d}' \
     "$(_li_esc "$text")" "$rc" "$dur_ms" "$(_li_esc "$PWD")" "$$" "${_LI_SID:-null}" "${_LI_NEW_SESSION:-false}" "$hc")"
  _LI_T0=""
  return 0
}

_li_bash_disable() {
  PROMPT_COMMAND=${PROMPT_COMMAND//_li_finish;/}
  PROMPT_COMMAND=${PROMPT_COMMAND//_li_finish/}
  PS0=""
  unset _LI_BASH_LOADED
}

_li_sess_init
# One pre-flight write: if FD 3 is not writable we stop emitting forever.
{ printf '' >&3 ; } 2>/dev/null || _LI_FD_OK=0
trap '' PIPE 2>/dev/null || true
PS0='${_LI_T0:0}${_LI_PS0_RC:=$?}${_LI_PS0_CMD:=$BASH_COMMAND}'
# PS0 above records state at zero visual footprint (expands to empty).
PS0='$( _li_ps0_rc=$? , _LI_PS0_RC=$? , _LI_T0=$EPOCHREALTIME , _LI_PS0_LAST=$BASH_COMMAND )'
