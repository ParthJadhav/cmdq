# cmdq OSC 133 shell integration (fish)

if test -n "$CMDQ_ACTIVE"; and test -z "$CMDQ_INTEGRATION_LOADED"
    set -g CMDQ_INTEGRATION_LOADED 1

    function _cmdq_preexec --on-event fish_preexec
        set -g _CMDQ_IN_CMD 1
        printf '\e]133;C\a'
    end

    function _cmdq_emit_cwd
        set -l cwd "$PWD"
        set cwd (string replace -a '%' '%25' -- "$cwd")
        set cwd (string replace -a \a '%07' -- "$cwd")
        set cwd (string replace -a \e '%1B' -- "$cwd")
        set cwd (string replace -a \r '%0D' -- "$cwd")
        set cwd (string replace -a \n '%0A' -- "$cwd")
        printf '\e]7;file://localhost%s\a' "$cwd"
    end

    function _cmdq_postexec --on-event fish_postexec
        set -l exit $status
        _cmdq_emit_cwd
        printf '\e]133;D;%s\a' $exit
        set -e _CMDQ_IN_CMD
    end

    function _cmdq_prompt --on-event fish_prompt
        _cmdq_emit_cwd
        printf '\e]133;A\a'
    end
end
