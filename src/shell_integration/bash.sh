# mutiny shell integration for bash: OSC 133 prompt/command markers, OSC 7 cwd.
# Loaded automatically by the bash rc wrapper; safe to source twice.
if [ -n "${MUTINY_INTEGRATION_LOADED:-}" ]; then
    return 0 2>/dev/null || exit 0
fi
MUTINY_INTEGRATION_LOADED=1

__mutiny_osc() { printf '\033]%s\007' "$1"; }

# Percent-encode a path for OSC 7. LC_ALL=C makes the loop iterate bytes, so
# UTF-8 paths encode correctly rather than per-character.
__mutiny_urlencode() {
    local LC_ALL=C s="$1" out='' i c
    for ((i = 0; i < ${#s}; i++)); do
        c="${s:i:1}"
        case "$c" in
            [a-zA-Z0-9.~_/-]) out+="$c" ;;
            *) printf -v c '%%%02X' "'$c"; out+="$c" ;;
        esac
    done
    printf '%s' "$out"
}

__mutiny_osc7() {
    __mutiny_osc "7;file://${HOSTNAME}$(__mutiny_urlencode "$PWD")"
}

# One C per command line, one D per completed command line. The prompt
# machinery sets __mutiny_in_prompt so the DEBUG trap ignores its own commands.
__mutiny_cmd_active=0
__mutiny_in_prompt=0

__mutiny_precmd() {
    local ec=$?
    __mutiny_in_prompt=1
    if [ "$__mutiny_cmd_active" = 1 ]; then
        __mutiny_osc "133;D;$ec"
        __mutiny_cmd_active=0
    fi
    __mutiny_osc7
    __mutiny_osc '133;A'
    __mutiny_in_prompt=0
}

__mutiny_preexec() {
    [ "$__mutiny_in_prompt" = 1 ] && return
    [ -n "${COMP_LINE:-}" ] && return
    if [ "$__mutiny_cmd_active" = 0 ]; then
        __mutiny_osc '133;C'
        __mutiny_cmd_active=1
    fi
}

# Prepend so $? still holds the user command's status when precmd runs.
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == 'declare -a'* ]]; then
    PROMPT_COMMAND=(__mutiny_precmd "${PROMPT_COMMAND[@]}")
else
    PROMPT_COMMAND="__mutiny_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi

trap '__mutiny_preexec' DEBUG

# B lands after the prompt is drawn. \[ \] keeps readline's width accounting
# correct, and the marker is literal so PS1 expansion runs no commands.
PS1="${PS1-}\[\033]133;B\007\]"
