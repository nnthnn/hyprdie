# AGENTS.md

Guidance for coding agents working in the hyprdie repository. User-facing
documentation is in [`README.md`](README.md); the backlog is in
[`TODO.md`](TODO.md).

## What this is

hyprdie is a graceful shutdown screen for **Hyprland** on Linux/Wayland. It shows
a fullscreen layer-shell overlay listing the session's apps, asks them to close,
escalates to `SIGKILL` when they refuse, then runs a post command and exits
Hyprland.

The whole program is a single binary in [`src/main.rs`](src/main.rs) (no library
target, no modules). It draws directly through `wl_shm` and
`smithay-client-toolkit` — no GTK, no winit, no toolkit.

## Commands

```sh
cargo build --release                              # binary at target/release/hyprdie
cargo test                                         # unit tests, no compositor needed
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings # CI denies warnings
cargo deny check advisories                        # optional, needs cargo-deny
./install-hooks.sh                                 # one-time: enable the git hooks below
```

[`justfile`](justfile) wraps these (needs `just`): `just check` is fmt + clippy +
test, and `just release X.Y.Z` cuts a release. See [Releasing](#releasing).

Run `./install-hooks.sh` once per clone to point `core.hooksPath` at the tracked
[`.githooks/`](.githooks/). It's repo-level config, so it covers every worktree.
The `pre-commit` hook auto-formats staged `.rs` files; `pre-push` runs
`cargo fmt --all --check` and the clippy command above, so the local checks
match CI's lint job.

Building needs **xkbcommon** headers (`libxkbcommon-dev` on
Debian, `xkbcommon` on Arch) because `smithay-client-toolkit`'s build script
`pkg-config`s it. CI runs all three jobs in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml); keep them green.

## Releasing

Cut releases from `main` with the `just` recipe — it's the only safe way to make
the tag:

```sh
just release 0.1.0
```

It refuses unless the tree is clean and `main` matches `origin/main`, bumps
`version` in `Cargo.toml`, refreshes `Cargo.lock`, runs `just check`, commits as
`Release X.Y.Z`, then pushes the commit and the annotated `vX.Y.Z` tag. Pushing
the tag runs [`release.yml`](.github/workflows/release.yml), whose first step
re-checks that the tag equals the `Cargo.toml` version and fails loudly if they
diverge. That is exactly the failure the recipe exists to prevent: the tag must
point at a commit whose `version` it matches, or the workflow publishes nothing.
Cut a release by hand only if you replicate every step above.

## Testing

- Unit tests cover pure helpers only: config parsing, `/proc/<pid>/stat` and
  cgroup parsing, process helpers, and key mapping.
- **Do not add tests that need a live compositor.** CI has no Hyprland. If you
  touch process or key handling, factor the decision into a pure function and
  test that.
- The binary itself can only run inside a Hyprland session and will tear the
  session down when it completes — never run it to "try something out".

## Hard-won invariants

This program kills processes and ends sessions. Breaking one of these has real
consequences, so treat them as load-bearing:

- **Never target Hyprland's ancestors.** Session discovery via the systemd
  cgroup (`session_descendants`) subtracts the ppid chain above the compositor;
  otherwise it kills the display manager's session helper (`sddm-helper`) and the
  launcher (`start-hyprland`), which ends the session and drops the Wayland
  connection before the post command can run. See `ancestors()`.
- **Never target ourselves.** `refresh_clients` removes `process::id()` from the
  kill set every poll.
- **The post command runs to completion before `hyprctl dispatch exit`,** so
  commands like `systemctl reboot` can authorise with logind/polkit while the
  session is still alive. `run_post_command` waits (bounded by
  `POST_COMMAND_TIMEOUT`).
- **All completion paths converge on the same tail.** Graceful close, automatic
  SIGKILL escalation, and the manual `F` key all end in `Ui::finish`; `F` only
  changes *how* apps close, not what happens afterwards.
- **`alive()` must treat zombies as dead.** A lone `/proc/<pid>` entry is not
  proof of life; check the `State` field.
- **`state.pids` is rebuilt from scratch each poll** (`hyprctl -j clients` +
  session descendants + `hyprctl -j layers`). Discovery is best-effort: a failed
  `hyprctl` must not discard state already collected.

## Conventions

- Keep it clippy-clean under `-D warnings`; the project has no `#[allow]`s
  without a comment explaining why.
- Comments explain *why*, not *what* — see the existing code for the tone.
- Config precedence is defaults < config file < CLI flags. `config.example.toml`
  and the README config tables must stay in sync with the structs.
- The keysym in a `KeyEvent` is modifier-applied (`Keysym::f` is a bare press,
  `Keysym::F` is Shift+f), so key bindings should accept both cases.
- Assets are not embedded in the binary and there is no install step yet.
- Commit messages: imperative subject, body explaining the reasoning. Match the
  existing history (`git log`).
