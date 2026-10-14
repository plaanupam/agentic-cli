use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::super::AgentRow;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    agents: &[AgentRow],
    selected: usize,
    tick: u64,
    focused: bool,
) {
    let spinner = ["|", "/", "-", "\\"][(tick / 2) as usize % 4];
    let lines: Vec<Line> = if agents.is_empty() {
        vec![Line::from("No agents yet.")]
    } else {
        agents
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                let marker = if index == selected { ">" } else { " " };
                let indent = if agent.parent_id.is_some() { "  " } else { "" };
                let activity = match &agent.status {
                    crate::agent::events::AgentStatus::Thinking
                    | crate::agent::events::AgentStatus::CallingTool { .. } => spinner,
                    crate::agent::events::AgentStatus::WaitingForHuman => "!",
                    crate::agent::events::AgentStatus::Completed => "+",
                    crate::agent::events::AgentStatus::Failed => "x",
                    crate::agent::events::AgentStatus::Idle => " ",
                };
                let style = status_style(&agent.status, index == selected);
                Line::from(vec![
                    Span::styled(
                        format!("{marker}{indent}{activity} {} ", agent.name),
                        style,
                    ),
                    Span::styled(
                        format!(
                            "{}  p{} c{}",
                            agent.status.label(),
                            agent.prompt_tokens,
                            agent.completion_tokens
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused))
        .title("agents");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn status_style(status: &crate::agent::events::AgentStatus, selected: bool) -> Style {
    let color = match status {
        crate::agent::events::AgentStatus::WaitingForHuman => Color::Yellow,
        crate::agent::events::AgentStatus::Failed => Color::Red,
        crate::agent::events::AgentStatus::Completed => Color::Green,
        crate::agent::events::AgentStatus::Thinking
        | crate::agent::events::AgentStatus::CallingTool { .. } => Color::Cyan,
        crate::agent::events::AgentStatus::Idle => Color::White,
    };
    let mut style = Style::default().fg(color);
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    }
}
