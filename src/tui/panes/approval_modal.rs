use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tui_textarea::TextArea;

pub enum ModalBody<'a> {
    Preview(&'a str),
    Edit(&'a TextArea<'static>),
    Reject(&'a str),
}

pub fn render(frame: &mut Frame, area: Rect, title: &str, body: ModalBody<'_>, error: &str) {
    let popup = centered(area, 80, 70);
    frame.render_widget(Clear, popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(popup);
    let footer = if error.is_empty() {
        "y approve   e edit   n reject   Ctrl-S save edit   Esc back".to_string()
    } else {
        error.to_string()
    };
    match body {
        ModalBody::Preview(preview) => {
            frame.render_widget(
                Paragraph::new(preview)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(title),
                    )
                    .wrap(Wrap { trim: false }),
                chunks[0],
            );
        }
        ModalBody::Edit(textarea) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(format!("{title}  edit JSON"));
            let inner = block.inner(chunks[0]);
            frame.render_widget(block, chunks[0]);
            frame.render_widget(textarea, inner);
        }
        ModalBody::Reject(reason) => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Reject this write and send the reason back to the agent."),
                    Line::from(""),
                    Line::from(format!("reason: {reason}_")),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(title),
                )
                .wrap(Wrap { trim: false }),
                chunks[0],
            );
        }
    }
    frame.render_widget(
        Paragraph::new(footer).block(
            Block::default()
                .borders(Borders::ALL)
                .title("decision"),
        ),
        chunks[1],
    );
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}
