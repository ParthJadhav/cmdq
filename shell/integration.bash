# cmdq OSC 133 shell integration (bash)
# Adds prompt boundary markers so cmdq can detect when the shell is at a prompt
# vs. running a command.

if [[ -n "$CMDQ_ACTIVE" ]] && [[ -z "$CMDQ_INTEGRATION_LOADED" ]]; then
    export CMDQ_INTEGRATION_LOADED=1

    _cmdq_prompt_command() {
        local exit=$?
        if [[ -n "$_CMDQ_IN_CMD" ]]; then
            printf '\e]133;D;%s\a' "$exit"
            unset _CMDQ_IN_CMD
        fi
        printf '\e]133;A\a'
    }

    # bash has no preexec; we use the DEBUG trap to mimic it. Guard so the
    # marker only fires for the user's interactive command, not internal
    # subshells launched by PROMPT_COMMAND itself.
    _cmdq_preexec() {
        # BASH_COMMAND holds the about-to-run command. We don't want to fire
        # while PROMPT_COMMAND is itself executing.
        if [[ -z "$COMP_LINE" && "$BASH_COMMAND" != "$PROMPT_COMMAND" && -z "$_CMDQ_IN_CMD" ]]; then
            _CMDQ_IN_CMD=1
            printf '\e]133;C\a'
        fi
    }

    # Compose with any existing PROMPT_COMMAND.
    if [[ -z "$PROMPT_COMMAND" ]]; then
        PROMPT_COMMAND="_cmdq_prompt_command"
    elif [[ "$PROMPT_COMMAND" != *"_cmdq_prompt_command"* ]]; then
        PROMPT_COMMAND="_cmdq_prompt_command; $PROMPT_COMMAND"
    fi

    trap '_cmdq_preexec' DEBUG
fi
