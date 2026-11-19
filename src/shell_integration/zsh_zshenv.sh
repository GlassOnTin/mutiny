# mutiny zsh .zshenv wrapper. ZDOTDIR still points here so the rest of the
# startup sequence reaches our wrappers; the user's own file is sourced by
# absolute path rather than by restoring ZDOTDIR early.
if [ -n "${MUTINY_ZDOTDIR_ORIG:-}" ]; then
    if [ -f "$MUTINY_ZDOTDIR_ORIG/.zshenv" ]; then
        source "$MUTINY_ZDOTDIR_ORIG/.zshenv"
    fi
elif [ -f "$HOME/.zshenv" ]; then
    source "$HOME/.zshenv"
fi
