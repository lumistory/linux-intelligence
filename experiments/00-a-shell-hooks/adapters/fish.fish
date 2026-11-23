# adapters/fish.fish -- minimal interactive fish hook adapter (experiment 00-a)
#
#   exec 3>...                     # launcher hands over a writable FD 3
#   source adapters/fish.fish
#
# fish is the easiest shell to hook and the hardest to hook *correctly*:
#  * fish_preexec and fish_postexec are real events, not prompt tricks, so they
#    also fire for commands run from a function body and for `exec`ed scripts.
#  * fish_preexec receives the command line in $argv (joined, multi-line OK).
#  * fish_postexec also receives the line in $argv, and $status holds the exit
#    status; fish additionally hands over $CMD_DURATION (milliseconds) for free,
#    which removes the need for our own start timestamp.
#  * fish has no FD redirection into a unix socket, and it refuses `>&3` unless
#    the fd exists; the write is done with `command printf` so a failure can be
#    caught by `or` instead of aborting the handler.
#  * Under a real TERM value fish 4.9.3 blocks at startup on terminal
#    capability queries (DA1/OSC11) and never paints a prompt in a bare pty.
#    TERM=dumb avoids it (measured: results/t-probe-fish-term.log).

function _li_on --description 'install LI hooks'
    set -g _LI_FISH_ON 1
    set -g _LI_SEQ 0
    set -g _LI_FD3_OK 0
    set -g _LI_LAST_CMD ""
    set -g _LI_SESSION_ID (set -q LI_SESSION_ID; and echo $LI_SESSION_ID; or echo "li-fish-$fish_pid")
    set -g _LI_PID $fish_pid
    set -g _LI_TTY (tty 2>/dev/null; or echo none)
    # session facts from /proc, no fork beyond the read itself
    set -l stat (cat /proc/$fish_pid/stat 2>/dev/null)
    if test -n "$stat"
        set -l f (string split ' ' (string replace -r '^.*\) ' '' -- $stat))
        set -g _LI_PGRP $f[3]
        set -g _LI_SID $f[4]
    else
        set -g _LI_PGRP $fish_pid
        set -g _LI_SID $fish_pid
    end
    if test "$_LI_SID" = "$fish_pid"
        set -g _LI_NEW_SESSION true
    else
        set -g _LI_NEW_SESSION false
    end
    printf '' >&3 2>/dev/null; and set -g _LI_FD3_OK 1
    return 0
end

function _li_w --description 'write one JSON line to FD 3, never fatal'
    test "$_LI_FD3_OK" = 1; or return 1
    printf '%s\n' $argv[1] >&3 2>/dev/null; or begin
        set -g _LI_FD3_OK 0
        return 1
    end
    return 0
end

function _li_esc --description 'JSON-escape a string'
    set -l s $argv[1]
    set s (string replace -a '\\' '\\\\' -- $s)
    set s (string replace -a '"' '\"' -- $s)
    set s (string replace -a \n '\\n' -- $s)
    set s (string replace -a \t '\\t' -- $s)
    set s (string replace -a \r '\\r' -- $s)
    printf '%s' $s
end

function _li_ts
    set -l d (date -u +%Y-%m-%dT%H:%M:%S.%3NZ 2>/dev/null)
    or set d (date -u +%Y-%m-%dT%H:%M:%SZ)
    printf '%s' $d
end

function _li_preexec --on-event fish_preexec
    set -g _LI_LAST_CMD $argv[1]
    test "$LI_EMIT_STARTED" = 1; or return 0
    set -g _LI_SEQ (math $_LI_SEQ + 1)
    _li_w (printf '{"schema_version":1,"source":"shell.fish","session_id":"%s","sequence":%d,"ts_wall":"%s","type":"CommandStarted","payload":{"command":"%s","command_text_source":"preexec","cwd":"%s","pid":%d},"evidence_status":"declared"}' \
        $_LI_SESSION_ID $_LI_SEQ (_li_ts) (_li_esc "$argv[1]") (_li_esc "$PWD") $fish_pid)
end

function _li_postexec --on-event fish_postexec
    set -l rc $status
    set -l dur 0
    set -q CMD_DURATION; and set dur $CMD_DURATION
    set -g _LI_SEQ (math $_LI_SEQ + 1)
    _li_w (printf '{"schema_version":1,"source":"shell.fish","session_id":"%s","sequence":%d,"ts_wall":"%s","type":"CommandFinished","payload":{"command":"%s","command_text_source":"postexec_argv","exit_status":%d,"duration_ms":%d,"cwd":"%s","pid":%d,"sid":%s,"pgrp":%s,"new_session":%s,"tty":"%s"},"evidence_status":"declared"}' \
        $_LI_SESSION_ID $_LI_SEQ (_li_ts) (_li_esc "$argv[1]") $rc $dur \
        (_li_esc "$PWD") $fish_pid $_LI_SID $_LI_PGRP $_LI_NEW_SESSION $_LI_TTY)
end

function _li_fish_off --description 'uninstall LI hooks'
    functions -e _li_preexec _li_postexec
    set -e _LI_FISH_ON
end

_li_on
