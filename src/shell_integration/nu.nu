# mutiny shell integration for nushell: OSC 133 markers, OSC 7 cwd.
# Loaded from a wrapper config that sources the user's own config first.

def __mutiny_osc [payload: string] {
    # `char escape` is not a named character in nu; -i 27 is ESC.
    print -n $"(char -i 27)]($payload)(char bel)"
}

def __mutiny_osc7 [] {
    # `sys host` is builtin; shelling out to hostname would fail on systems
    # that do not ship inetutils, and abort the hook before it emits A.
    let host = ((sys host).hostname | str trim)
    let encoded = ($env.PWD | str replace --all ' ' '%20')
    __mutiny_osc $"7;file://($host)($encoded)"
}

# One C per command, one D per completed command. pre_prompt also fires at
# startup, so D is guarded by this flag rather than emitted unconditionally.
$env.__MUTINY_CMD_ACTIVE = false

$env.config.hooks.pre_prompt = (
    $env.config.hooks.pre_prompt?
    | default []
    | append {||
        if $env.__MUTINY_CMD_ACTIVE {
            __mutiny_osc $"133;D;($env.LAST_EXIT_CODE)"
            $env.__MUTINY_CMD_ACTIVE = false
        }
        __mutiny_osc7
        __mutiny_osc "133;A"
        __mutiny_osc "133;B"
    }
)

$env.config.hooks.pre_execution = (
    $env.config.hooks.pre_execution?
    | default []
    | append {||
        $env.__MUTINY_CMD_ACTIVE = true
        __mutiny_osc "133;C"
    }
)
