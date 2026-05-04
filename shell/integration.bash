# cmdq OSC 133 shell integration (bash)
# Adds prompt boundary markers so cmdq can detect when the shell is at a prompt
# vs. running a command.

if [[ -n "$CMDQ_ACTIVE" ]] && [[ -z "$CMDQ_INTEGRATION_LOADED" ]]; then
    CMDQ_INTEGRATION_LOADED=1

    read -r -d '' _CMDQ_INSTALL_DEBUG_TRAP <<'CMDQ_DEBUG_TRAP' || true
if [[ -z "$_CMDQ_DEBUG_TRAP_INSTALLED" ]]; then
    _CMDQ_DEBUG_TRAP_INSTALLED=1
    _CMDQ_INSTALLING_DEBUG_TRAP=1
    _CMDQ_PREVIOUS_DEBUG_HANDLER=
    _cmdq_previous_debug_trap=$(trap -p DEBUG)
    if [[ "$_cmdq_previous_debug_trap" == trap\ --\ *\ DEBUG ]]; then
        _cmdq_previous_debug_handler=${_cmdq_previous_debug_trap#trap -- }
        _cmdq_previous_debug_handler=${_cmdq_previous_debug_handler% DEBUG}
        eval "_CMDQ_PREVIOUS_DEBUG_HANDLER=${_cmdq_previous_debug_handler}"
    fi
    if [[ -n "$_CMDQ_PREVIOUS_DEBUG_HANDLER" ]]; then
        trap "${_CMDQ_PREVIOUS_DEBUG_HANDLER}; _cmdq_preexec" DEBUG
    else
        trap "_cmdq_preexec" DEBUG
    fi
    unset _cmdq_previous_debug_trap _cmdq_previous_debug_handler _CMDQ_INSTALLING_DEBUG_TRAP
fi
CMDQ_DEBUG_TRAP

    _cmdq_emit_cwd() {
        local cwd=$PWD
        cwd=${cwd//'%'/'%25'}
        cwd=${cwd//$'\a'/'%07'}
        cwd=${cwd//$'\033'/'%1B'}
        cwd=${cwd//$'\r'/'%0D'}
        cwd=${cwd//$'\n'/'%0A'}
        printf '\e]7;file://localhost%s\a' "$cwd"
    }

    _cmdq_prompt_command() {
        local exit=${1:-$?}
        _cmdq_emit_cwd
        if [[ -n "$_CMDQ_IN_CMD" ]]; then
            printf '\e]133;D;%s\a' "$exit"
            unset _CMDQ_IN_CMD
        fi
        printf '\e]133;A\a'
    }

    _cmdq_prompt_start() {
        local exit=${1:-$?}
        _CMDQ_LAST_STATUS=$exit
        _CMDQ_IN_PROMPT_COMMAND=1
        _cmdq_prompt_command "$exit"
        return "$exit"
    }

    _cmdq_prompt_end() {
        local exit=${_CMDQ_LAST_STATUS:-$?}
        unset _CMDQ_IN_PROMPT_COMMAND _CMDQ_LAST_STATUS
        return "$exit"
    }

    # bash has no preexec; we use the DEBUG trap to mimic it. Guard so the
    # marker only fires for the user's interactive command, not internal
    # subshells launched by PROMPT_COMMAND itself.
    _cmdq_preexec() {
        # BASH_COMMAND holds the about-to-run command. We don't want to fire
        # while PROMPT_COMMAND is itself executing.
        if [[ -z "$COMP_LINE" \
              && -z "$_CMDQ_INSTALLING_DEBUG_TRAP" \
              && -z "$_CMDQ_IN_PROMPT_COMMAND" \
              && "$BASH_COMMAND" != "$PROMPT_COMMAND" \
              && "$BASH_COMMAND" != _CMDQ_LAST_STATUS=* \
              && "$BASH_COMMAND" != "eval \"\$_CMDQ_INSTALL_DEBUG_TRAP\"" \
              && "$BASH_COMMAND" != _CMDQ_IN_PROMPT_COMMAND=* \
              && "$BASH_COMMAND" != "unset _CMDQ_IN_PROMPT_COMMAND" \
              && "$BASH_COMMAND" != "unset _CMDQ_IN_PROMPT_COMMAND _CMDQ_LAST_STATUS" \
              && "$BASH_COMMAND" != "_cmdq_prompt_command" \
              && "$BASH_COMMAND" != "_cmdq_prompt_command \"\$_CMDQ_LAST_STATUS\"" \
              && "$BASH_COMMAND" != "_cmdq_prompt_start" \
              && "$BASH_COMMAND" != "_cmdq_prompt_start \"\$_CMDQ_LAST_STATUS\"" \
              && "$BASH_COMMAND" != "_cmdq_prompt_end" \
              && "$BASH_COMMAND" != "_cmdq_emit_cwd" \
              && -z "$_CMDQ_IN_CMD" ]]; then
            _CMDQ_IN_CMD=1
            printf '\e]133;C\a'
        fi
    }

    # Compose with any existing PROMPT_COMMAND. The DEBUG trap is installed
    # from PROMPT_COMMAND rather than directly while this file is sourced,
    # because bash temporarily masks/restores DEBUG traps during a DEBUG
    # handler. Installing at the first prompt lets us preserve user traps.
    _cmdq_prompt_entry='_CMDQ_LAST_STATUS=$?; eval "$_CMDQ_INSTALL_DEBUG_TRAP"; _cmdq_prompt_start "$_CMDQ_LAST_STATUS"'
    _cmdq_prompt_decl=$(declare -p PROMPT_COMMAND 2>/dev/null || true)
    if [[ "$_cmdq_prompt_decl" == declare\ -a* ]]; then
        _cmdq_prompt_has_cmdq=0
        for _cmdq_prompt_part in "${PROMPT_COMMAND[@]}"; do
            if [[ "$_cmdq_prompt_part" == *"_cmdq_prompt_command"* \
                  || "$_cmdq_prompt_part" == *"_cmdq_prompt_start"* ]]; then
                _cmdq_prompt_has_cmdq=1
                break
            fi
        done
        if [[ "$_cmdq_prompt_has_cmdq" -eq 0 ]]; then
            PROMPT_COMMAND=("$_cmdq_prompt_entry" "${PROMPT_COMMAND[@]}" "_cmdq_prompt_end")
        fi
        unset _cmdq_prompt_has_cmdq _cmdq_prompt_part
    elif [[ -z "$PROMPT_COMMAND" ]]; then
        PROMPT_COMMAND="$_cmdq_prompt_entry; _cmdq_prompt_end"
    elif [[ "$PROMPT_COMMAND" != *"_cmdq_prompt_command"* \
            && "$PROMPT_COMMAND" != *"_cmdq_prompt_start"* ]]; then
        PROMPT_COMMAND="$_cmdq_prompt_entry; $PROMPT_COMMAND; _cmdq_prompt_end"
    fi
    unset _cmdq_prompt_decl _cmdq_prompt_entry
fi
