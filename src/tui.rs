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

const BG: Color = Color::Rgb(10, 10, 10);
const PANEL: Color = Color::Rgb(20, 20, 20);
const ELEMENT: Color = Color::Rgb(30, 30, 30);
const FG: Color = Color::Rgb(238, 238, 238);
const MUTED: Color = Color::Rgb(128, 128, 128);
const ACCENT: Color = Color::Rgb(250, 178, 131);
const SUCCESS: Color = Color::Rgb(126, 190, 150);
const ERROR: Color = Color::Rgb(224, 126, 126);

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

enum TranscriptEntry {
    User(String),
    Assistant(String),
    Tool {
        call_id: String,
        name: String,
        arguments: Value,
        result: Option<(bool, String, u64)>,
    },
    Status {
        label: String,
        detail: String,
        error: bool,
    },
    Info(String),
}

pub struct App {
    status: Status,
    input: String,
    transcript: Vec<TranscriptEntry>,
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
            transcript: vec![],
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
        match event.kind.as_str() {
            "user" => self.transcript.push(TranscriptEntry::User(clean(
                d["text"].as_str().unwrap_or(""),
            ))),
            "delta" => {
                let text = clean(d["text"].as_str().unwrap_or(""));
                if let Some(TranscriptEntry::Assistant(current)) = self.transcript.last_mut() {
                    current.push_str(&text);
                } else {
                    self.transcript.push(TranscriptEntry::Assistant(text));
                }
            }
            // The final assistant event repeats the already streamed content.
            "assistant" => {
                if !matches!(self.transcript.last(), Some(TranscriptEntry::Assistant(_))) {
                    self.transcript.push(TranscriptEntry::Assistant(clean(
                        d["text"].as_str().unwrap_or(""),
                    )));
                }
            }
            "tool_start" => self.transcript.push(TranscriptEntry::Tool {
                call_id: d["call_id"].as_str().unwrap_or("").into(),
                name: clean(d["tool"].as_str().unwrap_or("tool")),
                arguments: d["arguments"].clone(),
                result: None,
            }),
            "tool_result" => {
                let call_id = d["call_id"].as_str().unwrap_or("");
                let ok = d["output"]["ok"] == true;
                let output = if ok {
                    d["output"]["result"].to_string()
                } else {
                    d["output"]["error"]
                        .as_str()
                        .unwrap_or("Tool failed")
                        .into()
                };
                let elapsed = d["elapsed_ms"].as_u64().unwrap_or(0);
                for entry in self.transcript.iter_mut().rev() {
                    if let TranscriptEntry::Tool {
                        call_id: id,
                        result,
                        ..
                    } = entry
                    {
                        if id == call_id {
                            *result = Some((ok, clean(&output), elapsed));
                            break;
                        }
                    }
                }
            }
            "state" => {
                let state = d["state"].as_str().unwrap_or("");
                if matches!(state, "failed" | "cancelled" | "unknown") {
                    self.transcript.push(TranscriptEntry::Status {
                        label: state.into(),
                        detail: clean(d["error"].as_str().unwrap_or("")),
                        error: true,
                    });
                } else if state == "succeeded" {
                    self.transcript.push(TranscriptEntry::Status {
                        label: "completed".into(),
                        detail: String::new(),
                        error: false,
                    });
                }
            }
            "metrics" => {
                let turns = d["turns"].as_u64().unwrap_or(0);
                let elapsed = d["elapsed_ms"].as_u64().unwrap_or(0);
                self.transcript.push(TranscriptEntry::Status {
                    label: format!("{turns} turn{}", if turns == 1 { "" } else { "s" }),
                    detail: format_duration(elapsed),
                    error: false,
                });
            }
            _ => {}
        }
        // Full events remain in SQLite; keep only the recent renderable entries.
        if self.transcript.len() > 2_000 {
            self.transcript.drain(..400);
        }
    }
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else {
        format!("{:.1}s", milliseconds as f64 / 1_000.0)
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
                    Ok(v) => app.transcript.push(TranscriptEntry::Info(clean(
                        &serde_json::to_string_pretty(&v).unwrap_or_default(),
                    ))),
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
        app.transcript.push(TranscriptEntry::Info(
            "/new                 New session\n/model               Choose a model\n/model <name>        Switch model profile\n/sessions            Session history\n/resume <session-id> Local history\n/devices             Discover devices\n/device <id|local>   Execution target\n/quit                Close UI\n\nShift+Tab  Plan / Build    Ctrl+A  Review approval\nEsc        Cancel task     Alt+Enter  New line"
                .into(),
        ));
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
        app.transcript.push(TranscriptEntry::Info(clean(
            &serde_json::to_string_pretty(&app.devices).unwrap_or_default(),
        )));
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
    let home = app.session.is_none() && app.transcript.is_empty();
    if home {
        draw_home(frame, app, area);
    } else {
        draw_session(frame, app, area);
    }
    if let Some(picker) = &app.model_picker {
        let width = area.width.saturating_sub(4).min(78);
        let desired_height = (picker.catalog.models.len() as u16)
            .saturating_mul(2)
            .saturating_add(8);
        let height = desired_height.min(area.height.saturating_sub(4)).max(8);
        let popup = centered(area, width, height);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(" Select model ")
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
        let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(inner);
        frame.render_widget(
            Paragraph::new(format!(
                "{}\nChanges apply to the next task",
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
                .highlight_style(Style::default().bg(ELEMENT).fg(ACCENT).bold()),
            rows[1],
            &mut selection,
        );
    }
    if app.approval_open {
        if let Some(a) = app.approvals.first() {
            let popup = centered(
                area,
                area.width.saturating_sub(4).min(82),
                area.height.saturating_sub(4).min(24),
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

fn draw_home(frame: &mut Frame, app: &App, area: Rect) {
    let footer = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        area.height.min(1),
    );
    let content_height = 12.min(area.height.saturating_sub(1));
    let content = centered(
        Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)),
        area.width.saturating_sub(4).min(76),
        content_height,
    );
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(content);
    frame.render_widget(
        Paragraph::new(vec![
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
                "Your workspace agent, available across devices",
                Style::default().fg(MUTED),
            ),
        ])
        .alignment(Alignment::Center),
        rows[0],
    );
    draw_composer(frame, app, rows[2]);
    draw_notice(frame, app, rows[3], true);
    let footer_cols = Layout::horizontal([
        Constraint::Percentage(55),
        Constraint::Length(1),
        Constraint::Percentage(45),
    ])
    .split(footer);
    frame.render_widget(
        Paragraph::new(format!(
            "  {}",
            compact_path(
                &app.status.workspace,
                footer_cols[0].width.saturating_sub(2) as usize
            )
        ))
        .style(Style::default().fg(MUTED)),
        footer_cols[0],
    );
    let identity = if area.width < 80 {
        format!("mutlink v{}  ", env!("CARGO_PKG_VERSION"))
    } else {
        format!(
            "mutlink v{}  ·  {}  ",
            env!("CARGO_PKG_VERSION"),
            clean(&app.status.device.name),
        )
    };
    frame.render_widget(
        Paragraph::new(identity)
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED)),
        footer_cols[2],
    );
}

fn draw_session(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .split(area);
    let body = centered_width(rows[0], area.width.saturating_sub(2).min(104));
    let lines = transcript_lines(app);
    let render_width = body.width.saturating_sub(4).max(1) as usize;
    let line_count: usize = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(render_width))
        .sum();
    let offset = line_count
        .saturating_sub(body.height as usize)
        .saturating_sub(app.scroll_back as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0))
            .block(Block::default().padding(ratatui::widgets::Padding::horizontal(2))),
        body,
    );
    let composer = centered_width(rows[2], area.width.saturating_sub(4).min(100));
    draw_composer(frame, app, composer);
    draw_notice(frame, app, centered_width(rows[3], composer.width), false);
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL).fg(FG))
        .padding(ratatui::widgets::Padding::new(2, 2, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let prompt = if app.input.is_empty() {
        "Ask anything, or type /help"
    } else {
        app.input.as_str()
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(if app.input.is_empty() { MUTED } else { FG }))
            .wrap(Wrap { trim: false }),
        rows[0],
    );
    let mode = if app.plan { "PLAN" } else { "BUILD" };
    let model = if app.target.is_some() {
        "remote model".into()
    } else {
        compact_path(&app.status.model, inner.width.saturating_sub(16) as usize)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(mode, Style::default().fg(ACCENT).bold()),
            Span::styled("  ", Style::default()),
            Span::styled(model, Style::default().fg(MUTED)),
        ])),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(if app.target.is_some() {
            "remote"
        } else {
            "local"
        })
        .alignment(Alignment::Right)
        .style(Style::default().fg(MUTED)),
        rows[1],
    );
}

fn draw_notice(frame: &mut Frame, app: &App, area: Rect, home: bool) {
    let (text, color) = if app.approvals.is_empty() {
        let suffix = if home && area.width >= 72 {
            "  ·  enter send"
        } else {
            ""
        };
        (format!("{}{}", clean(&app.notice), suffix), MUTED)
    } else {
        (
            format!(
                "{} approval(s) waiting · Ctrl+A to review",
                app.approvals.len()
            ),
            ACCENT,
        )
    };
    frame.render_widget(
        Paragraph::new(text)
            .alignment(if home {
                Alignment::Center
            } else {
                Alignment::Left
            })
            .style(Style::default().fg(color)),
        area,
    );
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &app.transcript {
        match entry {
            TranscriptEntry::User(text) => {
                lines.push(Line::from(""));
                for text_line in text.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("┃", Style::default().fg(ACCENT)),
                        Span::styled(
                            format!(" {text_line} "),
                            Style::default().bg(ELEMENT).fg(FG).bold(),
                        ),
                    ]));
                }
                lines.push(Line::from(""));
            }
            TranscriptEntry::Assistant(text) => {
                for text_line in text.lines() {
                    lines.push(Line::styled(
                        format!("  {text_line}"),
                        Style::default().fg(FG),
                    ));
                }
            }
            TranscriptEntry::Tool {
                name,
                arguments,
                result,
                ..
            } => {
                let (mark, color) = match result {
                    Some((true, _, _)) => ("✓", SUCCESS),
                    Some((false, _, _)) => ("×", ERROR),
                    None => ("›", ACCENT),
                };
                let elapsed = result
                    .as_ref()
                    .map(|(_, _, elapsed)| format!(" · {}", format_duration(*elapsed)))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("  {mark} "), Style::default().fg(color)),
                    Span::styled(name.clone(), Style::default().fg(FG)),
                    Span::styled(
                        format!("  {}{elapsed}", summarize_arguments(arguments)),
                        Style::default().fg(MUTED),
                    ),
                ]));
                if let Some((false, output, _)) = result {
                    lines.push(Line::styled(
                        format!("      {}", compact_path(output, 180)),
                        Style::default().fg(ERROR),
                    ));
                }
            }
            TranscriptEntry::Status {
                label,
                detail,
                error,
            } => {
                let separator = if detail.is_empty() { "" } else { " · " };
                lines.push(Line::styled(
                    format!("  ─ {label}{separator}{detail}"),
                    Style::default().fg(if *error { ERROR } else { MUTED }),
                ));
            }
            TranscriptEntry::Info(text) => {
                lines.push(Line::from(""));
                for text_line in text.lines() {
                    lines.push(Line::styled(
                        format!("  {text_line}"),
                        Style::default().fg(MUTED),
                    ));
                }
                lines.push(Line::from(""));
            }
        }
    }
    lines
}

fn summarize_arguments(arguments: &Value) -> String {
    for key in ["path", "command", "query", "pattern", "task", "agent"] {
        if let Some(value) = arguments.get(key).and_then(Value::as_str) {
            return compact_path(value, 72);
        }
    }
    let value = arguments.to_string();
    if value == "{}" {
        String::new()
    } else {
        compact_path(&value, 72)
    }
}

fn compact_path(value: &str, max: usize) -> String {
    let value = clean(value);
    if unicode_width::UnicodeWidthStr::width(value.as_str()) <= max {
        return value;
    }
    let keep = max.saturating_sub(1);
    let suffix: String = value
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{suffix}")
}

fn centered_width(area: Rect, width: u16) -> Rect {
    let width = width.min(area.width);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y,
        width,
        area.height,
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_content(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn event(seq: i64, kind: &str, data: Value) -> Event {
        Event {
            seq,
            task_id: "task".into(),
            session_id: "session".into(),
            kind: kind.into(),
            data,
            created_at: now(),
        }
    }

    #[test]
    fn renders_home_session_and_dialogs_responsively() {
        let root = tempfile::tempdir().unwrap();
        let agent = crate::agent::Agent::open(root.path().to_path_buf()).unwrap();
        for (w, h) in [(60, 20), (100, 30), (140, 40)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let mut app = App::new(agent.status());
            terminal.draw(|f| draw(f, &app)).unwrap();
            let content = screen_content(&terminal);
            assert!(content.contains("mutlink"));
            assert!(content.contains("Ask anything"));
            assert!(content.contains("BUILD"));
            assert!(!content.contains("CONTEXT"));

            app.event(event(1, "user", json!({"text":"Inspect the workspace"})));
            app.event(event(2, "delta", json!({"text":"I will inspect it."})));
            app.event(event(
                3,
                "tool_start",
                json!({"tool":"files.read","arguments":{"path":"README.md"},"call_id":"call"}),
            ));
            app.event(event(
                4,
                "tool_result",
                json!({"call_id":"call","output":{"ok":true,"result":"done"},"elapsed_ms":12}),
            ));
            app.event(event(5, "state", json!({"state":"succeeded"})));
            terminal.draw(|f| draw(f, &app)).unwrap();
            let content = screen_content(&terminal);
            assert!(content.contains("Inspect the workspace"));
            assert!(content.contains("I will inspect it."));
            assert!(content.contains("files.read"));
            assert!(content.contains("completed"));
            assert!(!content.contains("YOU"));
            assert!(!content.contains("MUTLINK"));
            assert!(!content.contains("CONTEXT"));

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
            let content = screen_content(&terminal);
            assert!(content.contains("Review operation"));
            app.approval_open = false;
            app.model_picker = Some(ModelPicker {
                catalog: crate::models::execute(&agent, "model.list", &Value::Null).unwrap(),
                selected: 0,
            });
            terminal.draw(|f| draw(f, &app)).unwrap();
            let content = screen_content(&terminal);
            assert!(content.contains("Select model"));
            assert!(content.contains("default"));
            assert!(content.contains("enter switch"));
        }
    }
}
