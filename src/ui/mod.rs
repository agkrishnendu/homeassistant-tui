mod entities;
mod history;
mod logbook;
mod popups;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs};

use crate::app::{App, Mode, Tab};
use crate::ha::client::ConnStatus;

pub fn render(f: &mut Frame, app: &mut App) {
    let [top, main, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    render_tabs(f, app, top);
    if !app.store.loaded {
        render_splash(f, app, main);
    } else {
        match app.tab {
            Tab::Entities | Tab::Scenes | Tab::Automations => entities::render(f, app, main),
            Tab::Logbook => logbook::render(f, app, main),
        }
    }
    render_status(f, app, status);

    match app.mode.clone() {
        Mode::Palette => popups::palette(f, app),
        Mode::Help => popups::help(f),
        Mode::Confirm { label, .. } => popups::confirm(f, &label),
        Mode::History => history::render(f, app),
        Mode::Normal | Mode::Filter => {}
    }
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} {}", i + 1, t.title()));
    let idx = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    let right = format!(" {} entities ", app.store.entities.len());
    // The branding yields to the tab titles on narrow terminals.
    let right_w = if area.width >= 100 {
        right.len() as u16 + 10
    } else {
        0
    };
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w)]).areas(area);
    let tabs = Tabs::new(titles)
        .select(idx)
        .style(theme::dim())
        .highlight_style(theme::title().reversed())
        .divider(" ");
    f.render_widget(tabs, left_area);
    let brand = Line::from(vec![
        Span::styled(right, theme::dim()),
        Span::styled("homeassistant-tui ", theme::title()),
    ])
    .right_aligned();
    f.render_widget(brand, right_area);
}

fn render_splash(f: &mut Frame, app: &App, area: Rect) {
    let msg = match &app.status {
        ConnStatus::AuthFailed(m) => vec![
            Line::from("Authentication failed".fg(theme::ERROR).bold()),
            Line::from(m.clone()),
            Line::from(""),
            Line::from(
                "Check your long-lived access token (HA → Profile → Security).".fg(theme::DIM),
            ),
        ],
        ConnStatus::Disconnected { error, retry_in } => vec![
            Line::from("Cannot reach Home Assistant".fg(theme::ERROR).bold()),
            Line::from(error.clone()),
            Line::from(
                format!(
                    "retrying in {}s… (r to retry now)",
                    retry_in
                        .saturating_sub(app.status_since.elapsed())
                        .as_secs()
                )
                .fg(theme::DIM),
            ),
        ],
        _ => vec![Line::from(
            "Connecting to Home Assistant…".fg(theme::ACCENT),
        )],
    };
    let [area] = Layout::vertical([Constraint::Length(msg.len() as u16)])
        .flex(Flex::Center)
        .areas(area);
    f.render_widget(Paragraph::new(msg).centered(), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let conn = match &app.status {
        ConnStatus::Connecting => Span::styled(" ● connecting ", Style::new().fg(theme::WARN)),
        ConnStatus::Connected { version } => {
            Span::styled(format!(" ● HA {version} "), Style::new().fg(theme::OK))
        }
        ConnStatus::Disconnected { retry_in, .. } => Span::styled(
            format!(
                " ● offline, retry in {}s ",
                retry_in
                    .saturating_sub(app.status_since.elapsed())
                    .as_secs()
            ),
            Style::new().fg(theme::ERROR),
        ),
        ConnStatus::AuthFailed(_) => Span::styled(" ● auth failed ", Style::new().fg(theme::ERROR)),
    };
    let mut left = vec![conn];
    if app.mode == Mode::Filter || !app.filter.is_empty() {
        let cursor = if app.mode == Mode::Filter { "▏" } else { "" };
        left.push(Span::styled(
            format!(" /{}{cursor} ", app.filter),
            theme::title(),
        ));
    }

    let right = match &app.toast {
        Some(t) => Span::styled(
            format!(" {} ", t.text),
            Style::new().fg(if t.error { theme::ERROR } else { theme::OK }),
        ),
        None => Span::styled(hints(app), theme::dim()),
    };

    let left_w: u16 = left.iter().map(|s| s.width() as u16).sum();
    let [l, r] = Layout::horizontal([Constraint::Length(left_w), Constraint::Fill(1)]).areas(area);
    // Long hints would be clipped on the left; fall back to the essentials.
    let right = if app.toast.is_none() && right.width() > r.width as usize {
        Span::styled("? help · q quit ", theme::dim())
    } else {
        right
    };
    f.render_widget(Line::from(left), l);
    f.render_widget(Line::from(right).right_aligned(), r);
}

fn hints(app: &App) -> String {
    let s = match (&app.mode, app.tab) {
        (Mode::Filter, _) => "type to filter · ↑↓ move · Enter keep · Esc clear",
        (_, Tab::Logbook) => "j/k scroll · f filter to entity · r refresh · ? help · q quit",
        (_, Tab::Entities) => {
            "Enter toggle · +/- adjust · H history · / filter · : palette · g group · ? help"
        }
        (_, Tab::Scenes) => "Enter run · s stop script · H history · / filter · ? help",
        (_, Tab::Automations) => "Enter enable/disable · x trigger · H history · / filter · ? help",
    };
    format!("{s} ")
}

/// A centered rectangle of the given percentage of `area`, at least `min` cells.
pub(crate) fn centered(area: Rect, pct_w: u16, pct_h: u16, min_w: u16, min_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).max(min_w).min(area.width);
    let h = (area.height * pct_h / 100).max(min_h).min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
