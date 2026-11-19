# mutiny zsh .zprofile wrapper, for login shells.
if [ -n "${MUTINY_ZDOTDIR_ORIG:-}" ]; then
    if [ -f "$MUTINY_ZDOTDIR_ORIG/.zprofile" ]; then
        source "$MUTINY_ZDOTDIR_ORIG/.zprofile"
    fi
elif [ -f "$HOME/.zprofile" ]; then
    source "$HOME/.zprofile"
fi
