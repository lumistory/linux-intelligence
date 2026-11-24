# adapters/fish.fish - product fish adapter (P1-18).  source adapters/fish.fish
# Reasoning: adapters/README.md. fish_preexec/fish_postexec; $CMD_DURATION is
# used as the duration. fish's dead-sink behaviour stays `unknown` in
# capabilities/fish.json - bash solving SIGPIPE says nothing about fish.

if not status --is-interactive
    exit 0
end
if set -q _LI_FISH_ON
    exit 0
end
set -g _LI_FISH_ON 1
set -g _LI_EMIT 0            # 1 only after the sink probe succeeds
set -g _LI_CMD ''
set -g _LI_PID $fish_pid
set -g _LI_STARTTIME 0
set -g _LI_PGRP 0
set -g _LI_SID 0
set -g _LI_TTY none
set -g _LI_CAP_RECORD '{}'

function _li_sess --description 'facts that never change during the session'
    set -l stat (cat /proc/$fish_pid/stat 2>/dev/null)
    if test -n "$stat"
        # Strip `pid (comm) ` - comm may contain spaces and parens - then split.
        # Fields resume at stat field 3 (state): pgrp=f[3], session=f[4],
        # starttime (field 22) = f[20].
        set -l f (string split ' ' (string replace -r '^.*\) ' '' -- $stat))
        if set -q f[4]; set -g _LI_PGRP $f[3]; set -g _LI_SID $f[4]; end
        if set -q f[20]; set -g _LI_STARTTIME $f[20]; end
    end
    if test "$_LI_SID" = "$fish_pid"
        set -g _LI_NEW_SESSION true
    else
        set -g _LI_NEW_SESSION false
    end
    if set -q LI_SESSION_ID
        set -g _LI_SESSION_ID $LI_SESSION_ID
    else
        set -g _LI_SESSION_ID li-fish-$fish_pid-$_LI_STARTTIME
        set -gx LI_SESSION_ID $_LI_SESSION_ID
    end
    # `tty` without -s blocks when an inherited fd is not a terminal; query only.
    if tty -s 2>/dev/null
        set -g _LI_TTY (tty 2>/dev/null; or echo tty)
    end
end

function _li_esc --description 'JSON-escape $argv[1]'
    set -l s $argv[1]
    set s (string replace -a '\\' '\\\\' -- $s)
    set s (string replace -a '"' '\"' -- $s)
    set s (string replace -a \n '\\n' -- $s)
    set s (string replace -a \t '\\t' -- $s)
    set s (string replace -a \r '\\r' -- $s)
    printf '%s' $s
end

# fish has no sub-second clock builtin, so this is one fork; 0-A measured the
# whole hook at 1085 us. Kept because a wrong timestamp is worse than a costly one.
function _li_ts
    set -l d (date -u +%Y-%m-%dT%H:%M:%S 2>/dev/null)
    or set d 1970-01-01T00:00:00
    printf '%s.000Z' $d
end

function _li_w --description 'write one request line, never fatal'
    test "$_LI_EMIT" = 1; or return 0
    printf '%s\n' $argv[1] >&3 2>/dev/null; or begin
        # Same rule as bash: one failure, then silent for the rest of the shell.
        set -g _LI_EMIT 0
    end
    return 0
end

function _li_req --description '$1 type, $2 extra payload JSON, $3 text source'
    set -l cmd (_li_esc "$_LI_CMD")
    set -l cwd (_li_esc "$PWD")
    # An empty command substitution yields zero arguments in fish, which shifts
    # every following printf argument by one and corrupts the frame. Force one
    # empty element for the two values that may legitimately be empty.
    set -q cmd[1]; or set cmd ''
    set -q cwd[1]; or set cwd ''
    set -l line (printf '{"type":"%s","ts_wall":"%s","payload":{"command":"%s","command_text_source":"%s","cwd":"%s","pid":%d,"sid":%d,"pgrp":%d,"new_session":%s,"tty":"%s","shell":"fish","shell_version":"%s"%s}}' \
        $argv[1] (_li_ts) $cmd $argv[3] $cwd $fish_pid $_LI_SID $_LI_PGRP \
        $_LI_NEW_SESSION $_LI_TTY $version $argv[2])
    _li_w $line
end

function _li_preexec --on-event fish_preexec
    set -g _LI_CMD $argv[1]
    test "$_LI_EMIT" = 1; or return 0
    test -n "$LI_EMIT_STARTED"; or return 0
    _li_req CommandStarted ',"prev_status":0' preexec
end

function _li_postexec --on-event fish_postexec
    set -l rc $status
    set -l dur 0
    set -q CMD_DURATION; and set dur $CMD_DURATION
    test "$_LI_EMIT" = 1; or return 0
    set -g _LI_CMD $argv[1]
    _li_req CommandFinished ",\"exit_status\":$rc,\"duration_ms\":$dur" postexec_argv
    set -g _LI_CMD ''
end

function _li_arm;     set -g _LI_EMIT 1; end
function _li_disarm;  set -g _LI_EMIT 0; end

function _li_off --description 'remove the hooks'
    functions -e _li_preexec _li_postexec
    set -e _LI_FISH_ON
    _li_disarm
end

function _li_session_started
    set -g _LI_CMD ''
    _li_req SessionStarted ",\"capability_record\":$_LI_CAP_RECORD" launcher
end

_li_sess
set -l _li_cap_file (path normalize (status dirname)/../../capabilities/fish.json)
set -g _LI_CAP_RECORD (string collect <$_li_cap_file)
# One-shot probe: `printf ''` writes nothing but still reports a bad fd.
if set -q LI_SINK_FD
    printf '' >&3 2>/dev/null; and _li_arm
end
_li_session_started
true
