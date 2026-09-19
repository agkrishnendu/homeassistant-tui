//! Overlays: command palette, help, confirmation.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Wrap};

use super::{centered, theme};
use crate::app::App;

fn popup_block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Span::styled(format!(" {title} "), theme::title()))
}

pub fn palette(f: &mut Frame, app: &mut App) {
    let full = f.area();
    let w = (full.width * 60 / 100).clamp(40.min(full.width), 90);
    let h = (full.height * 60 / 100).max(8).min(full.height);
    let area = Rect {
        x: full.x + (full.width - w) / 2,
        y: full.y + full.height / 6,
        width: w,
        height: h.min(full.height - full.height / 6),
    };
    f.render_widget(Clear, area);
    let block = popup_block("Command palette").title_bottom(
        Line::styled(" ↑↓ select · Enter run · Esc close ", theme::dim()).right_aligned(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [input, sep, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    f.render_widget(
        Line::from(vec![
            Span::styled("› ", Style::new().fg(theme::ACCENT).bold()),
            Span::raw(app.palette.input.clone()),
            Span::styled("▏", Style::new().fg(theme::ACCENT)),
        ]),
        input,
    );
    f.render_widget(
        Line::styled("─".repeat(sep.width as usize), theme::dim()),
        sep,
    );

    let items: Vec<ListItem> = app
        .palette
        .items
        .iter()
        .map(|i| {
            let mut spans = vec![Span::raw(i.label.clone())];
            if !i.hint.is_empty() {
                spans.push(Span::styled(format!("  {}", i.hint), theme::dim()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    if items.is_empty() {
        f.render_widget(Line::styled("No matches", theme::dim()), list);
        return;
    }
    let list_widget = List::new(items)
        .highlight_style(theme::selected())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list_widget, list, &mut app.palette.state);
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("1-4 / Tab", "switch tab"),
            ("j k ↑ ↓", "move selection"),
            ("h l ← →", "focus groups / entities"),
            ("PgUp PgDn Home End", "jump"),
            ("g", "group by area ↔ domain"),
            ("a", "show / hide system & hidden entities"),
            ("/", "filter (fuzzy, all entities)"),
            (": or Ctrl-p", "command palette"),
            ("Esc", "clear filter / close"),
        ],
    ),
    (
        "Control",
        &[
            ("Enter / Space", "toggle · run · press"),
            ("+ -", "brightness · temperature · volume · position"),
            ("[ ]", "color temperature · fan speed"),
            ("m", "cycle HVAC mode · option · source"),
            ("o c s", "open · close · stop"),
            ("x", "trigger automation"),
            ("n p", "next · previous track"),
            ("H", "history (24h)"),
        ],
    ),
    (
        "Other",
        &[
            ("f (Logbook)", "only the entity selected in Entities"),
            ("r", "resync · refresh logbook"),
            ("?", "this help"),
            ("q / Ctrl-c", "quit"),
        ],
    ),
];

pub fn help(f: &mut Frame) {
    let area = centered(f.area(), 60, 80, 60, 30);
    f.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (section, keys) in HELP {
        lines.push(Line::styled(*section, theme::title()));
        for (k, d) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k:<20}"), Style::new().fg(theme::ACCENT)),
                Span::raw(*d),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        "Locks and garage doors ask for confirmation first.",
        theme::dim(),
    ));
    f.render_widget(
        Paragraph::new(lines)
            .block(popup_block("Keys"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub fn confirm(f: &mut Frame, label: &str) {
    let area = centered(f.area(), 50, 0, 40, 5);
    f.render_widget(Clear, area);
    let text = vec![
        Line::from(label.to_string().bold()),
        Line::from(vec![
            Span::styled("y", Style::new().fg(theme::OK).bold()),
            Span::raw(" confirm   "),
            Span::styled("any other key", theme::dim()),
            Span::raw(" cancel"),
        ]),
    ];
    f.render_widget(
        Paragraph::new(text)
            .centered()
            .wrap(Wrap { trim: true })
            .block(popup_block("Confirm").border_style(Style::new().fg(theme::WARN))),
        area,
    );
}
