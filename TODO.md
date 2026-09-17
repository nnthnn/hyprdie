# TODO

Prioritized backlog for hyprdie. Higher up = do sooner.

## Next

- [x] Live app list: re-poll `hyprctl -j clients` each tick so closed apps
      disappear, and show an "N apps remaining" counter in the header.
- [x] Graceful force-kill escalation: `SIGTERM`, then `SIGKILL` after a
      configurable per-app timeout, instead of waiting forever for the `F` key.
- [x] Close layer-shell surfaces too (bars/notifications) via
      `hyprctl -j layers`, matching hyprshutdown.
- [x] Robust process discovery: use the systemd cgroup for the Hyprland
      session instead of the `/proc` ppid walk (which can miss apps).
- [x] Hyprland guard: check `HYPRLAND_INSTANCE_SIGNATURE` and fail with a
      clear message when not running under Hyprland.

## Later

- [ ] Multi-monitor: one layer surface per output (currently a single surface
      sized to the first output).
- [x] Config flexibility: `--config <path>` flag, honor `$XDG_CONFIG_HOME`,
      make text colors configurable instead of hardcoded constants.
- [ ] Post-command sequencing: add a `--vt N` equivalent for the NVIDIA+SDDM
      black-screen workaround.

## Packaging / hygiene

- [x] Git hooks: `pre-commit` formats staged Rust files, `pre-push` runs the
      fmt/clippy checks CI runs (`./install-hooks.sh` to enable).
- [ ] README: usage, config reference, and a Hyprland keybind example.
- [ ] Install script / packaging (e.g. `cargo install` instructions, systemd).
- [ ] `cargo fmt` + `clippy` clean, and add unit tests for config/CLI parsing,
      process discovery (`session_descendants`/`ppid_descendants`), and text
      measurement.

## Done

- [x] TOML config at `~/.config/hyprdie/config.toml` (`ui`, `behavior`, `commands`).
- [x] CLI flags: `--dry-run` and `--post-cmd`.
- [x] Wayland wlr-layer-shell overlay (no GTK, no winit) with `wl_shm` ARGB rendering.
- [x] Background image (content-sniffed format), cover-scaled to the output.
- [x] App list (class — title) with muted color, centered layout.
- [x] Keyboard hint line (`ESC` cancel, `F` force quit).
- [x] Close apps via `closewindow` + `SIGTERM`, retry loop, then
      `hyprctl dispatch exit` and run the post command.
- [x] Graceful force-kill escalation: `SIGTERM`, then `SIGKILL` after a
      configurable per-app timeout (`behavior.sigkill_timeout_ms`).
