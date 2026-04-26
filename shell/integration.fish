# cmdq OSC 133 shell integration (fish)

if test -n "$CMDQ_ACTIVE"; and test -z "$CMDQ_INTEGRATION_LOADED"
    set -gx CMDQ_INTEGRATION_LOADED 1

    function _cmdq_preexec --on-event fish_preexec
        set -g _CMDQ_IN_CMD 1
        printf '\e]133;C\a'
    end

    function _cmdq_postexec --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
        set -e _CMDQ_IN_CMD
    end

    function _cmdq_prompt --on-event fish_prompt
        printf '\e]133;A\a'
    end
end
