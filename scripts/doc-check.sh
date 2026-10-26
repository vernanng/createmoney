#!/bin/sh
# Advisory post-commit docs check. Maps the files changed in a commit
# (default HEAD) to the .md docs that may need an update and prints a
# checklist. Never fails: exit 0 always so it can run as a post-commit
# hook without blocking. Usage: scripts/doc-check.sh [commit]
set -u

files=$(git show --name-only --pretty=format: "${1:-HEAD}" -- 2>/dev/null) || exit 0
[ -z "$files" ] && exit 0

need_readme_layout=0
need_readme_venues=0
need_config_docs=0
need_agents=0

for f in $files; do
  case "$f" in
    crates/feed/*|crates/exec/*|crates/dex-*) need_readme_venues=1 ;;
    crates/strategy/*) need_readme_venues=1 ;;
    bins/arb-engine/*|crates/api/*) need_readme_layout=1 ;;
    config.toml|config.example.toml) need_config_docs=1 ;;
    .github/*) need_agents=1 ;;
    *.md) ;;
    crates/*|bins/*) need_readme_layout=1 ;;
  esac
done

[ $need_readme_venues$need_readme_layout$need_config_docs$need_agents = "0000" ] && exit 0

echo "[docs] HEAD touched code — consider updating:"
[ $need_readme_venues = 1 ] && echo "  - README.md 'Venues & strategies' table"
[ $need_readme_layout = 1 ] && echo "  - README.md 'Quick start' / 'Layout' sections"
[ $need_config_docs = 1 ] && echo "  - README.md 'Config' + config.example.toml comments"
[ $need_agents = 1 ] && echo "  - AGENTS.md 'Commands' (must match CI)"
exit 0
