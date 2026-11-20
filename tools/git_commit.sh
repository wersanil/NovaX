#!/usr/bin/env bash
# Commit the NovaX working tree into the bare repo under git_commits/.
# Usage: tools/git_commit.sh "Milestone N: short description"
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export GIT_DIR="$ROOT/git_commits/novax.git"

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <commit message>" >&2
    exit 2
fi

git --work-tree="$ROOT" add -A
git --work-tree="$ROOT" commit -m "$*"
git --work-tree="$ROOT" log --oneline -1
