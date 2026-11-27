# shell/zsh/zsh.zsh - product zsh adapter (P1-17).  . shell/zsh/zsh.zsh
# Reasoning: shell/README.md. preexec/precmd; the `*_functions` arrays are
# appended to directly because add-zsh-hook would erase an already-defined
# function (0-A F14); EPOCHREALTIME's 10-digit fraction is truncated first.

[[ -o interactive ]] || return 0
[[ -n ${_LI_ZSH_ON:-} ]] && return 0
typeset -g _LI_ZSH_ON=1

# Only used when zsh is unpacked outside a standard prefix; a system zsh needs
# neither line and both are no-ops.
if [[ -n ${LI_ZSH_MODULE_PATH:-} ]]; then
  typeset -ga module_path=($LI_ZSH_MODULE_PATH $module_path)
elif [[ ${ZSH_VERSION:-} == 5.9* ]]; then
  typeset -ga module_path=("${${(%):-%N}:A:h}/../../experiments/toolchain/root/usr/lib/zsh/5.9.2" $module_path)
fi
if [[ -n ${LI_ZSH_FPATH:-} ]]; then
  typeset -ga fpath=($LI_ZSH_FPATH $fpath)
elif [[ ${ZSH_VERSION:-} == 5.9* ]]; then
  typeset -ga fpath=("${${(%):-%N}:A:h}/../../experiments/toolchain/root/usr/share/zsh/functions/Misc" $fpath)
fi
zmodload zsh/datetime 2>/dev/null || true

typeset -g _LI_EMIT=0 _LI_T0=0 _LI_BUSY=0 _LI_SID=0 _LI_PGRP=0 _LI_STARTTIME=0
typeset -g _LI_CMD='' _LI_ESC='' _LI_LINE='' _LI_TS='' _LI_TTY=none
typeset -g _LI_CAP_RECORD='{}'

_li_sess() {
    typeset -g _LI_PID=$$
    local stat=''
    IFS= read -r -d '' stat </proc/$$/stat 2>/dev/null || :
    if [[ -n $stat ]]; then
        # Fields resume at stat field 3 (state) after `comm) `: pgrp=f[3],
        # session=f[4], starttime (field 22) = f[20]. `:-0` keeps JSON numeric.
        stat=${stat##*') '}
        local -a f=(${=stat})
        _LI_PGRP=${f[3]:-0} _LI_SID=${f[4]:-0} _LI_STARTTIME=${f[20]:-0}
    fi
    [[ $_LI_SID == $$ ]] && typeset -g _LI_NEW_SESSION=true \
                         || typeset -g _LI_NEW_SESSION=false
    typeset -g _LI_SESSION_ID=${LI_SESSION_ID:-li-zsh-$$-$_LI_STARTTIME}
    export LI_SESSION_ID=$_LI_SESSION_ID
    # No `tty` call: an external command here would fork once per session, and
    # `tty` without -s blocks when the inherited fd is not a terminal. No
    # consumer needs the name, so it is reported as `none`.
    typeset -g _LI_TTY=none
}

# Result in _LI_ESC, no fork (C9).
_li_esc() {
    local s=$1
    s=${s//\\/\\\\}; s=${s//\"/\\\"}
    s=${s//$'\n'/\\n}; s=${s//$'\t'/\\t}; s=${s//$'\r'/\\r}
    s=${s//[$'\001'-$'\010'$'\013'$'\014'$'\016'-$'\037']/?}
    _LI_ESC=$s
}

# strftime is a zsh/datetime builtin, so this is fork-free when the module loads.
# The protocol ts_wall is exactly millisecond precision (`YYYY-MM-DDTHH:MM:SS.mmmZ`,
# enforced by li-proto), so the 10-digit fraction is cut to 3.
_li_ts() {
    local sec=${EPOCHREALTIME%%.*} frac=${EPOCHREALTIME#*.}
    frac=${frac[1,3]}
    if [[ -n $sec && $sec == <-> ]]; then
        strftime -s _LI_TS '%Y-%m-%dT%H:%M:%S' $sec 2>/dev/null || _LI_TS=$sec
    else
        _LI_TS=1970-01-01T00:00:00; frac=000
    fi
    _LI_TS=${_LI_TS}.${(r:3::0:)frac}Z
}

# Microseconds since the epoch, computed without a command substitution.
#
# EPOCHREALTIME's fraction is 10 digits on this build (tenths of a nanosecond),
# so the first 6 digits are exactly the microseconds; taking all 19 digits into
# one integer overflows 64-bit (0-A probe-zsh-overflow.log). The result lands in
# _LI_NOW_US, which is a global on purpose: command substitution would fork.
_li_now_us() {
    local i=${EPOCHREALTIME%%.*} f=${EPOCHREALTIME#*.}
    (( _LI_NOW_US = i * 1000000 + ${f[1,6]} ))
}

_li_w() {
    (( _LI_EMIT )) || return 0
    trap '' PIPE                            # C10 / P1-09, scoped to the write
    if ! { print -r -- "$_LI_LINE" >&3 } 2>/dev/null; then
        _LI_EMIT=0
    fi
    trap - PIPE
    return 0
}

_li_req() {   # $1 type, $2 extra payload JSON, $3 text source
    _li_esc "$_LI_CMD"; local cmd=$_LI_ESC
    _li_esc "$PWD";     local cwd=$_LI_ESC
    # Two braces as two statements: `${extra}}` expands and eats one.
    _LI_LINE="{\"type\":\"$1\",\"ts_wall\":\"$_LI_TS\",\"payload\":{\"command\":\"$cmd\",\"command_text_source\":\"$3\",\"cwd\":\"$cwd\",\"pid\":$_LI_PID,\"sid\":$_LI_SID,\"pgrp\":$_LI_PGRP,\"new_session\":$_LI_NEW_SESSION,\"tty\":\"$_LI_TTY\",\"shell\":\"zsh\",\"shell_version\":\"${ZSH_VERSION:-unknown}\"${2-}"
    _LI_LINE="$_LI_LINE}"; _LI_LINE="$_LI_LINE}"
    _li_w
}

# preexec: the only place the command line exists verbatim, newlines and all
# (0-A t02: zsh is the only one of the three that keeps them).
_li_preexec() {
    (( _LI_BUSY )) && return 0              # never recurse into our own helpers
    _LI_BUSY=1
    _li_now_us; _LI_T0=$_LI_NOW_US
    typeset -g _LI_CMD=$1
    if (( _LI_EMIT )) && [[ -n ${LI_EMIT_STARTED:-} ]]; then
        _li_ts
        _li_req CommandStarted ',"prev_status":0' preexec
    fi
    _LI_BUSY=0
    return 0
}

# precmd: the real exit status, before the next prompt.
_li_precmd() {
    local rc=$?
    (( _LI_BUSY )) && return 0
    (( _LI_T0 == 0 )) && return 0           # nothing ran since the last prompt
    _LI_BUSY=1
    _li_now_us; local now=$_LI_NOW_US dur=0
    (( _LI_T0 > 0 )) && (( dur = (now - _LI_T0) / 1000 ))
    _li_ts
    _li_req CommandFinished ",\"exit_status\":$rc,\"duration_ms\":$dur" preexec
    _LI_T0=0
    typeset -g _LI_CMD=''
    _LI_BUSY=0
    return 0
}

_li_arm()    { _LI_EMIT=1 }
_li_disarm() { _LI_EMIT=0 }
_li_session_started() {
    _li_ts; _LI_CMD=''
    _li_req SessionStarted ",\"capability_record\":$_LI_CAP_RECORD" launcher
}
_li_off() {
    precmd_functions=(${precmd_functions:#_li_precmd})
    preexec_functions=(${preexec_functions:#_li_preexec})
    unset _LI_ZSH_ON
    _li_disarm
}

_li_sess
_LI_CAP_FILE=${LI_CAPABILITY_FILE:-${${(%):-%N}:A:h}/../../capabilities/zsh.json}
IFS= read -r _LI_CAP_RECORD < "$_LI_CAP_FILE" 2>/dev/null || _LI_CAP_RECORD='{}'
# One-shot probe: `: >&3` writes nothing but still reports a bad fd or a dead peer.
if [[ -n ${LI_SINK_FD:-} ]] && : >&3 2>/dev/null; then _li_arm; fi
# Register into the raw arrays (see the header: add-zsh-hook would erase them).
typeset -ga preexec_functions precmd_functions
[[ ${preexec_functions[(r)_li_preexec]} == _li_preexec ]] || preexec_functions+=( _li_preexec )
[[ ${precmd_functions[(r)_li_precmd]} == _li_precmd ]] || precmd_functions+=( _li_precmd )
_li_session_started
true
