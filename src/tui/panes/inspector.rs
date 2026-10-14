use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::super::TraceLine;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    lines: &[TraceLine],
    selected_agent: Option<&str>,
    scroll: usize,
    focused: bool,
    model: &str,
) {
    let filtered: Vec<&TraceLine> = lines
        .iter()
        .filter(|line| {
            line.agent_id.is_empty()
                || selected_agent.map(|id| line.agent_id == id).unwrap_or(true)
        })
        .collect();
    let height = area.height.saturating_sub(2) as usize;
    let start = filtered.len().saturating_sub(height.saturating_add(scroll));
    let end = filtered.len().saturating_sub(scroll);
    let visible: Vec<Line> = filtered
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|line| Line::styled(line.text.clone(), kind_style(&line.kind)))
        .collect();
    let title = if scroll == 0 {
        format!("trace  {model}")
    } else {
        format!("trace  {model}  scroll {scroll}")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(title);
    frame.render_widget(
        Paragraph::new(visible)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn kind_style(kind: &str) -> Style {
    let color = match kind {
        "error" => Color::Red,
        "tool" => Color::Magenta,
        "result" => Color::Green,
        "info" => Color::Yellow,
        _ => Color::White,
    };
    Style::default().fg(color)
}
