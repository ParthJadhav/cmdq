# cmdq OSC 133 shell integration (zsh)
# Adds prompt boundary markers so cmdq can detect when the shell is at a prompt
# vs. running a command.
#
# Safe to source multiple times. No-op if not running under cmdq.

if [[ -n "$CMDQ_ACTIVE" ]] && [[ -z "$CMDQ_INTEGRATION_LOADED" ]]; then
    CMDQ_INTEGRATION_LOADED=1

    _cmdq_emit_cwd() {
        local cwd=$PWD
        cwd=${cwd//\%/%25}
        cwd=${cwd//$'\a'/%07}
        cwd=${cwd//$'\033'/%1B}
        cwd=${cwd//$'\r'/%0D}
        cwd=${cwd//$'\n'/%0A}
        printf '\e]7;file://localhost%s\a' "$cwd"
    }

    _cmdq_precmd() {
        local exit=$?
        # Emit the new cwd before "command finished" so cmdq can make dispatch
        # decisions against the directory the next command would actually run in.
        _cmdq_emit_cwd
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
