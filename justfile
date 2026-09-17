# hyprdie task runner — run `just` to list recipes.

# Show available recipes.
default:
    @just --list

# Pre-PR checklist: format, lint, test — the same checks CI runs.
check: fmt-check clippy test

# Verify formatting without writing changes.
fmt-check:
    cargo fmt --all --check

# Apply formatting.
fmt:
    cargo fmt --all

# Lint all targets, treating warnings as errors.
clippy:
    cargo clippy --all-targets --locked -- -D warnings

# Run the test suite.
test:
    cargo test

# Build the release binary.
build:
    cargo build --release

# Audit dependencies for known security advisories (needs cargo-deny).
audit:
    cargo deny check advisories

# One-time: point git at the tracked .githooks/ dir (see install-hooks.sh).
install-hooks:
    ./install-hooks.sh

# Cut a release: bump the version, commit, tag, and push (triggers release.yml).
# Run from a clean main, e.g. `just release 0.1.0`. The release tag must point at
# a commit whose Cargo.toml `version` matches the tag, so this recipe is the only
# safe way to cut one.
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! printf '%s' "{{ version }}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
      echo "usage: just release X.Y.Z (e.g. just release 0.1.0)" >&2
      exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
      echo "working tree is not clean; commit or stash your changes first" >&2
      exit 1
    fi
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" != "main" ]; then
      echo "releases must be cut from main (currently on '$branch')" >&2
      exit 1
    fi
    if git rev-parse -q --verify "refs/tags/v{{ version }}" >/dev/null; then
      echo "tag v{{ version }} already exists" >&2
      exit 1
    fi
    git fetch --quiet origin main
    if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
      echo "local main is not in sync with origin/main; pull or push first" >&2
      exit 1
    fi
    # Bump the version, then let cargo refresh the package entry in Cargo.lock.
    # This must happen before the checks: clippy runs with --locked, which fails
    # if the lockfile still names the old version.
    sed -i -E 's/^version = ".*"/version = "{{ version }}"/' Cargo.toml
    cargo metadata --format-version 1 >/dev/null
    just check
    git add Cargo.toml Cargo.lock
    git commit -m "Release {{ version }}"
    git tag -a "v{{ version }}" -m "v{{ version }}"
    git push origin main
    git push origin "v{{ version }}"
