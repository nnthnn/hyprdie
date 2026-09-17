use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use calloop::{
    EventLoop, LoopSignal,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use image::{DynamicImage, ImageReader, imageops::FilterType};
use nix::sys::signal::{SigHandler, Signal, kill, signal};
use nix::unistd::Pid;
use serde::Deserialize;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Cursor, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, process};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
};

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
struct Config {
    ui: UiConfig,
    behavior: BehaviorConfig,
    commands: CommandConfig,
}
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
struct UiConfig {
    title: String,
    background: Option<String>,
    colors: UiColors,
}
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
struct BehaviorConfig {
    poll_interval_ms: u64,
    retry_interval_ms: u64,
    sigkill_timeout_ms: u64,
    dry_run: bool,
    no_exit: bool,
}
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
struct CommandConfig {
    post: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    fn rgb(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer)?;
        let digits = hex.trim_start_matches('#');
        if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(
                "expected a color in `#rrggbb` form",
            ));
        }
        let value = u32::from_str_radix(digits, 16).map_err(serde::de::Error::custom)?;
        Ok(Rgb {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        })
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
struct UiColors {
    title: Rgb,
    heading: Rgb,
    app: Rgb,
    hint: Rgb,
    background: Rgb,
}

impl Default for UiColors {
    fn default() -> Self {
        Self {
            title: Rgb::new(0xff, 0xff, 0xff),
            heading: Rgb::new(0xff, 0xff, 0xff),
            app: Rgb::new(0xac, 0xb5, 0xc7),
            hint: Rgb::new(0x80, 0x80, 0x80),
            background: Rgb::new(0x1b, 0x18, 0x18),
        }
    }
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            title: "Ending session...".into(),
            background: None,
            colors: UiColors::default(),
        }
    }
}
impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 150,
            retry_interval_ms: 5_000,
            sigkill_timeout_ms: 10_000,
            dry_run: false,
            no_exit: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Client {
    address: Option<String>,
    pid: Option<i32>,
    class: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HyprLayer {
    #[serde(default)]
    pid: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct MonitorLayers {
    levels: HashMap<String, Vec<HyprLayer>>,
}
struct ShutdownState {
    pids: HashSet<i32>,
    hypr_children: HashSet<i32>,
    addresses: Vec<String>,
    apps: Vec<(String, String)>,
    last_retry: Instant,
    term_started: HashMap<i32, Instant>,
    config: Config,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hyprdie/config.toml")
}
fn expand_path(value: &str) -> PathBuf {
    value
        .strip_prefix("~/")
        .and_then(|p| dirs::home_dir().map(|h| h.join(p)))
        .unwrap_or_else(|| PathBuf::from(value))
}
fn load_config(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}
fn hyprctl(args: &[&str]) -> Result<String, String> {
    let o = Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() {
        String::from_utf8(o.stdout).map_err(|e| e.to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).into())
    }
}
const IGNORE_DAEMONS: &[&str] = &["Xwayland"];

fn is_ignored_daemon(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| IGNORE_DAEMONS.contains(&comm.trim()))
        .unwrap_or(false)
}

/// Extract the cgroup path from the contents of `/proc/<pid>/cgroup`.
fn parse_cgroup(text: &str) -> Option<String> {
    // Each line is "<hierarchy-id>:<controllers>:<path>". The cgroups v2 unified
    // hierarchy has a single "0::<path>" line (controllers empty); v1 has one
    // line per controller with a non-empty controller list.
    let mut fallback: Option<String> = None;
    for line in text.lines() {
        let Some(rest) = line.split_once(':').map(|(_, r)| r) else {
            continue;
        };
        let Some((controllers, path)) = rest.split_once(':') else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        if controllers.is_empty() {
            return Some(path.to_string());
        }
        fallback.get_or_insert_with(|| path.to_string());
    }
    fallback
}

fn cgroup_path(pid: i32) -> Option<String> {
    parse_cgroup(&fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

/// Read a process's parent PID from `/proc/<pid>/stat`.
fn ppid_of(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Format: "pid (comm) state ppid ...". `comm` may contain spaces and parens,
    // so skip to the last ')' and take the second field after it.
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// PIDs on the parent chain above `pid`, up to but excluding PID 1.
fn ancestors(pid: i32) -> HashSet<i32> {
    let mut ancestors = HashSet::new();
    let mut current = pid;
    while let Some(parent) = ppid_of(current) {
        if parent <= 1 || !ancestors.insert(parent) {
            break;
        }
        current = parent;
    }
    ancestors
}

/// Find the processes to tear down along with the session.
///
/// Prefer enumerating the systemd session cgroup: unlike a ppid walk it still
/// catches processes that daemonized and were re-parented away from Hyprland.
/// Falls back to the ppid walk when there's no usable cgroup (e.g. no systemd).
fn session_descendants(root: i32) -> HashSet<i32> {
    if let Some(cgroup) = cgroup_path(root).filter(|c| c != "/") {
        let prefix = format!("{cgroup}/");
        let mut found = HashSet::new();
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                if pid == root || is_ignored_daemon(pid) {
                    continue;
                }
                let Some(path) = cgroup_path(pid) else {
                    continue;
                };
                if path == cgroup || path.starts_with(&prefix) {
                    found.insert(pid);
                }
            }
        }
        if !found.is_empty() {
            // The session cgroup also contains Hyprland's own ancestors: the
            // display manager's session leader (e.g. `sddm-helper`) and the
            // launcher (e.g. `start-hyprland`). Killing them ends the session
            // and drops our Wayland connection before the post command can run,
            // so never target anything above Hyprland.
            for ancestor in ancestors(root) {
                found.remove(&ancestor);
            }
            return found;
        }
    }
    ppid_descendants(root)
}

fn ppid_descendants(root: i32) -> HashSet<i32> {
    let mut found = HashSet::new();
    // Track visited pids separately from `found`: ignored daemons (Xwayland)
    // must not be killed, but we still have to walk *through* them to reach
    // their children, which are the X11 clients we do want to close.
    let mut visited = HashSet::new();
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        let Ok(entries) = fs::read_dir("/proc") else {
            break;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let (Some(open), Some(end)) = (stat.find('('), stat.rfind(')')) else {
                continue;
            };
            let comm = &stat[open + 1..end];
            let fields: Vec<_> = stat[end + 2..].split_whitespace().collect();
            if fields.get(1).and_then(|v| v.parse().ok()) != Some(parent) {
                continue;
            }
            if !visited.insert(pid) {
                continue;
            }
            if !IGNORE_DAEMONS.contains(&comm) {
                found.insert(pid);
            }
            pending.push(pid);
        }
    }
    found
}
fn refresh_clients(state: &mut ShutdownState) {
    let Ok(raw) = hyprctl(&["-j", "clients"]) else {
        return;
    };
    let Ok(clients) = serde_json::from_str::<Vec<Client>>(&raw) else {
        return;
    };

    state.addresses = clients.iter().filter_map(|c| c.address.clone()).collect();
    state.pids = clients.iter().filter_map(|c| c.pid).collect();
    state.pids.extend(&state.hypr_children);

    // Layer-shell surfaces (bars, notifications, launchers, ...) have no toplevel
    // window, so they never show up in `hyprctl clients`. Close their client
    // processes too, matching hyprshutdown. This is best-effort: a failure must
    // not discard the client state already collected above.
    if let Ok(layers_raw) = hyprctl(&["-j", "layers"]) {
        state.pids.extend(collect_layer_pids(&layers_raw));
    }

    // Never target ourselves: we run inside the session that we're tearing
    // down, so the process-discovery pass includes us.
    state.pids.remove(&(process::id() as i32));
    state.apps = clients
        .iter()
        .filter_map(|c| {
            let class = c.class.clone().unwrap_or_default();
            let title = c.title.clone().unwrap_or_default();
            if class.is_empty() && title.is_empty() {
                None
            } else {
                Some((class, title))
            }
        })
        .collect();
}

fn collect_layer_pids(raw: &str) -> HashSet<i32> {
    // `hyprctl -j layers` is keyed by monitor name:
    // { "<monitor>": { "levels": { "0": [ { .., "pid": N }, .. ], .. } } }.
    let Ok(by_monitor) = serde_json::from_str::<HashMap<String, MonitorLayers>>(raw) else {
        return HashSet::new();
    };
    by_monitor
        .values()
        .flat_map(|monitor| monitor.levels.values())
        .flatten()
        .filter_map(|layer| layer.pid)
        .collect()
}

/// Collect the processes to tear down along with the session. This only gathers
/// state; the close sequence waits until the overlay has actually been
/// presented (see `Ui::start_closing`), so apps don't start disappearing before
/// the user can see what's happening.
fn discover(state: &mut ShutdownState) {
    if let Ok(raw) = hyprctl(&["-j", "instances"])
        && let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&raw)
        && let Some(pid) = items
            .iter()
            .find_map(|i| i.get("pid").and_then(|v| v.as_i64()).map(|p| p as i32))
    {
        state.hypr_children = session_descendants(pid);
    }
    refresh_clients(state);
}
fn retry_close(state: &mut ShutdownState) {
    for address in &state.addresses {
        let _ = hyprctl(&["dispatch", "closewindow", &format!("address:{address}")]);
    }
    let now = Instant::now();
    let timeout = Duration::from_millis(state.config.behavior.sigkill_timeout_ms);
    for pid in &state.pids {
        let Some(started) = state.term_started.get(pid) else {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
            state.term_started.insert(*pid, now);
            continue;
        };
        if now.duration_since(*started) >= timeout {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
        }
    }
}

/// Whether a process is still running. A zombie still has a `/proc/<pid>`
/// entry, so existence alone isn't enough: a process that has exited but not
/// been reaped must count as dead, or the shutdown loop waits on a corpse.
fn is_alive(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Format: "pid (comm) state ...". `comm` may contain spaces and parens, so
    // find the last ')' and read the state that follows it.
    let Some(close) = stat.rfind(')') else {
        return true;
    };
    !matches!(
        stat[close + 1..].split_whitespace().next(),
        Some("Z") | Some("X")
    )
}

fn alive(pids: &HashSet<i32>) -> usize {
    pids.iter().filter(|p| is_alive(**p)).count()
}

/// How long to let the post command run before giving up on it.
const POST_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the first frame callback before starting the close
/// sequence anyway. The callback is the intended trigger (see
/// `Ui::start_closing`); this only keeps a compositor that never sends one from
/// wedging shutdown on the overlay.
const CLOSE_FALLBACK_DELAY: Duration = Duration::from_secs(1);

/// Run the post command and wait for it to finish.
///
/// Waiting matters: `systemctl reboot`/`poweroff` authorise with logind/polkit
/// while the session is still alive, so firing it off and immediately exiting
/// Hyprland lets session teardown kill it mid-authorisation. The wait is bounded
/// so a command that hangs can't wedge logout forever.
fn run_post_command(command: &str) {
    let Ok(mut child) = Command::new("sh").args(["-c", command]).spawn() else {
        return;
    };
    let deadline = Instant::now() + POST_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                return;
            }
        }
    }
}

fn load_font() -> Option<FontVec> {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = fs::read(path)
            && let Ok(font) = FontVec::try_from_vec(bytes)
        {
            return Some(font);
        }
    }
    None
}

fn measure_text(font: &FontVec, text: &str, size: f32) -> f32 {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    text.chars()
        .map(|ch| scaled.h_advance(scaled.glyph_id(ch)))
        .sum()
}

/// One line of overlay text: `(text, size, line_height, colour)`.
type TextLine = (String, f32, f32, (u8, u8, u8));

// A low-level text blitter. The parameters are all genuinely independent, so
// bundling them into a struct would add ceremony without clarifying much.
#[allow(clippy::too_many_arguments)]
fn blend_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font: &FontVec,
    text: &str,
    x: f32,
    mut y: f32,
    size: f32,
    line_height: f32,
    color: (u8, u8, u8),
) -> f32 {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let (fr, fg, fb) = (color.0 as f32, color.1 as f32, color.2 as f32);
    for line in text.split('\n') {
        let mut pen_x = x;
        for ch in line.chars() {
            let glyph_id = scaled.glyph_id(ch);
            let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(pen_x, y));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                        return;
                    }
                    let idx = (py as usize * width as usize + px as usize) * 4;
                    let inv = 1.0 - coverage;
                    let bg_b = canvas[idx] as f32;
                    let bg_g = canvas[idx + 1] as f32;
                    let bg_r = canvas[idx + 2] as f32;
                    canvas[idx] = (fb * coverage + bg_b * inv) as u8;
                    canvas[idx + 1] = (fg * coverage + bg_g * inv) as u8;
                    canvas[idx + 2] = (fr * coverage + bg_r * inv) as u8;
                    canvas[idx + 3] = 0xff;
                });
            }
            pen_x += scaled.h_advance(glyph_id);
        }
        y += line_height;
    }
    y
}

/// A single ANSI style: an optional 24-bit colour and a bold flag. `paint` is a
/// no-op when colour is disabled, so callers can build a line unconditionally
/// and let the terminal decide.
#[derive(Clone, Copy)]
struct Style {
    rgb: Option<(u8, u8, u8)>,
    bold: bool,
}

impl Style {
    const fn fg(r: u8, g: u8, b: u8) -> Self {
        Self {
            rgb: Some((r, g, b)),
            bold: false,
        }
    }

    fn paint(self, text: &str, color: bool) -> String {
        if !color {
            return text.to_owned();
        }
        let mut out = String::new();
        if self.bold {
            out.push_str("\x1b[1m");
        }
        if let Some((r, g, b)) = self.rgb {
            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
        }
        out.push_str(text);
        out.push_str("\x1b[0m");
        out
    }
}

// Muted palette, echoing the overlay's default `[ui.colors]` so the console and
// the shutdown screen read as the same tool. Deliberately not bright.
const HEADING: Style = Style {
    rgb: Some((0xd8, 0xde, 0xe9)),
    bold: true,
};
const ACCENT: Style = Style::fg(0xac, 0xb5, 0xc7);
const MUTED: Style = Style::fg(0x80, 0x80, 0x80);
const ERROR: Style = Style {
    rgb: Some((0xb8, 0x66, 0x66)),
    bold: true,
};
const WARNING: Style = Style::fg(0xb0, 0x9a, 0x6a);

const USAGE: &str = "hyprdie [OPTIONS]";

/// Whether to emit ANSI colour for a stream. Colour is for a human at a
/// terminal only: pipes and log files get plain text, and `NO_COLOR` opts out
/// even on a TTY.
fn color_enabled(is_terminal: bool) -> bool {
    is_terminal && !no_color()
}

fn no_color() -> bool {
    env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

fn stderr_color() -> bool {
    color_enabled(std::io::stderr().is_terminal())
}

/// "hyprdie" shaded from grey to slate. The gradient is the one flourish,
/// mirroring hypr-recall's bin name in this project's quieter palette.
fn wordmark(color: bool) -> String {
    const TEXT: &str = "hyprdie";
    const FROM: (f32, f32, f32) = (0x6b as f32, 0x72 as f32, 0x80 as f32);
    const TO: (f32, f32, f32) = (0xac as f32, 0xb5 as f32, 0xc7 as f32);
    if !color {
        return TEXT.to_owned();
    }
    let steps = (TEXT.len() - 1).max(1) as f32;
    let mut out = String::from("\x1b[1m");
    for (index, ch) in TEXT.chars().enumerate() {
        let frac = index as f32 / steps;
        let r = (FROM.0 + (TO.0 - FROM.0) * frac) as u8;
        let g = (FROM.1 + (TO.1 - FROM.1) * frac) as u8;
        let b = (FROM.2 + (TO.2 - FROM.2) * frac) as u8;
        out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{ch}"));
    }
    out.push_str("\x1b[0m");
    out
}

/// The banner line at the top of `--help`. The tombstone sits outside the
/// gradient: terminals render emoji in their own colours, so there's nothing
/// useful to tint.
fn title_line(color: bool) -> String {
    format!(
        "🪦 {} — a graceful shutdown screen for Hyprland",
        wordmark(color)
    )
}

/// The `--version` line. The version comes from `Cargo.toml` at compile time,
/// so it can't drift from the released binary.
fn version_line(color: bool) -> String {
    format!("🪦 {} {}", wordmark(color), env!("CARGO_PKG_VERSION"))
}

fn print_version() {
    let color = color_enabled(std::io::stdout().is_terminal());
    println!("{}", version_line(color));
}

fn print_help() {
    let color = color_enabled(std::io::stdout().is_terminal());
    println!("{}", title_line(color));
    println!();
    println!(
        "{} {}",
        HEADING.paint("Usage:", color),
        ACCENT.paint(USAGE, color)
    );
    println!();
    println!("{}", HEADING.paint("Options:", color));
    option_row(
        color,
        "    --dry-run",
        None,
        "Show the overlay and close nothing",
    );
    option_row(
        color,
        "    --post-cmd",
        Some("<CMD>"),
        "Run CMD before exiting Hyprland",
    );
    option_row(
        color,
        "    --config",
        Some("<PATH>"),
        "Read configuration from PATH",
    );
    option_row(color, "-h, --help", None, "Print this help");
    option_row(color, "-V, --version", None, "Print version");
}

/// One `Options:` row. The flags and value are painted separately so the value
/// reads as a placeholder, but the padding is measured from the plain text so
/// ANSI escapes don't throw the description column off.
fn option_row(color: bool, flags: &str, value: Option<&str>, description: &str) {
    const DESCRIPTION_COLUMN: usize = 30;
    let mut plain = format!("  {flags}");
    if let Some(value) = value {
        plain.push(' ');
        plain.push_str(value);
    }
    let padding = " ".repeat(DESCRIPTION_COLUMN.saturating_sub(plain.len()));
    let mut rendered = ACCENT.paint(&format!("  {flags}"), color);
    if let Some(value) = value {
        rendered.push(' ');
        rendered.push_str(&MUTED.paint(value, color));
    }
    println!("{rendered}{padding}{description}");
}

/// Print a parse error in the same style as `--help`, followed by the usage
/// line.
fn print_error(message: &str) {
    let color = stderr_color();
    eprintln!("{} {message}", ERROR.paint("error:", color));
    eprintln!(
        "{} {}",
        HEADING.paint("Usage:", color),
        ACCENT.paint(USAGE, color)
    );
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CliOptions {
    dry_run: bool,
    post_command: Option<String>,
    config_path: Option<PathBuf>,
}

/// What the parser decided the arguments mean. `Help` and `Version` are not
/// errors: they're requests to print and exit successfully.
#[derive(Debug, PartialEq, Eq)]
enum CliOutcome {
    Options(CliOptions),
    Help,
    Version,
}

/// A command-line parse failure. A typed error rather than a `String` keeps the
/// messages testable and lets the caller choose how to render them.
#[derive(Debug, PartialEq, Eq)]
enum CliError {
    UnknownOption(String),
    MissingValue(&'static str),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::UnknownOption(option) => write!(f, "unknown option: {option}"),
            CliError::MissingValue(message) => write!(f, "{message}"),
        }
    }
}

/// Parse arguments (excluding the program name).
///
/// Returning a result instead of printing and exiting keeps this testable; the
/// thin `cli_options` wrapper below handles the exit.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliOutcome, CliError> {
    let mut options = CliOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => options.dry_run = true,
            "--post-cmd" => {
                options.post_command = Some(
                    args.next()
                        .ok_or(CliError::MissingValue("--post-cmd requires a command"))?,
                );
            }
            "--config" => {
                options.config_path = Some(PathBuf::from(
                    args.next()
                        .ok_or(CliError::MissingValue("--config requires a path"))?,
                ));
            }
            "--help" | "-h" => return Ok(CliOutcome::Help),
            "--version" | "-V" => return Ok(CliOutcome::Version),
            unknown => return Err(CliError::UnknownOption(unknown.to_owned())),
        }
    }
    Ok(CliOutcome::Options(options))
}

fn cli_options() -> CliOptions {
    match parse_args(env::args().skip(1)) {
        Ok(CliOutcome::Options(options)) => options,
        Ok(CliOutcome::Help) => {
            print_help();
            process::exit(0);
        }
        Ok(CliOutcome::Version) => {
            print_version();
            process::exit(0);
        }
        Err(error) => {
            print_error(&error.to_string());
            process::exit(2);
        }
    }
}

struct Ui {
    shared: Arc<Mutex<ShutdownState>>,
    interval: Duration,
    retry: Duration,
    last_poll: Instant,
    layer: LayerSurface,
    pool: SlotPool,
    width: u32,
    height: u32,
    first_configure: bool,
    /// Set once the overlay has been presented and the close sequence has been
    /// kicked off; keeps `frame` and `tick` from starting it twice.
    close_started: bool,
    /// When the surface was first configured, used to bound how long we wait
    /// for the frame callback before falling back to closing anyway.
    configured_at: Option<Instant>,
    image: Option<DynamicImage>,
    apps: Vec<(String, String)>,
    count: usize,
    title: String,
    font: Option<FontVec>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    stop: LoopSignal,
}
impl Ui {
    fn output_size(&self) -> Option<(u32, u32)> {
        for output in self.output_state.outputs() {
            if let Some(info) = self.output_state.info(&output) {
                if let Some((w, h)) = info.logical_size {
                    let scale = info.scale_factor.max(1) as u32;
                    if w > 0 && h > 0 {
                        return Some((w as u32 * scale, h as u32 * scale));
                    }
                }
                if let Some(mode) = info.modes.iter().find(|m| m.current) {
                    let (w, h) = mode.dimensions;
                    if w > 0 && h > 0 {
                        return Some((w as u32, h as u32));
                    }
                }
            }
        }
        None
    }
    fn draw(&mut self) {
        let (width, height) = if self.width > 1 && self.height > 1 {
            (self.width, self.height)
        } else {
            self.output_size()
                .unwrap_or((self.width.max(1), self.height.max(1)))
        };
        self.width = width;
        self.height = height;
        let stride = width as i32 * 4;
        let (dry_run, colors) = {
            let state = self.shared.lock().unwrap();
            (state.config.behavior.dry_run, state.config.ui.colors)
        };
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            return;
        };
        if let Some(image) = &self.image {
            let scaled = image
                .resize_to_fill(width, height, FilterType::Triangle)
                .to_rgba8();
            for (chunk, pixel) in canvas
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(scaled.pixels())
            {
                chunk.copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        } else {
            let solid = [
                colors.background.r,
                colors.background.g,
                colors.background.b,
                0xff,
            ];
            for chunk in canvas.as_chunks_mut::<4>().0 {
                chunk.copy_from_slice(&solid);
            }
        }
        if let Some(font) = &self.font {
            let title_color = colors.title.rgb();
            let heading_color = colors.heading.rgb();
            let app_color = colors.app.rgb();
            let hint_color = colors.hint.rgb();
            let mut lines: Vec<TextLine> = vec![(self.title.clone(), 44.0, 60.0, title_color)];
            let noun = if self.count == 1 { "app" } else { "apps" };
            let header = if dry_run {
                format!("Dry run — {} {noun} would be closed:", self.count)
            } else {
                format!("Waiting for {} {noun} to close:", self.count)
            };
            lines.push((header.to_string(), 24.0, 36.0, heading_color));
            for (class, title) in &self.apps {
                let line = if title.is_empty() {
                    class.clone()
                } else if class.is_empty() {
                    title.clone()
                } else {
                    format!("{class} — {title}")
                };
                lines.push((line, 20.0, 30.0, app_color));
            }
            let block_height: f32 = lines
                .iter()
                .map(|(_, _, line_height, _)| *line_height)
                .sum();
            let mut y = (height as f32 - block_height) / 2.0;
            for (text, size, line_height, color) in &lines {
                let line_width = measure_text(font, text, *size);
                let x = (width as f32 - line_width) / 2.0;
                y = blend_text(
                    canvas,
                    width,
                    height,
                    font,
                    text,
                    x,
                    y,
                    *size,
                    *line_height,
                    *color,
                );
            }
            let hint = if dry_run {
                "ESC — quit".to_string()
            } else {
                "ESC — cancel    F — force quit".to_string()
            };
            let hint_size = 16.0;
            let hint_x = (width as f32 - measure_text(font, &hint, hint_size)) / 2.0;
            let hint_y = height as f32 - 40.0;
            blend_text(
                canvas, width, height, font, &hint, hint_x, hint_y, hint_size, 0.0, hint_color,
            );
        }
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        let _ = buffer.attach_to(self.layer.wl_surface());
        self.layer.commit();
    }
    /// Kick off the graceful close sequence. This runs from the first frame
    /// callback — i.e. once the compositor has the overlay on screen — so apps
    /// don't start vanishing before the user can see why.
    fn start_closing(&mut self) {
        if self.close_started {
            return;
        }
        self.close_started = true;
        let mut state = self.shared.lock().unwrap();
        if state.config.behavior.dry_run {
            return;
        }
        retry_close(&mut state);
        state.last_retry = Instant::now();
    }
    fn finish(&mut self, force: bool) {
        let state = self.shared.lock().unwrap();
        if state.config.behavior.dry_run {
            self.stop.stop();
            return;
        }
        if force {
            // Forcing only changes *how* the apps close: skip the graceful
            // closewindow/SIGTERM/retry sequence and SIGKILL everything now.
            for pid in &state.pids {
                let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
            }
        }
        // The tail is the same whether apps closed on their own, were escalated
        // to SIGKILL by the timeout, or the user forced it. Run the post command
        // *before* exiting Hyprland: commands like `systemctl reboot` need the
        // session to still be active when they ask logind/polkit for
        // authorization, and teardown races them if we exit first.
        // `run_post_command` waits for it to finish.
        if let Some(command) = &state.config.commands.post {
            run_post_command(command);
        }
        if !state.config.behavior.no_exit {
            let _ = hyprctl(&["dispatch", "exit"]);
        }
        self.stop.stop();
    }
    fn tick(&mut self) {
        if self.last_poll.elapsed() < self.interval {
            return;
        }
        self.last_poll = Instant::now();
        // Backstop for `frame`: if the compositor has configured the overlay but
        // never delivered a frame callback, don't leave the user stuck on it.
        if !self.close_started
            && let Some(configured_at) = self.configured_at
            && configured_at.elapsed() >= CLOSE_FALLBACK_DELAY
        {
            self.start_closing();
        }
        let mut state = self.shared.lock().unwrap();
        refresh_clients(&mut state);
        // Nothing is closed until the first frame callback fires, so don't treat
        // an empty (or already-closed) session as "done" before then.
        if !state.config.behavior.dry_run && self.close_started {
            if alive(&state.pids) == 0 {
                drop(state);
                self.finish(false);
                return;
            }
            if state.last_retry.elapsed() >= self.retry {
                retry_close(&mut state);
                state.last_retry = Instant::now();
            }
        }
        let apps = state.apps.clone();
        drop(state);
        if apps != self.apps {
            self.apps = apps;
            self.count = self.apps.len();
            self.draw();
        }
    }
}

impl CompositorHandler for Ui {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.start_closing();
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Ui {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl LayerShellHandler for Ui {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.stop.stop();
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (width, height) = if configure.new_size.0 > 0 && configure.new_size.1 > 0 {
            (configure.new_size.0, configure.new_size.1)
        } else {
            self.output_size().unwrap_or((self.width, self.height))
        };
        let size_changed = width != self.width || height != self.height;
        self.width = width;
        self.height = height;
        if self.first_configure {
            self.first_configure = false;
            self.configured_at = Some(Instant::now());
            // Request the frame callback *before* committing the first buffer:
            // it fires once the compositor has the overlay on screen, which is
            // when `frame` starts closing apps. Waiting avoids closing them
            // before the user ever sees the overlay.
            let surface = layer.wl_surface();
            surface.frame(qh, surface.clone());
            self.draw();
        } else if size_changed {
            // A later configure (monitor hotplug, resolution change) resizes the
            // surface; redraw or the overlay keeps a buffer at the old size.
            self.draw();
        }
    }
}
impl SeatHandler for Ui {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}
/// Whether a keysym is the force-quit key. The keysym arrives after the current
/// modifiers are applied, so a bare F key is `Keysym::f` (0x66) while `Keysym::F`
/// (0x46) is Shift+f — and Caps Lock flips it too. Accept both so the advertised
/// `F` works regardless.
fn is_force_key(keysym: Keysym) -> bool {
    matches!(keysym, Keysym::f | Keysym::F)
}

impl KeyboardHandler for Ui {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            self.stop.stop();
        } else if is_force_key(event.keysym) {
            self.finish(true);
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: u32,
    ) {
    }
}
impl ShmHandler for Ui {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for Ui {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Ui);
delegate_output!(Ui);
delegate_shm!(Ui);
delegate_seat!(Ui);
delegate_keyboard!(Ui);
delegate_layer!(Ui);
delegate_registry!(Ui);

fn main() {
    // Parse before the Hyprland guard so `--help` and argument errors work
    // anywhere, not only inside a session.
    let options = cli_options();

    // Check that we're actually running under Hyprland
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_err() {
        let color = stderr_color();
        eprintln!(
            "{} HYPRLAND_INSTANCE_SIGNATURE not found. Are you running under Hyprland?",
            ERROR.paint("error:", color)
        );
        eprintln!("hyprdie must be run from within a Hyprland session.");
        process::exit(1);
    }

    // Closing our launching terminal (a Wayland client) hangs up the pty and
    // sends SIGHUP to its foreground process group — which includes us. Ignore
    // it so we survive long enough to reach finish() and run the post command.
    unsafe {
        let _ = signal(Signal::SIGHUP, SigHandler::SigIgn);
    }
    let explicit_config = options.config_path.is_some();
    let config_file = options.config_path.clone().unwrap_or_else(config_path);
    let mut config = match load_config(&config_file) {
        Ok(config) => config,
        // An explicitly requested config that can't be read is an error: silently
        // falling back to defaults would run with settings the user didn't ask
        // for. The default location is different — it's often absent, so warn.
        Err(error) if explicit_config => {
            let color = stderr_color();
            eprintln!("{} {error}", ERROR.paint("error:", color));
            process::exit(1);
        }
        Err(error) => {
            let color = stderr_color();
            eprintln!(
                "{} {error}; using defaults",
                WARNING.paint("warning:", color)
            );
            Config::default()
        }
    };
    if options.dry_run {
        config.behavior.dry_run = true;
    }
    if options.post_command.is_some() {
        config.commands.post = options.post_command;
    }
    let interval = config.behavior.poll_interval_ms.max(50);
    let retry = config.behavior.retry_interval_ms.max(interval);
    let shared = Arc::new(Mutex::new(ShutdownState {
        pids: HashSet::new(),
        hypr_children: HashSet::new(),
        addresses: Vec::new(),
        apps: Vec::new(),
        last_retry: Instant::now(),
        term_started: HashMap::new(),
        config: config.clone(),
    }));
    let conn = Connection::connect_to_env().expect("could not connect to Wayland");
    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("could not initialize Wayland registry");
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr layer shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm is not available");
    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("hyprdie"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.commit();
    let image = config
        .ui
        .background
        .as_deref()
        .map(expand_path)
        .and_then(|path| {
            match fs::read(&path)
                .map_err(image::ImageError::IoError)
                .and_then(|bytes| {
                    ImageReader::new(Cursor::new(bytes))
                        .with_guessed_format()
                        .map_err(image::ImageError::IoError)
                        .and_then(|reader| reader.decode())
                }) {
                Ok(image) => Some(image),
                Err(error) => {
                    eprintln!("could not load background: {error}");
                    None
                }
            }
        });
    let mut event_loop = EventLoop::<Ui>::try_new().expect("could not create event loop");
    let stop = event_loop.get_signal();
    discover(&mut shared.lock().unwrap());
    let apps = shared.lock().unwrap().apps.clone();
    let count = apps.len();
    let font = load_font();
    let pool = SlotPool::new(32 * 1024 * 1024, &shm).expect("could not create SHM pool");
    let registry_state = RegistryState::new(&globals);
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let mut ui = Ui {
        shared: shared.clone(),
        interval: Duration::from_millis(interval),
        retry: Duration::from_millis(retry),
        last_poll: Instant::now() - Duration::from_millis(interval),
        layer,
        pool,
        width: 1,
        height: 1,
        first_configure: true,
        close_started: false,
        configured_at: None,
        image,
        apps,
        count,
        title: config.ui.title.clone(),
        font,
        keyboard: None,
        registry_state,
        seat_state,
        output_state,
        shm,
        stop,
    };
    event_queue
        .roundtrip(&mut ui)
        .expect("could not roundtrip Wayland queue");
    let timer = Timer::from_duration(Duration::from_millis(20));
    event_loop
        .handle()
        .insert_source(timer, |_, _, ui| {
            ui.tick();
            TimeoutAction::ToDuration(Duration::from_millis(20))
        })
        .unwrap();
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .unwrap();
    event_loop
        .run(None, &mut ui, |_| {})
        .expect("Wayland event loop failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_sensible() {
        assert_eq!(Config::default().ui.title, "Ending session...");
        assert_eq!(Config::default().behavior.poll_interval_ms, 150);
        assert_eq!(Config::default().behavior.sigkill_timeout_ms, 10_000);
    }

    #[test]
    fn parses_cgroup_v2_unified() {
        assert_eq!(
            parse_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n").as_deref(),
            Some("/user.slice/user-1000.slice/session-2.scope")
        );
    }

    #[test]
    fn parses_cgroup_v1_controller_line() {
        assert_eq!(
            parse_cgroup("11:memory:/user.slice/user-1000.slice/session-2.scope\n").as_deref(),
            Some("/user.slice/user-1000.slice/session-2.scope")
        );
    }

    #[test]
    fn root_cgroup_parses_as_slash() {
        assert_eq!(parse_cgroup("0::/\n").as_deref(), Some("/"));
    }

    #[test]
    fn parses_color_hex() {
        let colors: UiColors = toml::from_str("title = \"#ff8000\"").unwrap();
        assert_eq!(
            colors.title,
            Rgb {
                r: 0xff,
                g: 0x80,
                b: 0x00
            }
        );
    }

    #[test]
    fn rejects_invalid_color() {
        assert!(toml::from_str::<UiColors>("title = \"#12\"").is_err());
        assert!(toml::from_str::<UiColors>("title = \"#zzzzzz\"").is_err());
    }

    #[test]
    fn force_key_accepts_plain_and_shifted_f() {
        assert!(is_force_key(Keysym::f));
        assert!(is_force_key(Keysym::F));
        assert!(!is_force_key(Keysym::Escape));
        assert!(!is_force_key(Keysym::g));
    }

    #[test]
    fn ppid_of_missing_process_is_none() {
        assert_eq!(ppid_of(i32::MAX), None);
    }

    #[test]
    fn session_descendants_never_target_ancestors() {
        let me = process::id() as i32;
        // Whether discovery goes through the cgroup or the ppid walk, our own
        // parent — an ancestor like the DM's session helper — must never end up
        // in the kill set.
        if let Some(parent) = ppid_of(me) {
            assert!(!session_descendants(me).contains(&parent));
        }
    }

    #[test]
    fn ancestors_walk_up_without_self_or_init() {
        let me = process::id() as i32;
        let ancestors = ancestors(me);
        assert!(ancestors.contains(&ppid_of(me).unwrap()));
        assert!(!ancestors.contains(&me));
        assert!(!ancestors.contains(&1));
    }

    #[test]
    fn running_process_is_alive() {
        assert!(is_alive(process::id() as i32));
    }

    #[test]
    fn missing_process_is_not_alive() {
        assert!(!is_alive(i32::MAX));
    }

    #[test]
    fn zombie_is_not_alive() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        // `true` exits at once; without reaping it, it stays a zombie.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
                && let Some(close) = stat.rfind(')')
                && stat[close + 1..].trim_start().starts_with('Z')
            {
                break;
            }
            assert!(Instant::now() < deadline, "child never became a zombie");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!is_alive(pid));
        let _ = child.wait();
    }

    #[test]
    fn parses_config_colors_with_defaults() {
        let config: Config = toml::from_str(
            r##"
            [ui.colors]
            title = "#ff0000"
            "##,
        )
        .unwrap();
        assert_eq!(
            config.ui.colors.title,
            Rgb {
                r: 0xff,
                g: 0x00,
                b: 0x00
            }
        );
        assert_eq!(
            config.ui.colors.hint,
            Rgb {
                r: 0x80,
                g: 0x80,
                b: 0x80
            }
        );
        assert_eq!(
            config.ui.colors.background,
            Rgb {
                r: 0x1b,
                g: 0x18,
                b: 0x18
            }
        );
    }

    #[test]
    fn parses_cli_flags() {
        let args = [
            "--dry-run",
            "--post-cmd",
            "systemctl reboot",
            "--config",
            "/tmp/hyprdie.toml",
        ]
        .map(String::from);
        let Ok(CliOutcome::Options(options)) = parse_args(args) else {
            panic!("expected parsed options");
        };
        assert!(options.dry_run);
        assert_eq!(options.post_command.as_deref(), Some("systemctl reboot"));
        assert_eq!(
            options.config_path.as_deref(),
            Some(Path::new("/tmp/hyprdie.toml"))
        );
    }

    #[test]
    fn empty_cli_is_all_defaults() {
        let Ok(CliOutcome::Options(options)) = parse_args(std::iter::empty::<String>()) else {
            panic!("expected parsed options");
        };
        assert_eq!(options, CliOptions::default());
    }

    #[test]
    fn help_is_not_an_error() {
        assert_eq!(parse_args([String::from("--help")]), Ok(CliOutcome::Help));
        assert_eq!(parse_args([String::from("-h")]), Ok(CliOutcome::Help));
    }

    #[test]
    fn version_is_not_an_error() {
        assert_eq!(
            parse_args([String::from("--version")]),
            Ok(CliOutcome::Version)
        );
        assert_eq!(parse_args([String::from("-V")]), Ok(CliOutcome::Version));
    }

    #[test]
    fn version_line_reports_the_crate_version() {
        let plain = version_line(false);
        assert!(plain.contains(env!("CARGO_PKG_VERSION")), "{plain}");
        assert!(plain.contains("hyprdie"), "{plain}");
    }

    #[test]
    fn options_missing_a_value_are_errors() {
        assert_eq!(
            parse_args([String::from("--post-cmd")]),
            Err(CliError::MissingValue("--post-cmd requires a command"))
        );
        assert_eq!(
            parse_args([String::from("--config")]),
            Err(CliError::MissingValue("--config requires a path"))
        );
    }

    #[test]
    fn unknown_option_is_an_error() {
        assert_eq!(
            parse_args([String::from("--nope")]),
            Err(CliError::UnknownOption("--nope".to_owned()))
        );
    }

    #[test]
    fn paint_is_plain_without_colour() {
        assert_eq!(ACCENT.paint("--dry-run", false), "--dry-run");
        assert_eq!(ERROR.paint("error:", false), "error:");
    }

    #[test]
    fn paint_wraps_text_in_ansi_when_enabled() {
        let painted = ACCENT.paint("--dry-run", true);
        assert!(painted.contains("--dry-run"));
        assert!(painted.starts_with("\x1b["));
        assert!(painted.ends_with("\x1b[0m"));
    }

    #[test]
    fn wordmark_is_plain_without_colour() {
        assert_eq!(wordmark(false), "hyprdie");
    }

    #[test]
    fn title_line_has_the_tombstone_and_wordmark() {
        let plain = title_line(false);
        assert!(plain.starts_with('🪦'), "{plain}");
        assert!(plain.contains("hyprdie"), "{plain}");
        // Colour, when on, tints the wordmark but never the tombstone.
        assert!(title_line(true).contains("🪦"));
    }

    #[test]
    fn wordmark_colours_each_letter_independently() {
        let painted = wordmark(true);
        for ch in "hyprdie".chars() {
            assert!(painted.contains(ch), "missing {ch}");
        }
        // One truecolour sequence per character, plus the leading bold and the
        // trailing reset.
        assert_eq!(painted.matches("\x1b[38;2;").count(), 7);
    }

    #[test]
    fn text_measurement_grows_with_length() {
        // Skip where no candidate font is installed rather than fail a machine
        // that has none: this checks arithmetic, not font availability.
        let Some(font) = load_font() else {
            return;
        };
        assert_eq!(measure_text(&font, "", 20.0), 0.0);
        let short = measure_text(&font, "a", 20.0);
        let long = measure_text(&font, "aaaa", 20.0);
        assert!(long > short, "{long} should exceed {short}");
    }
}
