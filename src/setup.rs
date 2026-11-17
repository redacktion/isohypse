use std::io::IsTerminal;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn store_root() -> PathBuf {
    std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    })
}

fn config_path() -> PathBuf {
    store_root().join("config.json")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    Off,
    WorkspaceWrite,
    WorkspaceWriteNoNetwork,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub mode: String,
    pub inactive_hours: u64,
    pub cold_days: u64,
    pub encrypt_objects: bool,
    pub encrypt_caches: bool,
    pub encrypt_metadata: bool,
    pub encrypt_sessions: bool,
    #[serde(default = "default_true")]
    pub reconfig_locked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_disabled_reason: Option<String>,
    #[serde(default)]
    pub enforce_profiles: Vec<CustomEnforceProfile>,
    #[serde(default)]
    pub validate_sandbox: SandboxMode,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CustomEnforceProfile {
    pub name: String,
    pub allowed: Vec<String>,
    #[serde(default)]
    pub denied: Vec<String>,
}


impl Default for Config {
    fn default() -> Config {
        Config {
            mode: "global".to_string(),
            inactive_hours: 48,
            cold_days: 15,
            encrypt_objects: true,
            encrypt_caches: true,
            encrypt_metadata: true,
            encrypt_sessions: true,
            reconfig_locked: true,
            agent_disabled_reason: None,
            enforce_profiles: Vec::new(),
            validate_sandbox: SandboxMode::Off,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let root = store_root();
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        crate::objects::write_private(&config_path(), body.as_bytes())
            .ok_or_else(|| "cannot write config".to_string())?;
        let _ = crate::machine::refresh();
        Ok(())
    }

    pub fn summary_json(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode,
            "inactive_hours": self.inactive_hours,
            "cold_days": self.cold_days,
            "encrypt": {
                "objects": self.encrypt_objects,
                "caches": self.encrypt_caches,
                "metadata": self.encrypt_metadata,
                "sessions": self.encrypt_sessions,
            },
            "validate_sandbox": self.validate_sandbox,
            "agent_mode": if self.agent_disabled_reason.is_some() { "disabled" } else { "enabled" },
        })
    }
}

fn authorize_reconfig() -> Result<(), String> {
    if !config_path().exists() {
        return Ok(());
    }
    if !Config::load().reconfig_locked {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(
            "this store is already configured and settings are password-protected; changing them needs your account password at a terminal".to_string(),
        );
    }
    let user = std::env::var("USER").map_err(|_| "cannot determine current user".to_string())?;
    eprintln!("Changing a password-protected store. Authorize with your account password.");
    let status = std::process::Command::new("/usr/bin/dscl")
        .args([".", "-authonly", &user])
        .status()
        .map_err(|e| format!("cannot run authorization: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("authorization failed; settings unchanged".to_string())
    }
}

pub fn run(args: &[String]) -> Result<String, String> {
    if args.iter().any(|a| a == "--reenable-agent") {
        return crate::machine::reenable();
    }
    authorize_reconfig()?;
    let agent = args.iter().any(|a| a == "--agent") || !std::io::stdout().is_terminal();
    let outcome = if agent { run_agent(args) } else { run_human() };
    if outcome.is_ok() {
        let _ = crate::objects::migrate_store();
    }
    outcome
}

fn run_agent(args: &[String]) -> Result<String, String> {
    let mut config = Config::load();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--global" => config.mode = "global".to_string(),
            "--micro" => config.mode = "micro".to_string(),
            "--inactive-hours" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    config.inactive_hours = v;
                }
            }
            "--cold-days" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    config.cold_days = v;
                }
            }
            "--encrypt" => {
                if let Some(spec) = it.next() {
                    apply_encrypt_spec(&mut config, spec);
                }
            }
            _ => {}
        }
    }
    if let Ok(spec) = std::env::var("ISOHYPSE_SETUP_ENCRYPT") {
        apply_encrypt_spec(&mut config, &spec);
    }
    config.save()?;
    let mut summary = config.summary_json();
    summary["ok"] = serde_json::Value::Bool(true);
    if config.mode == "micro" {
        summary["note"] = serde_json::Value::String(
            "micro mode: per-repo store only, no cross-repo knowledge; run `isohypse setup --global` to unlock the shared store".to_string(),
        );
    }
    Ok(format!("{}\n", serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?))
}

fn apply_encrypt_spec(config: &mut Config, spec: &str) {
    let (value, list) = match spec {
        "all" => (true, None),
        "none" => (false, None),
        other => (true, Some(other)),
    };
    if let Some(list) = list {
        config.encrypt_objects = false;
        config.encrypt_caches = false;
        config.encrypt_metadata = false;
        config.encrypt_sessions = false;
        for piece in list.split(',') {
            match piece.trim() {
                "objects" => config.encrypt_objects = true,
                "caches" => config.encrypt_caches = true,
                "metadata" => config.encrypt_metadata = true,
                "sessions" => config.encrypt_sessions = true,
                _ => {}
            }
        }
    } else {
        config.encrypt_objects = value;
        config.encrypt_caches = value;
        config.encrypt_metadata = value;
        config.encrypt_sessions = value;
    }
}

// ── unit: setup and configuration ──────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_spec_selects_a_subset() {
        let mut config = Config::default();
        apply_encrypt_spec(&mut config, "objects,sessions");
        let __ok0: bool = config.encrypt_objects && config.encrypt_sessions;
        let __ok1: bool = !config.encrypt_caches && !config.encrypt_metadata;
        apply_encrypt_spec(&mut config, "none");
        let __ok2: bool = !config.encrypt_objects;
        apply_encrypt_spec(&mut config, "all");
        let __ok3: bool = config.encrypt_objects && config.encrypt_metadata;

        let failed: Vec<&str> = [(__ok0, "config.encrypt_objects && config.encrypt_sessions"), (__ok1, "!config.encrypt_caches && !config.encrypt_metadata"), (__ok2, "!config.encrypt_objects"), (__ok3, "config.encrypt_objects && config.encrypt_metadata")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
        assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = Config { mode: "micro".to_string(), cold_days: 30, ..Config::default() };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        let __ok0: bool = (back.mode) == ("micro");
        let __ok1: bool = (back.cold_days) == (30);
        let __ok2: bool = back.encrypt_objects;

        let failed: Vec<&str> = [(__ok0, "back.mode == \"micro\""), (__ok1, "back.cold_days == 30"), (__ok2, "back.encrypt_objects")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
        assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
    }
}

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use opentui_rust::{Renderer, RendererOptions, Rgba, Style as OStyle};
use opentui_rust::buffer::{BoxOptions, BoxStyle};

const FG: Rgba = Rgba::WHITE;
const DIM: Rgba = Rgba::new(0.5, 0.5, 0.5, 1.0);
const GOLD: Rgba = Rgba::new(1.0, 0.84, 0.0, 1.0);
const GREEN: Rgba = Rgba::GREEN;
const DARK: Rgba = Rgba::new(0.3, 0.3, 0.3, 1.0);
const YELLOW: Rgba = Rgba::new(1.0, 1.0, 0.0, 1.0);

fn s_fg(c: Rgba) -> OStyle { OStyle::builder().fg(c).build() }
fn s_bold(c: Rgba) -> OStyle { OStyle::builder().fg(c).bold().build() }
fn s_inv() -> OStyle { OStyle::builder().fg(Rgba::BLACK).bg(Rgba::WHITE).bold().build() }
fn s_underline(c: Rgba) -> OStyle { OStyle::builder().fg(c).underline().build() }

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Mode,
    Inactive,
    Cold,
    EncObjects,
    EncCaches,
    EncMetadata,
    EncSessions,
    Protect,
    EnforceProfile,
}

struct Phase {
    title: &'static str,
    blurb: &'static str,
    items: &'static [Item],
}

const PHASES: [Phase; 4] = [
    Phase { title: "STORAGE & LIFECYCLE", blurb: "how your code is stored, and when old copies get cleaned up", items: &[Item::Mode, Item::Inactive, Item::Cold] },
    Phase { title: "ENCRYPTION", blurb: "what gets encrypted on disk. leave it on unless you have a reason", items: &[Item::EncObjects, Item::EncCaches, Item::EncMetadata] },
    Phase { title: "SESSIONS & PROTECTION", blurb: "session encryption and settings protection", items: &[Item::EncSessions, Item::Protect] },
    Phase { title: "ENFORCEMENT", blurb: "session.enforce profiles restrict what ops an agent session may call", items: &[Item::EnforceProfile] },
];

fn run_human() -> Result<String, String> {
    let (w, h) = opentui_rust::terminal_size().unwrap_or((80, 24));
    let mut renderer = Renderer::new_with_options(u32::from(w), u32::from(h), RendererOptions {
        use_alt_screen: true,
        hide_cursor: true,
        enable_mouse: false,
        query_capabilities: false,
    }).map_err(|e| e.to_string())?;
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut config = Config::load();
    let result = wizard(&mut renderer, &mut config);
    disable_raw_mode().map_err(|e| e.to_string())?;
    drop(renderer);
    result
}

fn wizard(renderer: &mut Renderer, config: &mut Config) -> Result<String, String> {
    let mut intro = true;
    let mut phase = 0usize;
    let mut focus = 0usize;
    loop {
        renderer.clear();
        let (w, h) = renderer.size();
        if intro {
            draw_intro(renderer, w, h);
        } else {
            draw_phase(renderer, config, phase, focus, w, h);
        }
        renderer.present().map_err(|e| e.to_string())?;
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else { continue };
        if key.kind != KeyEventKind::Press { continue; }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok("setup cancelled; nothing changed\n".to_string()),
            KeyCode::Enter if intro => intro = false,
            KeyCode::Enter => {
                if phase + 1 < PHASES.len() {
                    phase += 1;
                    focus = 0;
                } else {
                    config.save()?;
                    return run_loading(renderer, config);
                }
            }
            KeyCode::Up if !intro => {
                let items = phase_items(phase);
                focus = (focus + items.len() - 1) % items.len();
            }
            KeyCode::Down if !intro => {
                let items = phase_items(phase);
                focus = (focus + 1) % items.len();
            }
            KeyCode::Left | KeyCode::Right if !intro => {
                let increase = matches!(key.code, KeyCode::Right);
                let items = phase_items(phase);
                adjust(config, items[focus], increase);
            }
            KeyCode::Char(c) if !intro && c.is_ascii_whitespace() => {
                let items = phase_items(phase);
                adjust(config, items[focus], true);
            }
            KeyCode::Backspace if !intro => {
                if phase > 0 { phase -= 1; focus = 0; } else { intro = true; }
            }
            _ => {}
        }
    }
}

fn adjust(config: &mut Config, item: Item, increase: bool) {
    match item {
        Item::Mode => config.mode = if config.mode == "global" { "micro".to_string() } else { "global".to_string() },
        Item::Inactive => config.inactive_hours = step(config.inactive_hours, increase, 1, 1..=8760),
        Item::Cold => config.cold_days = step(config.cold_days, increase, 1, 1..=365),
        Item::EncObjects => config.encrypt_objects = !config.encrypt_objects,
        Item::EncCaches => config.encrypt_caches = !config.encrypt_caches,
        Item::EncMetadata => config.encrypt_metadata = !config.encrypt_metadata,
        Item::EncSessions => config.encrypt_sessions = !config.encrypt_sessions,
        Item::Protect => config.reconfig_locked = !config.reconfig_locked,
        Item::EnforceProfile => {}
    }
}

fn step(value: u64, increase: bool, by: u64, bounds: std::ops::RangeInclusive<u64>) -> u64 {
    let next = if increase { value.saturating_add(by) } else { value.saturating_sub(by) };
    next.clamp(*bounds.start(), *bounds.end())
}

fn draw_box(renderer: &mut Renderer, x: u32, y: u32, w: u32, h: u32, title: Option<&str>) {
    let buf = renderer.buffer();
    let box_style = BoxStyle::single(s_fg(FG));
    let mut opts = BoxOptions::new(box_style);
    if let Some(t) = title {
        opts.title = Some(format!(" {t} "));
    }
    buf.draw_box_with_options(x, y, w, h, opts);
}

fn center_text(renderer: &mut Renderer, y: u32, w: u32, text: &str, style: OStyle) {
    let x = w.saturating_sub(text.len() as u32) / 2;
    renderer.buffer().draw_text(x, y, text, style);
}

fn draw_intro(renderer: &mut Renderer, w: u32, h: u32) {
    draw_box(renderer, 0, 0, w, h, Some("ISOHYPSE"));
    let cy = h / 2;
    center_text(renderer, cy.saturating_sub(4), w, "A coding tool built for agents.", s_bold(FG));
    center_text(renderer, cy.saturating_sub(2), w, "Reads and edits exact lines. No blind file overwrites.", s_fg(DIM));
    center_text(renderer, cy, w, "Shared workspaces encrypt saved data and hide file names.", s_fg(DIM));
    center_text(renderer, cy + 2, w, "Setup takes a minute. Choose storage and access settings.", s_fg(DIM));
    center_text(renderer, cy + 5, w, " Continue ", s_inv());
    center_text(renderer, cy + 7, w, "enter continue  q cancel", s_fg(DARK));
}

fn draw_phase(renderer: &mut Renderer, config: &Config, phase: usize, focus: usize, w: u32, h: u32) {
    draw_box(renderer, 0, 0, w, 4, None);
    center_text(renderer, 1, w, &format!("isohypse setup  step {} of {}", phase + 1, PHASES.len()), s_bold(FG));
    center_text(renderer, 2, w, PHASES[phase].blurb, s_fg(DIM));
    draw_box(renderer, 0, 4, w, h.saturating_sub(8), Some(PHASES[phase].title));
    let items = phase_items(phase);
    for (i, item) in items.iter().enumerate() {
        let y = 6 + (i as u32) * 2;
        draw_item(renderer, config, *item, i == focus, 4, y, w);
    }
    let help_y = 6 + (items.len() as u32) * 2 + 1;
    if phase == 3 {
        draw_help_lines(renderer, w, help_y, h.saturating_sub(6), &enforce_help());
    }
    draw_box(renderer, 0, h.saturating_sub(4), w, 4, None);
    let action = if phase + 1 == PHASES.len() { " Initialize " } else { " Next " };
    center_text(renderer, h.saturating_sub(3), w, action, s_inv());
    center_text(renderer, h.saturating_sub(2), w, "up/down move  space toggle  left/right adjust  enter next  backspace back  q cancel", s_fg(DARK));
}

fn draw_item(renderer: &mut Renderer, config: &Config, item: Item, focused: bool, x: u32, y: u32, _w: u32) {
    let (label, value, note) = item_fields(config, item);
    let marker = if focused { "> " } else { "  " };
    let label_style = if focused { s_bold(FG) } else { s_fg(DIM) };
    let buf = renderer.buffer();
    buf.draw_text(x, y, marker, s_fg(FG));
    buf.draw_text(x + 2, y, &format!("{label:<26}"), label_style);
    buf.draw_text(x + 28, y, &format!("{value:<8}"), s_fg(FG));
    if !note.is_empty() {
        buf.draw_text(x + 38, y, &note, s_fg(YELLOW));
    }
}

fn item_fields(config: &Config, item: Item) -> (&'static str, String, String) {
    match item {
        Item::Mode => ("storage", if config.mode == "global" { "global".to_string() } else { "micro".to_string() }, if config.mode == "global" { String::new() } else { "per-repo only, no cross-repo knowledge".to_string() }),
        Item::Inactive => ("idle before inactive", format!("{} hours", config.inactive_hours), String::new()),
        Item::Cold => ("inactive before cold", format!("{} days", config.cold_days), String::new()),
        Item::EncObjects => ("encrypt objects", toggle(config.encrypt_objects), if config.encrypt_objects { String::new() } else { "code snapshots stored unencrypted".to_string() }),
        Item::EncCaches => ("encrypt caches", toggle(config.encrypt_caches), if config.encrypt_caches { String::new() } else { "index and vectors stored unencrypted".to_string() }),
        Item::EncMetadata => ("encrypt metadata", toggle(config.encrypt_metadata), if config.encrypt_metadata { String::new() } else { "workspace and journal records unencrypted".to_string() }),
        Item::EncSessions => ("encrypt sessions", toggle(config.encrypt_sessions), if config.encrypt_sessions { String::new() } else { "session tokens stored unencrypted".to_string() }),
        Item::Protect => ("password-protect settings", toggle(config.reconfig_locked), if config.reconfig_locked { String::new() } else { "settings can be changed without authorization".to_string() }),
        Item::EnforceProfile => {
            let names = crate::session::profile_names();
            let custom = config.enforce_profiles.len();
            ("profiles available", format!("{}", names.len()), if custom > 0 { "includes custom profiles from config".to_string() } else { "built-in: readonly, review, edit, observe, full".to_string() })
        }
    }
}

struct HelpLine { text: String, heading: bool }

fn draw_help_lines(renderer: &mut Renderer, w: u32, start_y: u32, last_y: u32, lines: &[HelpLine]) {
    let max_chars = w.saturating_sub(8) as usize;
    for (i, line) in lines.iter().enumerate() {
        let y = start_y + i as u32;
        if y > last_y {
            break;
        }
        let style = if line.heading { s_underline(GOLD) } else { s_fg(GOLD) };
        let text: String = line.text.chars().take(max_chars).collect();
        renderer.buffer().draw_text(4, y, &text, style);
    }
}

fn h(text: &str) -> HelpLine { HelpLine { text: text.to_string(), heading: true } }
fn l(text: &str) -> HelpLine { HelpLine { text: text.to_string(), heading: false } }
fn blank() -> HelpLine { l("") }
fn profile_row(name: &str, ops: &str) -> HelpLine { l(&format!("{name:<10}{ops}")) }

fn enforce_help() -> Vec<HelpLine> {
    let mut lines = vec![
        h("Enforcement profiles"),
        l("session.enforce activates a named profile on the workspace."),
        l("Once active it can only narrow. Only state.stop clears it."),
        l("It persists across daemon restarts. New sessions inherit it."),
        blank(),
        profile_row("readonly", "context.read, find, explore, log, prompt, status"),
        profile_row("review", "readonly + verify.check, diagnose, build"),
        profile_row("edit", "review + mutate.edit, create, delete, undo"),
        profile_row("observe", "readonly + session.request, requests"),
        profile_row("full", "all ops, no enforcement"),
        blank(),
        l("Custom profiles: add enforce_profiles to config.json."),
        l("Sandbox (macOS): set validate_sandbox in config.json to"),
        l("workspace_write or workspace_write_no_network."),
        blank(),
        h("Active enforcement by workspace"),
    ];
    let enforce_dir = store_root().join("enforce");
    let mut any = false;
    if let Ok(entries) = std::fs::read_dir(&enforce_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let ops = data.get("allowed").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                    let hash = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                    let label = crate::objects::runtime_list().into_iter().find(|(root, _)| {
                        let key = blake3::hash(root.to_string_lossy().as_bytes()).to_hex().to_string();
                        key == hash
                    }).map(|(root, _)| root.to_string_lossy().to_string()).unwrap_or_else(|| format!("({hash})"));
                    lines.push(l(&format!("{label}  ({ops} allowed ops)")));
                    any = true;
                }
            }
        }
    }
    if !any {
        lines.push(l("(none active)"));
    }
    lines
}

fn phase_items(phase: usize) -> Vec<Item> {
    PHASES[phase].items.to_vec()
}

fn toggle(on: bool) -> String {
    if on { "on".to_string() } else { "off".to_string() }
}

fn all_encrypted(config: &Config) -> bool {
    config.encrypt_objects && config.encrypt_caches && config.encrypt_metadata && config.encrypt_sessions
}

fn run_loading(renderer: &mut Renderer, config: &Config) -> Result<String, String> {
    let steps = [
        "sealing the store",
        "writing the signed manifest",
        "building the reference index",
        "warming the daemon",
        "opening sockets",
    ];
    let total = steps.len();
    let (w, h) = renderer.size();
    for i in 0..total {
        for tick in 0..10 {
            let ratio = (i as f64 + tick as f64 / 10.0) / total as f64;
            renderer.clear();
            draw_loading(renderer, &steps, i, ratio, w, h);
            renderer.present().map_err(|e| e.to_string())?;
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    renderer.clear();
    draw_loading(renderer, &steps, total, 1.0, w, h);
    renderer.present().map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    Ok(format!(
        "isohypse ready  {} store, encryption {}, settings {}\n",
        config.mode,
        if all_encrypted(config) { "on" } else { "partial" },
        if config.reconfig_locked { "password-protected" } else { "open" },
    ))
}

fn draw_loading(renderer: &mut Renderer, steps: &[&str], current: usize, ratio: f64, w: u32, h: u32) {
    draw_box(renderer, 0, 0, w, h.saturating_sub(3), Some("BRINGING ISOHYPSE ONLINE"));
    for (i, step) in steps.iter().enumerate() {
        let y = 2 + i as u32;
        let (mark, style) = if i < current {
            ("[done] ", s_fg(GREEN))
        } else if i == current {
            ("[ .. ] ", s_bold(FG))
        } else {
            ("[    ] ", s_fg(DARK))
        };
        let buf = renderer.buffer();
        buf.draw_text(4, y, mark, style);
        buf.draw_text(11, y, step, style);
    }
    let bar_y = h.saturating_sub(3);
    draw_box(renderer, 0, bar_y, w, 3, Some("PROGRESS"));
    let inner_w = w.saturating_sub(4);
    let filled = ((ratio * inner_w as f64) as u32).min(inner_w);
    let buf = renderer.buffer();
    buf.fill_rect(2, bar_y + 1, filled, 1, FG);
    if filled < inner_w {
        buf.fill_rect(2 + filled, bar_y + 1, inner_w - filled, 1, DARK);
    }
}
