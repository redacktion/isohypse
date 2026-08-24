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
}

fn default_true() -> bool {
    true
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
            .or_else(|| {
                let _ = std::fs::remove_file(config_path());
                crate::objects::write_private(&config_path(), body.as_bytes())
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_spec_selects_a_subset() {
        let mut config = Config::default();
        apply_encrypt_spec(&mut config, "objects,sessions");
        assert!(config.encrypt_objects && config.encrypt_sessions);
        assert!(!config.encrypt_caches && !config.encrypt_metadata);
        apply_encrypt_spec(&mut config, "none");
        assert!(!config.encrypt_objects);
        apply_encrypt_spec(&mut config, "all");
        assert!(config.encrypt_objects && config.encrypt_metadata);
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = Config { mode: "micro".to_string(), cold_days: 30, ..Config::default() };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.mode, "micro");
        assert_eq!(back.cold_days, 30);
        assert!(back.encrypt_objects);
    }
}

// ----- human track (ratatui) -----

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph};
use ratatui::{Frame, Terminal};

const WHITE: Color = Color::White;

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
}

struct Phase {
    title: &'static str,
    blurb: &'static str,
    items: &'static [Item],
}

const PHASES: [Phase; 3] = [
    Phase { title: "STORAGE & LIFECYCLE", blurb: "how your code is stored, and when old copies get cleaned up", items: &[Item::Mode, Item::Inactive, Item::Cold] },
    Phase { title: "ENCRYPTION", blurb: "what gets encrypted on disk. leave it on unless you have a reason", items: &[Item::EncObjects, Item::EncCaches, Item::EncMetadata] },
    Phase { title: "SESSIONS & PROTECTION", blurb: "session encryption, and whether changing settings needs a password", items: &[Item::EncSessions, Item::Protect] },
];

fn run_human() -> Result<String, String> {
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;
    let mut config = Config::load();
    let result = wizard(&mut terminal, &mut config);
    disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|e| e.to_string())?;
    terminal.show_cursor().map_err(|e| e.to_string())?;
    result
}

fn wizard<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, config: &mut Config) -> Result<String, String> {
    let mut intro = true;
    let mut phase = 0usize;
    let mut focus = 0usize;
    loop {
        if intro {
            terminal.draw(draw_intro).map_err(|e| e.to_string())?;
        } else {
            terminal.draw(|frame| draw_phase(frame, config, phase, focus)).map_err(|e| e.to_string())?;
        }
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok("setup cancelled; nothing changed\n".to_string()),
            KeyCode::Enter if intro => intro = false,
            KeyCode::Enter => {
                if phase + 1 < PHASES.len() {
                    phase += 1;
                    focus = 0;
                } else {
                    config.save()?;
                    return run_loading(terminal, config);
                }
            }
            KeyCode::Up if !intro => {
                let n = PHASES[phase].items.len();
                focus = (focus + n - 1) % n;
            }
            KeyCode::Down if !intro => {
                let n = PHASES[phase].items.len();
                focus = (focus + 1) % n;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if !intro => {
                let increase = matches!(key.code, KeyCode::Right | KeyCode::Char(' '));
                adjust(config, PHASES[phase].items[focus], increase);
            }
            KeyCode::Backspace if !intro => {
                if phase > 0 {
                    phase -= 1;
                    focus = 0;
                } else {
                    intro = true;
                }
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
    }
}

fn step(value: u64, increase: bool, by: u64, bounds: std::ops::RangeInclusive<u64>) -> u64 {
    let next = if increase { value.saturating_add(by) } else { value.saturating_sub(by) };
    next.clamp(*bounds.start(), *bounds.end())
}

fn frame_block(title: &str) -> Block {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(WHITE))
        .title(Span::styled(format!(" {title} "), Style::default().fg(WHITE).add_modifier(Modifier::BOLD)))
}

fn draw_intro(frame: &mut Frame) {
    let block = frame_block("ISOHYPSE");
    let area = frame.area();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("A coding tool built for agents.", Style::default().fg(WHITE).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("It reads and edits your code by exact lines, so it doesn't overwrite the wrong thing.", Style::default().fg(Color::Gray))).alignment(Alignment::Center),
        Line::from(""),
        Line::from(Span::styled("Agents share one machine, so what it saves is encrypted and the file names don't give away your code.", Style::default().fg(Color::Gray))).alignment(Alignment::Center),
        Line::from(""),
        Line::from(Span::styled("Setup takes a minute. Pick how it stores things and who can change these settings.", Style::default().fg(Color::Gray))).alignment(Alignment::Center),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(" Continue ", Style::default().fg(Color::Black).bg(WHITE).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("enter continue · q cancel", Style::default().fg(Color::DarkGray))).alignment(Alignment::Center),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_phase(frame: &mut Frame, config: &Config, phase: usize, focus: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(1), Constraint::Length(4)])
        .split(frame.area());
    let p = &PHASES[phase];
    let header = Block::default().borders(Borders::ALL).border_style(Style::default().fg(WHITE));
    let hinner = header.inner(chunks[0]);
    frame.render_widget(header, chunks[0]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(format!("isohypse setup — step {} of {}", phase + 1, PHASES.len()), Style::default().fg(WHITE).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
            Line::from(Span::styled(p.blurb, Style::default().fg(Color::Gray))).alignment(Alignment::Center),
        ]),
        hinner,
    );
    let block = frame_block(p.title);
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    let mut lines: Vec<Line> = vec![Line::from(""), Line::from("")];
    for (i, item) in p.items.iter().enumerate() {
        lines.push(item_line(config, *item, i == focus));
        lines.push(Line::from(""));
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    let last = phase + 1 == PHASES.len();
    let action = if last { " Initialize " } else { " Next " };
    let footer = Block::default().borders(Borders::ALL).border_style(Style::default().fg(WHITE));
    let finner = footer.inner(chunks[2]);
    frame.render_widget(footer, chunks[2]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(action, Style::default().fg(Color::Black).bg(WHITE).add_modifier(Modifier::BOLD))).alignment(Alignment::Center),
            Line::from(Span::styled("up/down move · space toggle · left/right adjust · enter next · backspace back · q cancel", Style::default().fg(Color::DarkGray))).alignment(Alignment::Center),
        ]),
        finner,
    );
}

fn item_line<'a>(config: &Config, item: Item, focused: bool) -> Line<'a> {
    let (label, value, off_note): (&str, String, &str) = match item {
        Item::Mode => (
            "storage",
            if config.mode == "global" { "global".to_string() } else { "micro".to_string() },
            if config.mode == "global" { "" } else { "per-repo only, no cross-repo knowledge" },
        ),
        Item::Inactive => ("idle before inactive", format!("{} hours", config.inactive_hours), ""),
        Item::Cold => ("inactive before cold", format!("{} days", config.cold_days), ""),
        Item::EncObjects => ("encrypt objects", toggle(config.encrypt_objects), if config.encrypt_objects { "" } else { "code snapshots stored unencrypted" }),
        Item::EncCaches => ("encrypt caches", toggle(config.encrypt_caches), if config.encrypt_caches { "" } else { "index and vectors stored unencrypted" }),
        Item::EncMetadata => ("encrypt metadata", toggle(config.encrypt_metadata), if config.encrypt_metadata { "" } else { "workspace and journal records unencrypted" }),
        Item::EncSessions => ("encrypt sessions", toggle(config.encrypt_sessions), if config.encrypt_sessions { "" } else { "session tokens stored unencrypted" }),
        Item::Protect => ("password-protect settings", toggle(config.reconfig_locked), if config.reconfig_locked { "" } else { "any program running as you can change these later" }),
    };
    let marker = if focused { "> " } else { "  " };
    let label_style = if focused { Style::default().fg(WHITE).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) };
    let mut spans = vec![
        Span::styled(marker, Style::default().fg(WHITE)),
        Span::styled(format!("{label:<26}"), label_style),
        Span::styled(format!("{value:<8}"), Style::default().fg(WHITE)),
    ];
    if !off_note.is_empty() {
        spans.push(Span::styled(format!("— {off_note}"), Style::default().fg(Color::Yellow)));
    }
    Line::from(spans)
}

fn toggle(on: bool) -> String {
    if on { "on".to_string() } else { "off".to_string() }
}

fn all_encrypted(config: &Config) -> bool {
    config.encrypt_objects && config.encrypt_caches && config.encrypt_metadata && config.encrypt_sessions
}

fn run_loading<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, config: &Config) -> Result<String, String> {
    let steps = [
        "sealing the store",
        "writing the signed manifest",
        "building the reference index",
        "warming the daemon",
        "opening sockets",
    ];
    let total = steps.len();
    for i in 0..total {
        for tick in 0..10 {
            let progressed = i as f64 + (tick as f64 / 10.0);
            let ratio = (progressed / total as f64).clamp(0.0, 1.0);
            terminal.draw(|frame| draw_loading(frame, &steps, i, ratio)).map_err(|e| e.to_string())?;
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    terminal.draw(|frame| draw_loading(frame, &steps, total, 1.0)).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    Ok(format!(
        "isohypse ready — {} store, encryption {}, settings {}\n",
        config.mode,
        if all_encrypted(config) { "on" } else { "partial" },
        if config.reconfig_locked { "password-protected" } else { "open" },
    ))
}

fn draw_loading(frame: &mut Frame, steps: &[&str], current: usize, ratio: f64) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());
    let block = frame_block("BRINGING ISOHYPSE ONLINE");
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);
    let mut lines: Vec<Line> = vec![Line::from("")];
    for (i, step) in steps.iter().enumerate() {
        let (mark, style) = if i < current {
            ("[done] ", Style::default().fg(Color::Green))
        } else if i == current {
            ("[ .. ] ", Style::default().fg(WHITE).add_modifier(Modifier::BOLD))
        } else {
            ("[    ] ", Style::default().fg(Color::DarkGray))
        };
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default().fg(WHITE)),
            Span::styled(mark, style),
            Span::styled(*step, style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    let gauge = Gauge::default()
        .block(frame_block("PROGRESS"))
        .gauge_style(Style::default().fg(WHITE).bg(Color::DarkGray))
        .ratio(ratio);
    frame.render_widget(gauge, chunks[1]);
}
