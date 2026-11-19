# mutiny bash rc wrapper. bash --rcfile replaces the normal rc search, so this
# reproduces it (system bashrc, then the user's) before loading integration.
# Applies to interactive non-login shells; bash ignores --rcfile with --login.
if [ -f /etc/bash.bashrc ]; then
    . /etc/bash.bashrc
fi
if [ -f "$HOME/.bashrc" ]; then
    . "$HOME/.bashrc"
fi
if [ -n "${MUTINY_INTEGRATION_DIR:-}" ] && [ -f "$MUTINY_INTEGRATION_DIR/bash.sh" ]; then
    . "$MUTINY_INTEGRATION_DIR/bash.sh"
fi
