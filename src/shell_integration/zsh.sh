# mutiny shell integration for zsh: OSC 133 prompt/command markers, OSC 7 cwd.
if [ -n "${MUTINY_INTEGRATION_LOADED:-}" ]; then
    return 0
fi
MUTINY_INTEGRATION_LOADED=1

__mutiny_osc() { printf '\033]%s\007' "$1"; }

# Byte-wise percent encoding: nomultibyte makes ${#s} and ${s:i:1} count bytes,
# so UTF-8 paths encode as their bytes rather than as codepoints.
__mutiny_urlencode() {
    emulate -L zsh
    setopt localoptions nomultibyte
    local s="$1" out='' i c
    for ((i = 1; i <= ${#s}; i++)); do
        c="${s[i]}"
        case "$c" in
            [a-zA-Z0-9.~_/-]) out+="$c" ;;
            *) printf -v c '%%%02X' "'$c"; out+="$c" ;;
        esac
    done
    print -n -- "$out"
}

__mutiny_cmd_active=0

__mutiny_precmd() {
    local ec=$?
    if ((__mutiny_cmd_active)); then
        __mutiny_osc "133;D;$ec"
        __mutiny_cmd_active=0
    fi
    __mutiny_osc "7;file://${HOST}$(__mutiny_urlencode "$PWD")"
    __mutiny_osc '133;A'
}

__mutiny_preexec() {
    if ((!__mutiny_cmd_active)); then
        __mutiny_osc '133;C'
        __mutiny_cmd_active=1
    fi
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __mutiny_precmd
add-zsh-hook preexec __mutiny_preexec

# B after the prompt is drawn; %{ %} keeps zsh's width accounting correct.
__mutiny_b=$'\033]133;B\007'
PS1="${PS1-}%{${__mutiny_b}%}"
