//! Logbook tab: recent events, newest first.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Row, Table};

use super::theme;
use crate::app::App;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let lb = &app.logbook;
    let scope = match &lb.entity {
        Some(id) => app
            .store
            .get(id)
            .map(|e| e.friendly_name().to_string())
            .unwrap_or_else(|| id.clone()),
        None => "all entities".into(),
    };
    let status = if lb.loading { " · loading…" } else { "" };
    let title = format!(
        " Logbook · {scope} · last 24h ({}){status} ",
        lb.entries.len()
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Span::styled(title, theme::title()));

    if let Some(err) = &lb.error {
        let p = Paragraph::new(Line::styled(
            format!("Could not load logbook: {err}"),
            Style::new().fg(theme::ERROR),
        ))
        .block(block)
        .centered();
        f.render_widget(p, area);
        return;
    }

    let rows: Vec<Row> = lb
        .entries
        .iter()
        .map(|e| {
            let name = e
                .name
                .clone()
                .or_else(|| {
                    e.entity_id
                        .as_ref()
                        .and_then(|id| app.store.get(id))
                        .map(|s| s.friendly_name().to_string())
                })
                .or_else(|| e.entity_id.clone())
                .unwrap_or_default();
            let color = e
                .state
                .as_deref()
                .map(theme::state_color)
                .unwrap_or(ratatui::style::Color::White);
            Row::new(vec![
                Line::styled(theme::clock(e.when), theme::dim()),
                Line::from(name),
                Line::styled(e.describe(), Style::new().fg(color)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Fill(1),
            Constraint::Fill(2),
        ],
    )
    .header(Row::new(["Time", "Name", "Event"]).style(theme::dim().bold()))
    .block(block)
    .row_highlight_style(theme::selected())
    .column_spacing(2);
    f.render_stateful_widget(table, area, &mut app.logbook.state);
}
