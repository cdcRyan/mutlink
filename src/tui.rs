use crate::{client::Client, model::*, models::ModelList};
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as Input, EventStream, KeyCode,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{json, Value};
use std::{io, time::Duration};
use tokio::sync::mpsc;

const BG: Color = Color::Rgb(20, 20, 20);
const PANEL: Color = Color::Rgb(28, 28, 28);
const FG: Color = Color::Rgb(222, 221, 215);
const MUTED: Color = Color::Rgb(145, 144, 139);
const ACCENT: Color = Color::Rgb(221, 177, 124);

struct ScreenGuard;
impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}

enum Update {
    Submitted(Result<Task>),
    Poll(u64, Result<(Vec<Event>, Option<Task>, Vec<Approval>)>),
    History(Result<Value>),
    Notice(Result<Value>),
    Devices(Result<Value>),
    Models(Result<ModelList>, bool),
}

struct ModelPicker {
    catalog: ModelList,
    selected: usize,
}

pub struct App {
    status: Status,
    input: String,
    transcript: String,
    task: Option<Task>,
    session: Option<String>,
    after: i64,
    plan: bool,
    target: Option<String>,
    approvals: Vec<Approval>,
    approval_open: bool,
    approval_scroll: u16,
    scroll_back: u16,
    notice: String,
    devices: Value,
    busy: bool,
    polling: bool,
    generation: u64,
    model_picker: Option<ModelPicker>,
    model_pending: bool,
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

impl App {
    fn new(status: Status) -> Self {
        let notice = if status.provider_ready {
            "Ready".into()
        } else {
            "Type /model to choose a model or find models.json".into()
        };
        Self {
            status,
            input: String::new(),
            transcript: String::new(),
            task: None,
            session: None,
            after: 0,
            plan: false,
            target: None,
            approvals: vec![],
            approval_open: false,
            approval_scroll: 0,
            scroll_back: 0,
            notice,
            devices: Value::Null,
            busy: false,
            polling: false,
            generation: 0,
            model_picker: None,
            model_pending: false,
        }
    }
    fn reset(&mut self) {
        self.generation += 1;
        self.task = None;
        self.session = None;
        self.transcript.clear();
        self.after = 0;
        self.scroll_back = 0;
    }
    fn event(&mut self, event: Event) {
        self.after = self.after.max(event.seq);
        let d = event.data;
        let text = match event.kind.as_str() {
            "user" => format!(
                "\n  YOU\n  {}\n\n  MUTLINK\n  ",
                d["text"].as_str().unwrap_or("")
            ),
            "delta" => d["text"].as_str().unwrap_or("").into(),
            "tool_start" => format!(
                "\n\n  › {}  {}\n",
                d["tool"].as_str().unwrap_or("tool"),
                d["arguments"]
            ),
            "tool_result" => format!(
                "  {} {}\n",
                if d["output"]["ok"] == true {
                    "✓"
                } else {
                    "×"
                },
                d["output"]
            ),
            "state" => {
                let state = d["state"].as_str().unwrap_or("");
                if matches!(state, "failed" | "cancelled" | "unknown") {
                    format!("\n\n  {state}: {}\n", d["error"].as_str().unwrap_or(""))
                } else if state == "succeeded" {
                    "\n\n  ─ completed ─\n".into()
                } else {
                    String::new()
                }
            }
            "metrics" => format!("\n  {} turns · {} ms", d["turns"], d["elapsed_ms"]),
            _ => String::new(),
        };
        self.transcript.push_str(&clean(&text));
        // Keep rendering bounded. Full events remain in SQLite.
        if self.transcript.len() > 200_000 {
            let mut boundary = self.transcript.len() - 160_000;
            while !self.transcript.is_char_boundary(boundary) {
                boundary += 1;
            }
            self.transcript.drain(..boundary);
        }
    }
}

pub async fn run(client: Client) -> Result<()> {
    // A returning terminal picks up edits even if the daemon was already running.
    client.call("model.list", Value::Null, None).await?;
    let status: Status = serde_json::from_value(client.call("status", Value::Null, None).await?)?;
    enable_raw_mode()?;
    let _guard = ScreenGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new(status);
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(350));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut ticks = 0usize;
    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        tokio::select! {
            Some(result) = input.next() => {
                match result? {
                    Input::Paste(text) if !app.approval_open && app.model_picker.is_none() => {
                        if app.input.len()+text.len() <= 65536 { app.input.push_str(&clean(&text)); }
                    }
                    Input::Key(key) if key.kind == KeyEventKind::Press => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl && key.code == KeyCode::Char('c') { break; }
                        if let Some(picker) = app.model_picker.as_mut() {
                            match key.code {
                                KeyCode::Esc => app.model_picker = None,
                                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                                KeyCode::Down => picker.selected = (picker.selected + 1).min(picker.catalog.models.len().saturating_sub(1)),
                                KeyCode::Enter if !app.model_pending => {
                                    if let Some(model) = picker.catalog.models.get(picker.selected) {
                                        app.model_pending = true;
                                        spawn_model(&client, &tx, Some(model.name.clone()));
                                    }
                                }
                                _ => {},
                            }
                            continue;
                        }
                        if app.approval_open {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Char('n') => {
                                    if let Some(a) = app.approvals.first() {
                                        spawn_call(&client, &tx, "approve", json!({"id":a.id,"allow":key.code==KeyCode::Char('y')}), None);
                                    }
                                    app.approval_open = false;
                                }
                                KeyCode::Esc => app.approval_open = false,
                                KeyCode::Down | KeyCode::PageDown => app.approval_scroll = app.approval_scroll.saturating_add(5),
                                KeyCode::Up | KeyCode::PageUp => app.approval_scroll = app.approval_scroll.saturating_sub(5),
                                _ => {},
                            }
                            continue;
                        }
                        match key.code {
                            KeyCode::Char('a') if ctrl => { app.approval_open = !app.approvals.is_empty(); app.approval_scroll = 0; }
                            KeyCode::Char('n') if ctrl && !app.busy => app.reset(),
                            KeyCode::BackTab => app.plan = !app.plan,
                            KeyCode::Esc => {
                                if let Some(task) = &app.task {
                                    spawn_call(&client, &tx, "cancel", json!({"task_id":task.id}), app.target.clone());
                                }
                            }
                            KeyCode::PageUp => app.scroll_back = app.scroll_back.saturating_add(10),
                            KeyCode::PageDown => app.scroll_back = app.scroll_back.saturating_sub(10),
                            KeyCode::Backspace => { app.input.pop(); }
                            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => app.input.push('\n'),
                            KeyCode::Enter if !app.input.trim().is_empty() && !app.model_pending
                                && (!app.busy || app.input.trim() == "/model" || app.input.trim().starts_with("/model ")) => {
                                let prompt = std::mem::take(&mut app.input);
                                if prompt == "/quit" { break; }
                                if prompt.starts_with('/') {
                                    command(&mut app, &client, &tx, &prompt);
                                } else {
                                    app.busy = true; app.notice = "Submitting…".into();
                                    let request = RunRequest { id: id(), session_id: app.session.clone(), prompt, plan: app.plan, delegation_depth: 0 };
                                    let c = client.clone(); let tx = tx.clone(); let target = app.target.clone();
                                    tokio::spawn(async move {
                                        let result = async { Ok(serde_json::from_value(c.call("run", json!(request), target.as_deref()).await?)?) }.await;
                                        let _ = tx.send(Update::Submitted(result));
                                    });
                                }
                            }
                            KeyCode::Char(ch) if !ctrl && app.input.len()<65536 => app.input.push(ch),
                            _ => {},
                        }
                    }
                    _ => {},
                }
            }
            _ = ticker.tick() => {
                ticks += 1;
                if !app.polling {
                    app.polling = true;
                    let c = client.clone(); let tx = tx.clone();
                    let after = app.after; let session = app.session.clone();
                    let generation = app.generation;
                    let task = app.task.clone(); let target = app.target.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let approvals = serde_json::from_value(c.call("approvals", Value::Null, None).await?)?;
                            let events = if session.is_some() {
                                serde_json::from_value(c.call("events", json!({"session_id":session,"task_id":task.as_ref().map(|t| &t.id),"after":after}), target.as_deref()).await?)?
                            } else { vec![] };
                            let task = if let Some(t) = task {
                                Some(serde_json::from_value(c.call("task", json!({"task_id":t.id}), target.as_deref()).await?)?)
                            } else { None };
                            Ok((events, task, approvals))
                        }.await;
                        let _ = tx.send(Update::Poll(generation, result));
                    });
                }
                if ticks % 30 == 1 {
                    let c = client.clone(); let tx = tx.clone();
                    tokio::spawn(async move { let _ = tx.send(Update::Devices(c.call("devices", Value::Null, None).await)); });
                }
            }
            Some(update) = rx.recv() => match update {
                Update::Submitted(Ok(task)) => {
                    app.session = Some(task.session_id.clone()); app.task = Some(task);
                    app.notice = "Working · Esc to cancel".into();
                }
                Update::Submitted(Err(e)) => { app.busy=false; app.notice=e.to_string(); }
                Update::Poll(generation, result) => {
                    app.polling = false;
                    if generation != app.generation { continue; }
                    match result {
                        Ok((events, task, approvals)) => {
                            for event in events { app.event(event); }
                            if let Some(task) = task {
                                app.busy = matches!(task.state.as_str(), "queued" | "running" | "waiting_approval");
                                app.notice = format!("{} · {}", task.state, task.id);
                                app.task = Some(task);
                            }
                            app.approvals=approvals;
                        }
                        Err(e) => app.notice=e.to_string(),
                    }
                }
                Update::Devices(Ok(value)) => app.devices=value,
                Update::Devices(Err(e)) => app.notice=format!("Discovery: {e}"),
                Update::Models(result, open) => {
                    app.model_pending = false;
                    match result {
                        Ok(catalog) => {
                            let selected = catalog.models.iter().position(|m| m.name == catalog.selected).unwrap_or(0);
                            if let Some(model) = catalog.models.get(selected) {
                                app.status.model = model.model.clone();
                                app.status.provider_ready = model.ready;
                                app.notice = if model.ready {
                                    format!("Model: {} · applies to your next task", model.name)
                                } else {
                                    format!("Model: {} · set {} before starting the daemon", model.name, model.api_key_env)
                                };
                            }
                            app.model_picker = if open { Some(ModelPicker { catalog, selected }) } else { None };
                        }
                        Err(error) => app.notice = format!("{error:#}"),
                    }
                }
                Update::History(result) => match result {
                    Ok(v) => app.transcript.push_str(&format!("\n{}\n",clean(&serde_json::to_string_pretty(&v).unwrap_or_default()))),
                    Err(e) => app.notice=e.to_string(),
                },
                Update::Notice(result) => app.notice=match result { Ok(v)=>v.to_string(), Err(e)=>e.to_string() },
            }
        }
    }
    Ok(())
}

fn spawn_call(
    client: &Client,
    tx: &mpsc::UnboundedSender<Update>,
    action: &str,
    payload: Value,
    target: Option<String>,
) {
    let c = client.clone();
    let tx = tx.clone();
    let action = action.to_string();
    tokio::spawn(async move {
        let _ = tx.send(Update::Notice(
            c.call(&action, payload, target.as_deref()).await,
        ));
    });
}

fn command(app: &mut App, client: &Client, tx: &mpsc::UnboundedSender<Update>, text: &str) {
    let text = text.trim();
    if text == "/new" {
        app.reset();
    } else if text == "/help" {
        app.transcript.push_str("\n  /new  New session\n  /model  Choose a model (reload models.json)\n  /model <name>  Switch model profile\n  /sessions  Session history\n  /resume <session-id>  Local history\n  /devices  Discover devices\n  /device <full-id|local>  Execution target\n  /quit  Close UI (agent stays online)\n\n  Shift+Tab  Plan / Build\n  Ctrl+A  Review pending approval\n  Esc  Cancel task\n  PageUp / PageDown  Scroll\n  Alt+Enter  New line\n");
    } else if text == "/model" || text.starts_with("/model ") {
        if app.target.is_some() {
            app.notice =
                "Choose models on the execution device; /device local switches back here.".into();
        } else {
            app.model_pending = true;
            app.notice = "Loading models…".into();
            let name = text.strip_prefix("/model ").map(|s| s.trim().to_owned());
            spawn_model(client, tx, name);
        }
    } else if text == "/sessions" {
        let c = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Update::History(c.call("sessions", Value::Null, None).await));
        });
    } else if text == "/devices" {
        app.transcript.push_str(&format!(
            "\n{}\n",
            clean(&serde_json::to_string_pretty(&app.devices).unwrap_or_default())
        ));
    } else if let Some(id) = text.strip_prefix("/resume ") {
        if app.target.is_some() {
            app.notice =
                "Resume history locally; remote tasks can be followed with `mutlink events`".into();
        } else {
            app.reset();
            app.session = Some(id.trim().into());
        }
    } else if let Some(id) = text.strip_prefix("/device ") {
        let id = id.trim();
        if id == "local" || app.devices["trusted"].get(id).is_some() {
            app.reset();
            app.target = if id == "local" { None } else { Some(id.into()) };
        } else {
            app.notice = "Unknown device. Pair it first with `mutlink pair trust`.".into();
        }
    } else {
        app.notice = "Unknown command · /help".into();
    }
}

fn spawn_model(client: &Client, tx: &mpsc::UnboundedSender<Update>, name: Option<String>) {
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let open = name.is_none();
        let (action, payload) = match name {
            Some(name) => ("model.use", json!({"name":name})),
            None => ("model.list", Value::Null),
        };
        let result = async {
            Ok(serde_json::from_value(
                client.call(action, payload, None).await?,
            )?)
        }
        .await;
        let _ = tx.send(Update::Models(result, open));
    });
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  mutlink", Style::default().fg(FG).bold()),
            Span::styled("   /   ", Style::default().fg(MUTED)),
            Span::raw(if app.session.is_some() {
                "session"
            } else {
                "new session"
            }),
        ])),
        rows[0],
    );
    let cols = Layout::horizontal(if area.width >= 100 {
        vec![Constraint::Min(40), Constraint::Length(30)]
    } else {
        vec![Constraint::Percentage(100)]
    })
    .split(rows[1]);
    if app.transcript.is_empty() {
        let center = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(11),
            Constraint::Min(0),
        ])
        .split(cols[0])[1];
        let logo = vec![
            Line::from(""),
            Line::styled(
                "█▀▄▀█ █ █ ▀█▀ █   █ █▄ █ █▄▀",
                Style::default().fg(FG).bold(),
            ),
            Line::styled(
                "█ ▀ █ █▄█  █  █▄▄ █ █ ▀█ █ █",
                Style::default().fg(FG).bold(),
            ),
            Line::from(""),
            Line::styled(
                "Your agent. Across your devices.",
                Style::default().fg(MUTED),
            ),
            Line::from(""),
            Line::styled(
                "Ask anything about this workspace",
                Style::default().fg(ACCENT),
            ),
            Line::from(""),
            Line::styled(
                "/model   models    /help   commands",
                Style::default().fg(MUTED),
            ),
        ];
        frame.render_widget(Paragraph::new(logo).alignment(Alignment::Center), center);
    } else {
        let width = cols[0].width.saturating_sub(4).max(1) as usize;
        let line_count: usize = app
            .transcript
            .lines()
            .map(|l| {
                unicode_width::UnicodeWidthStr::width(l)
                    .max(1)
                    .div_ceil(width)
            })
            .sum();
        let offset = line_count
            .saturating_sub(cols[0].height as usize)
            .saturating_sub(app.scroll_back as usize)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(
            Paragraph::new(app.transcript.as_str())
                .wrap(Wrap { trim: false })
                .scroll((offset, 0))
                .block(Block::default().padding(ratatui::widgets::Padding::horizontal(2))),
            cols[0],
        );
    }
    if cols.len() > 1 {
        let target = app
            .target
            .as_deref()
            .and_then(|id| app.devices["trusted"][id]["device"]["name"].as_str())
            .unwrap_or("This computer");
        let mut lines = vec![
            Line::styled("CONTEXT", Style::default().fg(MUTED)),
            Line::from(""),
            Line::from(clean(&app.status.workspace)),
            Line::from(""),
            Line::styled("MODEL", Style::default().fg(MUTED)),
            Line::from(if app.target.is_some() {
                "Managed on remote device".into()
            } else {
                clean(&app.status.model)
            }),
            Line::from(""),
            Line::styled("EXECUTION", Style::default().fg(MUTED)),
            Line::styled(clean(target), Style::default().fg(ACCENT)),
            Line::from(""),
            Line::styled("DEVICES", Style::default().fg(MUTED)),
            Line::from(format!("● {} · local", clean(&app.status.device.name))),
        ];
        if let Some(devices) = app.devices["discovered"].as_array() {
            for d in devices
                .iter()
                .filter(|d| d["device"]["id"] != app.status.device.id)
            {
                lines.push(Line::from(format!(
                    "{} {}",
                    if d["online"] == true { "●" } else { "○" },
                    clean(d["device"]["name"].as_str().unwrap_or("device"))
                )));
            }
        }
        lines.extend([
            Line::from(""),
            Line::styled("/devices to inspect", Style::default().fg(MUTED)),
        ]);
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(PANEL))
                    .padding(ratatui::widgets::Padding::new(2, 1, 1, 0)),
            ),
            cols[1],
        );
    }
    let mode = if app.plan { "PLAN" } else { "BUILD" };
    let title = format!(
        "  {mode}  ·  {}  ·  {}  ",
        if app.target.is_some() {
            "remote"
        } else {
            "local"
        },
        if app.target.is_some() {
            "remote model".into()
        } else {
            clean(&app.status.model)
        }
    );
    let composer = Paragraph::new(if app.input.is_empty() {
        " Ask mutlink…"
    } else {
        app.input.as_str()
    })
    .style(
        Style::default()
            .bg(PANEL)
            .fg(if app.input.is_empty() { MUTED } else { FG }),
    )
    .block(
        Block::default()
            .title(Line::styled(title, Style::default().fg(ACCENT)))
            .borders(Borders::TOP)
            .border_style(Style::default().fg(ACCENT))
            .padding(ratatui::widgets::Padding::new(2, 2, 1, 0)),
    )
    .wrap(Wrap { trim: false });
    frame.render_widget(composer, rows[2]);
    let hint = if app.approvals.is_empty() {
        format!("  {}   ·   enter send  /help", clean(&app.notice))
    } else {
        format!(
            "  {} approval(s) waiting · Ctrl+A to review on this device",
            app.approvals.len()
        )
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(if app.approvals.is_empty() {
            MUTED
        } else {
            ACCENT
        })),
        rows[3],
    );
    if let Some(picker) = &app.model_picker {
        let popup = Rect::new(
            area.x + area.width / 20,
            area.y + area.height / 8,
            area.width * 9 / 10,
            area.height * 3 / 4,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(" Models · this computer ")
            .title_bottom(if app.model_pending {
                " Switching… "
            } else {
                " ↑↓ choose · enter switch · esc close "
            })
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL).fg(FG))
            .padding(ratatui::widgets::Padding::uniform(1));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let rows = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).split(inner);
        frame.render_widget(
            Paragraph::new(format!(
                "Edit models.json, then reopen /model:\n{}",
                clean(&picker.catalog.path)
            ))
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: false }),
            rows[0],
        );
        let items: Vec<_> = picker
            .catalog
            .models
            .iter()
            .map(|model| {
                ListItem::new(vec![
                    Line::from(format!(
                        "{}{} · {}",
                        if model.name == picker.catalog.selected {
                            "● "
                        } else {
                            "  "
                        },
                        clean(&model.name),
                        clean(&model.model)
                    )),
                    Line::styled(
                        if model.ready {
                            clean(&model.base_url)
                        } else {
                            format!("Set {} to use this model", clean(&model.api_key_env))
                        },
                        Style::default().fg(MUTED),
                    ),
                ])
            })
            .collect();
        let mut selection = ListState::default().with_selected(Some(picker.selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(BG).fg(ACCENT)),
            rows[1],
            &mut selection,
        );
    }
    if app.approval_open {
        if let Some(a) = app.approvals.first() {
            let popup = Rect::new(
                area.x + area.width / 10,
                area.y + area.height / 8,
                area.width * 8 / 10,
                area.height * 3 / 4,
            );
            frame.render_widget(Clear, popup);
            let body=format!("Task: {}\nTool: {}\n\n{}\n\nThis operation runs on this computer.\nShell has your OS user permissions.",a.task_id,a.tool,serde_json::to_string_pretty(&a.arguments).unwrap_or_default());
            frame.render_widget(
                Paragraph::new(clean(&body))
                    .wrap(Wrap { trim: false })
                    .scroll((app.approval_scroll, 0))
                    .block(
                        Block::default()
                            .title(" Review operation ")
                            .title_bottom(" y approve · n deny · ↑↓ scroll · esc close ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT))
                            .padding(ratatui::widgets::Padding::uniform(1)),
                    )
                    .style(Style::default().bg(PANEL).fg(FG)),
                popup,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_home_and_approval_at_small_and_large_sizes() {
        let root = tempfile::tempdir().unwrap();
        let agent = crate::agent::Agent::open(root.path().to_path_buf()).unwrap();
        for (w, h) in [(120, 36), (60, 20)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let mut app = App::new(agent.status());
            terminal.draw(|f| draw(f, &app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(content.contains("mutlink"));
            assert!(content.contains("BUILD"));
            app.approvals.push(Approval {
                id: "a".into(),
                task_id: "t".into(),
                tool: "run_shell".into(),
                arguments: json!({"command":"pwd"}),
                state: "pending".into(),
                created_at: now(),
                expires_at: now() + 300,
            });
            app.approval_open = true;
            terminal.draw(|f| draw(f, &app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(content.contains("Review operation"));
            app.approval_open = false;
            app.model_picker = Some(ModelPicker {
                catalog: crate::models::execute(&agent, "model.list", &Value::Null).unwrap(),
                selected: 0,
            });
            terminal.draw(|f| draw(f, &app)).unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.contains("Models"));
            assert!(content.contains("default"));
            assert!(content.contains("enter switch"));
        }
    }
}
