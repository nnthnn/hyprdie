#!/usr/bin/env bash
# One-time: point git at the tracked .githooks/ directory.
#
# core.hooksPath is repo-level config, so one run covers every worktree of this
# repo. `pre-commit` formats staged Rust files; `pre-push` runs fmt-check and
# clippy, the same checks CI's lint job runs.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

git config core.hooksPath .githooks
chmod +x .githooks/*
