mod panes;

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::{broadcast, mpsc};
use tui_textarea::{Input, Key, TextArea};

use crate::agent::events::{AgentEvent, AgentStatus, ApprovalTicket};
use crate::agent::supervisor;
use crate::agent::RuntimeDeps;
use crate::policy::ApprovalDecision;
use crate::util::truncate_chars;

struct AgentRow {
    id: String,
    parent_id: Option<String>,
    name: String,
    status: AgentStatus,
    prompt_tokens: u32,
    completion_tokens: u32,
}

struct TraceLine {
    agent_id: String,
    kind: String,
    text: String,
}

enum Focus {
    Agents,
    Inspector,
    Input,
}

enum ModalMode {
    Decide,
    Edit(TextArea<'static>),
    Reject(String),
}

struct UiState {
    session_id: String,
    model: String,
    agents: Vec<AgentRow>,
    selected: usize,
    traces: Vec<TraceLine>,
    scroll: usize,
    focus: Focus,
    input: String,
    pending: VecDeque<ApprovalTicket>,
    modal: ModalMode,
    modal_error: String,
    tick: u64,
}

pub async fn run_with_approvals(
    deps: RuntimeDeps,
    model: String,
    mut approval_rx: mpsc::UnboundedReceiver<ApprovalTicket>,
    mut ui_rx: broadcast::Receiver<AgentEvent>,
) -> anyhow::Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        anyhow::bail!("stdout is not a terminal; use `google-harness run <goal>`");
    }
    let _guard = TermGuard;
    let mut terminal = setup_terminal()?;
    let mut state = UiState {
        session_id: deps.session_id.clone(),
        model,
        agents: Vec::new(),
        selected: 0,
        traces: Vec::new(),
        scroll: 0,
        focus: Focus::Input,
        input: String::new(),
        pending: VecDeque::new(),
        modal: ModalMode::Decide,
        modal_error: String::new(),
        tick: 0,
    };
    state.push_trace(
        "",
        "info",
        format!(
            "Session {}. Type a goal and press Enter. Read tools run on their own. Write tools wait for approval.",
            state.session_id
        ),
    );

    loop {
        loop {
            match ui_rx.try_recv() {
                Ok(event) => state.apply(event),
                Err(broadcast::error::TryRecvError::Empty)
                | Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        while let Ok(ticket) = approval_rx.try_recv() {
            state.pending.push_back(ticket);
        }
        terminal.draw(|frame| draw(frame, &state))?;
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && handle_key(&mut state, key, &deps) {
                    break;
                }
            }
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        state.tick = state.tick.wrapping_add(1);
    }
    deps.cancel.cancel();
    while let Some(ticket) = state.pending.pop_front() {
        let _ = ticket.respond(ApprovalDecision::Reject {
            reason: "session closed".into(),
        });
    }
    Ok(())
}

fn handle_key(state: &mut UiState, key: KeyEvent, deps: &RuntimeDeps) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        return true;
    }
    if !state.pending.is_empty() {
        return handle_modal_key(state, key);
    }
    match key.code {
        KeyCode::Tab => {
            state.focus = match state.focus {
                Focus::Agents => Focus::Inspector,
                Focus::Inspector => Focus::Input,
                Focus::Input => Focus::Agents,
            };
        }
        KeyCode::Up => match state.focus {
            Focus::Agents => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            Focus::Inspector => state.scroll = state.scroll.saturating_add(1),
            Focus::Input => {}
        },
        KeyCode::Down => match state.focus {
            Focus::Agents => {
                if state.selected + 1 < state.agents.len() {
                    state.selected += 1;
                }
            }
            Focus::Inspector => state.scroll = state.scroll.saturating_sub(1),
            Focus::Input => {}
        },
        KeyCode::PageUp => state.scroll = state.scroll.saturating_add(10),
        KeyCode::PageDown => state.scroll = state.scroll.saturating_sub(10),
        KeyCode::Enter if matches!(state.focus, Focus::Input) => submit_goal(state, deps),
        KeyCode::Backspace if matches!(state.focus, Focus::Input) => {
            state.input.pop();
        }
        KeyCode::Char('u') if ctrl && matches!(state.focus, Focus::Input) => state.input.clear(),
        KeyCode::Char(ch) if matches!(state.focus, Focus::Input) => state.input.push(ch),
        KeyCode::Char('q') if !matches!(state.focus, Focus::Input) => return true,
        _ => {}
    }
    false
}

fn handle_modal_key(state: &mut UiState, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        return true;
    }
    let mode = std::mem::replace(&mut state.modal, ModalMode::Decide);
    match mode {
        ModalMode::Edit(mut textarea) => {
            if ctrl && matches!(key.code, KeyCode::Char('s')) {
                let text = textarea.lines().join("\n");
                match serde_json::from_str(&text) {
                    Ok(value) => state.respond(ApprovalDecision::ApproveWithModifiedArgs(value)),
                    Err(err) => {
                        state.modal_error = format!("invalid JSON: {err}");
                        state.modal = ModalMode::Edit(textarea);
                    }
                }
                return false;
            }
            if matches!(key.code, KeyCode::Esc) {
                state.modal_error.clear();
                return false;
            }
            textarea.input(to_textarea_input(key));
            state.modal = ModalMode::Edit(textarea);
            false
        }
        ModalMode::Reject(mut reason) => {
            match key.code {
                KeyCode::Esc => {
                    state.modal_error.clear();
                }
                KeyCode::Enter => {
                    let reason = if reason.trim().is_empty() {
                        "rejected by user".to_string()
                    } else {
                        reason.trim().to_string()
                    };
                    state.respond(ApprovalDecision::Reject { reason });
                }
                KeyCode::Backspace => {
                    reason.pop();
                    state.modal = ModalMode::Reject(reason);
                }
                KeyCode::Char(ch) => {
                    reason.push(ch);
                    state.modal = ModalMode::Reject(reason);
                }
                _ => state.modal = ModalMode::Reject(reason),
            }
            false
        }
        ModalMode::Decide => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => state.respond(ApprovalDecision::Approve),
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    state.modal = ModalMode::Reject(String::new());
                    state.modal_error.clear();
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    if let Some(ticket) = state.pending.front() {
                        let pretty = serde_json::to_string_pretty(&ticket.arguments)
                            .unwrap_or_else(|_| ticket.arguments.to_string());
                        let lines: Vec<String> = if pretty.is_empty() {
                            vec![String::new()]
                        } else {
                            pretty.lines().map(str::to_string).collect()
                        };
                        state.modal = ModalMode::Edit(TextArea::new(lines));
                        state.modal_error.clear();
                    }
                }
                _ => {}
            }
            false
        }
    }
}

fn submit_goal(state: &mut UiState, deps: &RuntimeDeps) {
    let goal = state.input.trim().to_string();
    if goal.is_empty() {
        return;
    }
    state.input.clear();
    state.push_trace("", "info", format!("you: {goal}"));
    if let Err(err) = deps.store.update_goal(&deps.session_id, &goal) {
        state.push_trace("", "error", format!("could not store goal: {err}"));
    }
    let deps = deps.clone();
    supervisor::launch(goal, deps);
}

impl UiState {
    fn respond(&mut self, decision: ApprovalDecision) {
        if let Some(ticket) = self.pending.pop_front() {
            let _ = ticket.respond(decision);
        }
        self.modal_error.clear();
    }

    fn push_trace(&mut self, agent_id: &str, kind: &str, text: impl Into<String>) {
        self.traces.push(TraceLine {
            agent_id: agent_id.to_string(),
            kind: kind.to_string(),
            text: text.into(),
        });
        if self.traces.len() > 2000 {
            let extra = self.traces.len() - 2000;
            self.traces.drain(0..extra);
        }
    }

    fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Spawned {
                agent_id,
                parent_id,
                name,
                role: _,
            } => {
                self.agents.push(AgentRow {
                    id: agent_id.clone(),
                    parent_id,
                    name: name.clone(),
                    status: AgentStatus::Idle,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                });
                if self.agents.len() == 1 {
                    self.selected = 0;
                }
                self.push_trace(&agent_id, "info", format!("{name} started"));
            }
            AgentEvent::StatusChanged { agent_id, status } => {
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
                    agent.status = status;
                }
            }
            AgentEvent::ThinkingDelta { agent_id, delta } => {
                if let Some(last) = self.traces.last_mut() {
                    if last.agent_id == agent_id && last.kind == "think" && last.text.len() < 400 {
                        last.text.push_str(&delta);
                        return;
                    }
                }
                self.push_trace(&agent_id, "think", delta);
            }
            AgentEvent::Thought { .. } => {}
            AgentEvent::ToolCallRequested {
                agent_id,
                tool_name,
                arguments,
                ..
            } => {
                self.push_trace(
                    &agent_id,
                    "tool",
                    format!("{tool_name} {}", truncate_chars(&arguments, 300)),
                );
            }
            AgentEvent::ApprovalRequired(ticket) => {
                self.push_trace(
                    &ticket.agent_id,
                    "info",
                    format!("approval required: {}", ticket.tool_name),
                );
            }
            AgentEvent::ToolCallCompleted {
                agent_id,
                tool_name,
                result_preview,
                latency_ms,
                ..
            } => {
                self.push_trace(
                    &agent_id,
                    "result",
                    format!("{tool_name} ({latency_ms} ms) {result_preview}"),
                );
            }
            AgentEvent::Finished { agent_id, summary } => {
                self.push_trace(&agent_id, "info", format!("finished: {summary}"));
            }
            AgentEvent::Failed { agent_id, error } => {
                self.push_trace(&agent_id, "error", error);
            }
            AgentEvent::TokenUsage {
                agent_id,
                prompt_tokens,
                completion_tokens,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
                    agent.prompt_tokens += prompt_tokens;
                    agent.completion_tokens += completion_tokens;
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, state: &UiState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(outer[0]);
    panes::render_agent_tree(
        frame,
        body[0],
        &state.agents,
        state.selected,
        state.tick,
        matches!(state.focus, Focus::Agents),
    );
    let selected_id = state.agents.get(state.selected).map(|agent| agent.id.as_str());
    panes::render_inspector(
        frame,
        body[1],
        &state.traces,
        selected_id,
        state.scroll,
        matches!(state.focus, Focus::Inspector),
        &state.model,
    );
    let input = Paragraph::new(format!("> {}", state.input)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(if matches!(state.focus, Focus::Input) {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            })
            .title("goal"),
    );
    frame.render_widget(input, outer[1]);
    let help = if state.pending.is_empty() {
        "Tab focus   Enter run   Up/Down select or scroll   y/e/n when a write is waiting   q quit"
    } else {
        "approval waiting: y approve   e edit JSON   n reject with a reason   Ctrl-S saves an edit"
    };
    frame.render_widget(Paragraph::new(help), outer[2]);

    if let Some(ticket) = state.pending.front() {
        let queued = if state.pending.len() > 1 {
            format!(
                "approve {} from {}  (+{} more)",
                ticket.tool_name,
                short_id(&ticket.agent_id),
                state.pending.len() - 1
            )
        } else {
            format!(
                "approve {} from {}",
                ticket.tool_name,
                short_id(&ticket.agent_id)
            )
        };
        let body = match &state.modal {
            ModalMode::Decide => panes::ModalBody::Preview(&ticket.preview),
            ModalMode::Edit(textarea) => panes::ModalBody::Edit(textarea),
            ModalMode::Reject(reason) => panes::ModalBody::Reject(reason),
        };
        panes::render_approval(frame, frame.area(), &queued, body, &state.modal_error);
    }
}

fn to_textarea_input(key: KeyEvent) -> Input {
    let mapped = match key.code {
        KeyCode::Char(ch) => Key::Char(ch),
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Tab => Key::Tab,
        KeyCode::Delete => Key::Delete,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Esc => Key::Esc,
        _ => Key::Null,
    };
    Input {
        key: mapped,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    }
}

fn short_id(id: &str) -> &str {
    let end = id.char_indices().nth(8).map(|(idx, _)| idx).unwrap_or(id.len());
    &id[..end]
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
