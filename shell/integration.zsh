# cmdq OSC 133 shell integration (zsh)
# Adds prompt boundary markers so cmdq can detect when the shell is at a prompt
# vs. running a command.
#
# Safe to source multiple times. No-op if not running under cmdq.

if [[ -n "$CMDQ_ACTIVE" ]] && [[ -z "$CMDQ_INTEGRATION_LOADED" ]]; then
    export CMDQ_INTEGRATION_LOADED=1

    _cmdq_precmd() {
        local exit=$?
        # Emit "command finished" *before* "prompt start" so cmdq sees them in
        # the right order. The very first prompt (no previous command) emits
        # only the prompt-start marker.
        if [[ -n "$_CMDQ_IN_CMD" ]]; then
            printf '\e]133;D;%s\a' "$exit"
            unset _CMDQ_IN_CMD
        fi
        printf '\e]133;A\a'
    }

    _cmdq_preexec() {
        _CMDQ_IN_CMD=1
        printf '\e]133;C\a'
    }

    # Append our hooks without clobbering any existing hooks.
    autoload -Uz add-zsh-hook 2>/dev/null
    if (( $+functions[add-zsh-hook] )); then
        add-zsh-hook precmd  _cmdq_precmd
        add-zsh-hook preexec _cmdq_preexec
    else
        # Fallback for very old zsh.
        precmd_functions+=(_cmdq_precmd)
        preexec_functions+=(_cmdq_preexec)
    fi
fi
