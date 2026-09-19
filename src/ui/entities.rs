//! Entities / Scenes & Scripts / Automations tabs: optional group sidebar, entity table, detail pane.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, Paragraph, Row, Table, Wrap};
use serde_json::Value;

use super::theme;
use crate::app::{App, Focus, Tab};
use crate::ha::actions;
use crate::ha::types::EntityState;

const SIDEBAR_W: u16 = 24;
const DETAIL_W: u16 = 44;
const DETAIL_MIN_TOTAL: u16 = 110;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let show_sidebar = app.tab == Tab::Entities;
    let show_detail = area.width >= DETAIL_MIN_TOTAL;
    let [side, table, detail] = Layout::horizontal([
        Constraint::Length(if show_sidebar { SIDEBAR_W } else { 0 }),
        Constraint::Fill(1),
        Constraint::Length(if show_detail { DETAIL_W } else { 0 }),
    ])
    .areas(area);

    if show_sidebar {
        render_groups(f, app, side);
    }
    let selected = render_table(f, app, table);
    if show_detail {
        render_detail(f, app, detail, selected.as_deref());
    }
}

fn block(title: &str, focused: bool) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            Style::new().fg(theme::ACCENT)
        } else {
            theme::dim()
        })
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                theme::title()
            } else {
                Style::new().bold()
            },
        ))
}

fn render_groups(f: &mut Frame, app: &mut App, area: Rect) {
    let groups = app.store.groups(app.group_by);
    let filtering = !app.filter.is_empty();
    let items: Vec<ListItem> = groups
        .iter()
        .map(|g| {
            let count = format!("{:>4}", g.count);
            let name_w = (area.width as usize).saturating_sub(count.len() + 4);
            ListItem::new(Line::from(vec![
                Span::raw(truncate(&g.title, name_w)),
                Span::raw(" ".repeat(name_w.saturating_sub(g.title.chars().count()))),
                Span::styled(count, theme::dim()),
            ]))
        })
        .collect();
    let title = if filtering {
        "Search (all)"
    } else {
        app.group_by.label()
    };
    let list = List::new(items)
        .block(block(title, app.focus == Focus::Groups))
        .highlight_style(theme::selected())
        .style(if filtering {
            theme::dim()
        } else {
            Style::new()
        });
    app.sync_groups(&groups);
    f.render_stateful_widget(list, area, &mut app.group_state);
}

/// Renders the entity table and returns the selected entity id.
fn render_table(f: &mut Frame, app: &mut App, area: Rect) -> Option<String> {
    let ids = app.list_ids();
    let tab = app.tab;
    let store = &app.store;
    let entities: Vec<&EntityState> = ids.iter().filter_map(|id| store.get(id)).collect();

    let area_w = entities
        .iter()
        .map(|e| store.area_name(&e.entity_id).chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 18) as u16;
    let (header, widths): (Vec<&str>, Vec<Constraint>) = match tab {
        Tab::Scenes => (
            vec!["", "Name", "Type", "Last run / state"],
            vec![
                Constraint::Length(1),
                Constraint::Fill(2),
                Constraint::Length(7),
                Constraint::Fill(1),
            ],
        ),
        Tab::Automations => (
            vec!["", "Name", "Enabled", "Last triggered", "Mode"],
            vec![
                Constraint::Length(1),
                Constraint::Fill(2),
                Constraint::Length(8),
                Constraint::Length(15),
                Constraint::Length(9),
            ],
        ),
        _ => (
            vec!["", "Name", "State", "Area", "Changed"],
            vec![
                Constraint::Length(1),
                Constraint::Fill(3),
                Constraint::Fill(2),
                Constraint::Length(area_w),
                Constraint::Length(7),
            ],
        ),
    };

    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for e in &entities {
        *name_counts.entry(e.friendly_name()).or_default() += 1;
    }
    let rows: Vec<Row> = entities
        .iter()
        .map(|e| {
            let dot = Span::styled("●", Style::new().fg(theme::state_color(&e.state)));
            // Tell apart entities that share a name (e.g. a TV's media_player and remote).
            let name = if name_counts[e.friendly_name()] > 1 {
                Line::from(vec![
                    Span::raw(e.friendly_name().to_string()),
                    Span::styled(format!(" · {}", e.domain()), theme::dim()),
                ])
            } else {
                Line::from(e.friendly_name().to_string())
            };
            match tab {
                Tab::Scenes => Row::new(vec![
                    dot.into(),
                    name.clone(),
                    Line::styled(e.domain().to_string(), theme::dim()),
                    Line::from(theme::format_state(e)),
                ]),
                Tab::Automations => Row::new(vec![
                    dot.into(),
                    name.clone(),
                    Line::styled(
                        e.state.clone(),
                        Style::new().fg(theme::state_color(&e.state)),
                    ),
                    Line::from(theme::last_triggered(e)),
                    Line::styled(e.attr_str("mode").unwrap_or("").to_string(), theme::dim()),
                ]),
                _ => Row::new(vec![
                    dot.into(),
                    name,
                    Line::styled(
                        theme::format_state(e),
                        Style::new().fg(theme::state_color(&e.state)),
                    ),
                    Line::styled(store.area_name(&e.entity_id).to_string(), theme::dim()),
                    Line::styled(
                        e.last_changed.map(theme::ago).unwrap_or_default(),
                        theme::dim(),
                    ),
                ]),
            }
        })
        .collect();

    let title = match tab {
        Tab::Entities if app.filter.is_empty() => {
            let groups = store.groups(app.group_by);
            let i = app
                .group_state
                .selected()
                .unwrap_or(0)
                .min(groups.len().saturating_sub(1));
            groups.get(i).map(|g| g.title.clone()).unwrap_or_default()
        }
        Tab::Entities => "Search results".into(),
        t => t.title().into(),
    };
    let title = format!("{title} ({})", rows.len());
    let focused = app.focus == Focus::List || tab != Tab::Entities;
    let empty = rows.is_empty();

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(theme::dim().bold()))
        .block(block(&title, focused))
        .row_highlight_style(theme::selected())
        .column_spacing(1);

    let selected = app.sync_list(&ids).map(|row| ids[row].clone());
    f.render_stateful_widget(table, area, app.list_state());

    if empty {
        let msg = if app.filter.is_empty() {
            "Nothing here"
        } else {
            "No matches"
        };
        let inner = Rect {
            y: area.y + 2,
            height: 1,
            ..area.inner(ratatui::layout::Margin::new(2, 0))
        };
        f.render_widget(Line::styled(msg, theme::dim()).centered(), inner);
    }
    selected
}

fn render_detail(f: &mut Frame, app: &App, area: Rect, id: Option<&str>) {
    let Some(e) = id.and_then(|id| app.store.get(id)) else {
        f.render_widget(block("Details", false), area);
        return;
    };
    let label = |s: &str| Span::styled(format!("{s:<9}"), theme::dim());
    let mut lines = vec![
        Line::from(e.friendly_name().to_string().bold()),
        Line::styled(e.entity_id.clone(), theme::dim()),
        Line::from(""),
        Line::from(vec![
            label("State"),
            Span::styled(
                theme::format_state(e),
                Style::new().fg(theme::state_color(&e.state)).bold(),
            ),
        ]),
        Line::from(vec![
            label("Area"),
            Span::raw(app.store.area_name(&e.entity_id).to_string()),
        ]),
    ];
    if let Some(t) = e.last_changed {
        lines.push(Line::from(vec![
            label("Changed"),
            Span::raw(format!("{} ago", theme::ago(t))),
            Span::styled(format!("  {}", theme::clock(t)), theme::dim()),
        ]));
    }

    let controls = actions::available(e);
    lines.push(Line::from(""));
    lines.push(Line::styled("Controls", theme::title()));
    if controls.is_empty() {
        lines.push(Line::styled("read-only", theme::dim()));
    }
    for (key, desc) in controls {
        lines.push(Line::from(vec![
            Span::styled(format!(" {key:<6}"), Style::new().fg(theme::ACCENT)),
            Span::raw(desc),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled(format!(" {:<6}", "H"), Style::new().fg(theme::ACCENT)),
        Span::raw("history (24h)"),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::styled("Attributes", theme::title()));
    let width = area.width.saturating_sub(4) as usize;
    let mut attrs: Vec<(&String, &Value)> = e
        .attributes
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "friendly_name" | "icon" | "entity_picture" | "supported_features"
            )
        })
        .collect();
    attrs.sort_by_key(|(k, _)| *k);
    for (k, v) in attrs {
        let v = match v {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| x.to_string())
                })
                .collect::<Vec<_>>()
                .join(", "),
            v => v.to_string(),
        };
        let key = k.replace('_', " ");
        lines.push(Line::from(vec![
            Span::styled(format!("{key}: "), theme::dim()),
            Span::raw(truncate(&v, width.saturating_sub(key.chars().count() + 2))),
        ]));
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(block("Details", false))
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
