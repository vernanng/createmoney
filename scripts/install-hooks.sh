#!/bin/sh
# Install the versioned git hooks into .git/hooks (hooks are not committed
# by git itself, so each clone runs this once).
# Usage: scripts/install-hooks.sh
set -eu

root=$(git rev-parse --show-toplevel)
ln -sf "$root/scripts/hook-post-commit" "$root/.git/hooks/post-commit"
chmod +x "$root/.git/hooks/post-commit" "$root/scripts/doc-check.sh"
echo "installed: .git/hooks/post-commit"
