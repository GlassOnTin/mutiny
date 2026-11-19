# mutiny zsh .zshrc wrapper: the user's config first, then integration, then
# restore ZDOTDIR so .zlogin and any later zsh invocation behave normally.
if [ -n "${MUTINY_ZDOTDIR_ORIG:-}" ]; then
    if [ -f "$MUTINY_ZDOTDIR_ORIG/.zshrc" ]; then
        source "$MUTINY_ZDOTDIR_ORIG/.zshrc"
    fi
elif [ -f "$HOME/.zshrc" ]; then
    source "$HOME/.zshrc"
fi

if [ -n "${MUTINY_INTEGRATION_DIR:-}" ] && [ -f "$MUTINY_INTEGRATION_DIR/zsh.sh" ]; then
    source "$MUTINY_INTEGRATION_DIR/zsh.sh"
fi

if [ -n "${MUTINY_ZDOTDIR_ORIG:-}" ]; then
    export ZDOTDIR="$MUTINY_ZDOTDIR_ORIG"
else
    unset ZDOTDIR
fi
