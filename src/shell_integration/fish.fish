# mutiny shell integration for fish: OSC 133 prompt/command markers, OSC 7 cwd.
if set -q MUTINY_INTEGRATION_LOADED
    exit 0
end
set -g MUTINY_INTEGRATION_LOADED 1

function __mutiny_osc
    printf '\033]%s\007' $argv[1]
end

# fish's url style encodes UTF-8 correctly; slashes are path separators and
# must survive, so they are restored afterwards.
function __mutiny_osc7
    set -l encoded (string escape --style=url -- $PWD | string replace --all '%2F' '/' | string replace --all '%2f' '/')
    __mutiny_osc "7;file://$hostname$encoded"
end

# fish 4 emits OSC 133 A/B/C/D itself. Adding our own there duplicates every
# marker, which would double the engine's command records, so on 4.x we only
# contribute cwd reporting. fish 3 has no native markers and gets the full set.
if test (string split -f1 . -- $FISH_VERSION) -ge 4
    function __mutiny_prompt --on-event fish_prompt
        __mutiny_osc7
    end
else
    function __mutiny_postexec --on-event fish_postexec
        set -l ec $status
        __mutiny_osc "133;D;$ec"
    end

    function __mutiny_preexec --on-event fish_preexec
        __mutiny_osc '133;C'
    end

    function __mutiny_prompt --on-event fish_prompt
        __mutiny_osc7
        __mutiny_osc '133;A'
        __mutiny_osc '133;B'
    end
end
