# shell/bash/bash.sh - product bash adapter (P1-16).  . shell/bash/bash.sh
# Reasoning, rejected alternatives and C10/P1-09 detail: shell/README.md.
# Short version: PROMPT_COMMAND + `builtin history 1` only (PS0 lags one
# command and DEBUG fires per simple command); no fork per command; every write
# guarded by `trap '' PIPE` plus a load-time probe that silences it.

[[ $- == *i* ]] || return 0 2>/dev/null || exit 0   # interactive only
[[ -n ${_LI_BASH_ON:-} ]] && return 0               # idempotent re-source
_LI_BASH_ON=1
_LI_EMIT=0      # 1 only after the sink probe succeeds
_LI_PEND=0      # bumped by PS0 before each command
_LI_SEEN=0
_LI_T0=0        # microseconds, from PS0
_LI_LAST_HC='none'
_LI_CMD=''
_LI_ESC=''
_LI_LINE=''
_LI_TS=''
_LI_PGRP=0 _LI_SID=0 _LI_STARTTIME=0
_LI_CAP_RECORD='{}'
_LI_HIST_FILE=${TMPDIR:-/tmp}/li-bash-history-$$
_LI_HISTORY_RAW=''

_li_sess() {
    local stat
    _LI_PID=$$
    stat=''
    if [[ -r /proc/$$/stat ]]; then
        IFS= read -r -d '' stat </proc/$$/stat || :
    fi
    if [[ -n $stat ]]; then
        # `comm` may hold spaces and parens, so split after `) `. Fields resume
        # at stat field 3 (state): pgrp=$3, session=$4, starttime(field 22)=$20.
        stat=${stat##*') '}
        set -- $stat
        _LI_PGRP=${3:-0} _LI_SID=${4:-0} _LI_STARTTIME=${20:-0}
    fi
    [[ $_LI_SID == "$$" ]] && _LI_NEW_SESSION=true || _LI_NEW_SESSION=false
    _LI_SESSION_ID=${LI_SESSION_ID:-li-bash-$$-$_LI_STARTTIME}
    export LI_SESSION_ID=$_LI_SESSION_ID
    # `tty` without -s opens the tty by name and BLOCKS forever when an inherited
    # fd is not a terminal (it hung 0-A's shell at load). The name is not needed
    # by any consumer, so it is reported as `none` instead of paying a fork.
    _LI_TTY=none
}

# Escapes into `_LI_ESC` using parameter expansion only (C9).
_li_esc() {
    local s=$1
    s=${s//\\/\\\\}; s=${s//\"/\\\"}
    s=${s//$'\n'/\\n}; s=${s//$'\t'/\\t}; s=${s//$'\r'/\\r}
    # Other control bytes make the line invalid JSON; replacing them keeps one
    # byte from losing the whole frame.
    s=${s//[$'\001'-$'\010'$'\013'$'\014'$'\016'-$'\037']/?}
    _LI_ESC=$s
}

_li_ts() {
    printf -v _LI_TS '%(%Y-%m-%dT%H:%M:%S)T' -1 2>/dev/null || _LI_TS=1970-01-01T00:00:00
    _LI_TS=${_LI_TS}.000Z   # protocol ts_wall is exactly millisecond precision
}

_li_req() {   # $1 type, $2 extra payload JSON (may be empty), $3 text source
    _li_esc "$_LI_CMD"; local cmd=$_LI_ESC
    _li_esc "$PWD";     local cwd=$_LI_ESC
    # The two closing braces are two statements on purpose: `${extra}}` expands
    # and eats one, producing a line with a missing `}` that lost every frame to
    # a JSON parse error. Measured.
    _LI_LINE="{\"type\":\"$1\",\"ts_wall\":\"$_LI_TS\",\"payload\":{\"command\":\"$cmd\",\"command_text_source\":\"$3\",\"cwd\":\"$cwd\",\"pid\":${_LI_PID:-$$},\"sid\":$_LI_SID,\"pgrp\":$_LI_PGRP,\"new_session\":$_LI_NEW_SESSION,\"tty\":\"$_LI_TTY\",\"shell\":\"bash\",\"shell_version\":\"${BASH_VERSION%%(*}\"${2-}"
    _LI_LINE="$_LI_LINE}"; _LI_LINE="$_LI_LINE}"
    _li_w
}

# Never fatal, never retried: 0-A section 8 condition 2.
_li_w() {
    (( _LI_EMIT )) || return 0
    trap '' PIPE                                    # C10 / P1-09, scoped
    if ! printf '%s\n' "$_LI_LINE" >&3 2>/dev/null; then
        _LI_EMIT=0
    fi
    trap - PIPE                                     # never leak SigIgn to children
    return 0
}

# Finish side is authoritative.
_li_finish() {
    local rc=$? hc=${HISTCMD:-0} text dur=0 src extra
    (( _LI_PEND == _LI_SEEN )) && return 0          # blank line / ^C at prompt
    _LI_SEEN=$_LI_PEND
    HISTTIMEFORMAT= builtin history 1 >"$_LI_HIST_FILE" 2>/dev/null
    IFS= read -r _LI_HISTORY_RAW <"$_LI_HIST_FILE" 2>/dev/null
    if [[ -n $_LI_HISTORY_RAW ]]; then
        text=${_LI_HISTORY_RAW#"${_LI_HISTORY_RAW%%[![:space:]]*}"}
        text=${text#"${text%%[!0-9]*}"}              # history number
        text=${text#"${text%%[![:space:]]*}"}        # gap after it
        _LI_CMD=$text
        src=history
        extra=",\"exit_status\":$rc,\"history_cmd\":$hc"
        # PS0 cannot emit without a command substitution (C9), so the start
        # event is written at the prompt from the tick PS0 captured *before*
        # the command ran. It is declared, and its ts_wall is the start second,
        # not the prompt's second.
        if [[ -n $_LI_CMD && ${LI_EMIT_STARTED:-0} == 1 ]]; then
            printf -v _LI_TS '%(%Y-%m-%dT%H:%M:%S)T' "$(( _LI_T0 / 1000000 ))" \
                2>/dev/null || _LI_TS=1970-01-01T00:00:00
            _LI_TS=${_LI_TS}.000Z
            _li_req CommandStarted ",\"prev_status\":0" history
        fi
    else
        # Never guess a command when HISTCONTROL dropped the line.
        _LI_CMD=''
        src=unavailable
        extra=",\"exit_status\":$rc,\"history_cmd\":$hc,\"command_unavailable\":\"history_dropped_by_histcontrol\""
    fi
    (( _LI_T0 > 0 )) && dur=$(( (${EPOCHREALTIME/./} - _LI_T0) / 1000 ))
    _li_ts
    _li_req CommandFinished "$extra,\"duration_ms\":$dur" "$src"
    _LI_T0=0
    return 0
}

_li_arm()    { _LI_EMIT=1; }
_li_disarm() { _LI_EMIT=0; }
_li_session_started() {
    _li_ts; _LI_CMD=''
    _li_req SessionStarted ",\"capability_record\":$_LI_CAP_RECORD" launcher
}
_li_off() {
    PROMPT_COMMAND=${PROMPT_COMMAND//_li_finish/}
    PROMPT_COMMAND=${PROMPT_COMMAND//builtin history 1/}
    PS0=''
    unset _LI_BASH_ON
    _li_disarm
}

_li_sess
_LI_CAP_FILE=${LI_CAPABILITY_FILE:-${BASH_SOURCE[0]%/*}/../../capabilities/bash.json}
IFS= read -r _LI_CAP_RECORD < "$_LI_CAP_FILE" 2>/dev/null || _LI_CAP_RECORD='{}'
# The scratch file is intentionally left for the next prompt to truncate; an
# EXIT trap would invoke external `rm` and violate the fork-free hot-path rule.

# One-shot probe. `: >&3` writes nothing but still reports EBADF/EPIPE, so a
# broken sink silences the adapter at load, not on the first command.
if [[ -n ${LI_SINK_FD:-} ]] && : >&3 2>/dev/null; then _li_arm; fi

_LI_DUMMY=()
# `history 1` and `read` are both builtins inside `_li_finish` AFTER `local rc=$?`
# so the user's exit status is preserved before builtin redirections run (C9).
PROMPT_COMMAND="_li_finish${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
_LI_PS0_TICK='${_LI_DUMMY[$((_LI_PEND+=1,_LI_T0=${EPOCHREALTIME/./}))]}'
PS0=$_LI_PS0_TICK
_li_session_started
true
