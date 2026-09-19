//! History popup: a line chart for numeric sensors, a colored timeline for everything else.

use chrono::{DateTime, Duration, Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, BorderType, Chart, Clear, Dataset, GraphType, Paragraph};

use super::{centered, theme};
use crate::app::{App, HistoryView};
use crate::ha::types::HistoryPoint;

pub fn render(f: &mut Frame, app: &App) {
    let Some(h) = &app.history else { return };
    let area = centered(f.area(), 90, 80, 40, 12);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Span::styled(
            format!(" History · {} · last 24h ", h.name),
            theme::title(),
        ))
        .title_bottom(Line::styled(" r refresh · Esc close ", theme::dim()).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let msg = |f: &mut Frame, text: String, style: Style| {
        f.render_widget(
            Paragraph::new(text).style(style).centered(),
            Rect {
                y: inner.y + inner.height / 2,
                height: 1,
                ..inner
            },
        );
    };
    if let Some(err) = &h.error {
        return msg(
            f,
            format!("Could not load history: {err}"),
            Style::new().fg(theme::ERROR),
        );
    }
    let Some(points) = &h.points else {
        return msg(f, "Loading…".into(), theme::dim());
    };
    if points.is_empty() {
        return msg(
            f,
            "No history recorded in the last 24h".into(),
            theme::dim(),
        );
    }

    let end = Utc::now();
    let start = end - Duration::hours(24);
    if let Some(values) = numeric(points) {
        render_chart(f, inner, h, &values, start, end);
    } else {
        render_timeline(f, inner, points, start, end);
    }
}

/// Numeric (seconds since `start`, value) pairs, or `None` if the entity isn't numeric.
fn numeric(points: &[HistoryPoint]) -> Option<Vec<(DateTime<Utc>, f64)>> {
    let mut out = Vec::new();
    let mut any_numeric = false;
    for p in points {
        match p.state.parse::<f64>() {
            Ok(v) => {
                any_numeric = true;
                out.push((p.when, v));
            }
            // Gaps (unavailable/unknown) are skipped, but any other text means it's categorical.
            Err(_) if matches!(p.state.as_str(), "unavailable" | "unknown" | "") => {}
            Err(_) => return None,
        }
    }
    any_numeric.then_some(out)
}

fn render_chart(
    f: &mut Frame,
    area: Rect,
    h: &HistoryView,
    values: &[(DateTime<Utc>, f64)],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) {
    let secs = |t: DateTime<Utc>| (t.max(start) - start).num_seconds() as f64;
    let mut data: Vec<(f64, f64)> = values.iter().map(|(t, v)| (secs(*t), *v)).collect();
    // Extend the last known value to "now" so the line reaches the right edge.
    if let Some(&(_, v)) = data.last() {
        data.push((secs(end), v));
    }
    let (mut lo, mut hi) = data.iter().fold((f64::MAX, f64::MIN), |(lo, hi), (_, v)| {
        (lo.min(*v), hi.max(*v))
    });
    let pad = ((hi - lo) * 0.1).max(0.5);
    lo -= pad;
    hi += pad;

    let unit = h.unit.clone().unwrap_or_default();
    let last = values.last().map(|(_, v)| *v).unwrap_or_default();
    let (min_v, max_v) = values.iter().fold((f64::MAX, f64::MIN), |(a, b), (_, v)| {
        (a.min(*v), b.max(*v))
    });

    let [summary, chart_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    f.render_widget(
        Line::from(vec![
            Span::styled(" now ", theme::dim()),
            Span::styled(
                format!("{}{unit}", theme::trim_num(last)),
                Style::new().fg(theme::ACCENT).bold(),
            ),
            Span::styled("   min ", theme::dim()),
            Span::raw(format!("{}{unit}", theme::trim_num(min_v))),
            Span::styled("   max ", theme::dim()),
            Span::raw(format!("{}{unit}", theme::trim_num(max_v))),
        ]),
        summary,
    );

    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::new().fg(theme::ACCENT))
        .data(&data);
    let x_labels: Vec<Line> = [start, start + Duration::hours(12), end]
        .into_iter()
        .map(|t| {
            Line::styled(
                t.with_timezone(&Local).format("%H:%M").to_string(),
                theme::dim(),
            )
        })
        .collect();
    let y_labels: Vec<Line> = [lo, (lo + hi) / 2.0, hi]
        .into_iter()
        .map(|v| Line::styled(format!("{}{unit}", theme::trim_num(v)), theme::dim()))
        .collect();
    let chart = Chart::new(vec![dataset])
        .x_axis(
            Axis::default()
                .bounds([0.0, secs(end)])
                .labels(x_labels)
                .style(theme::dim()),
        )
        .y_axis(
            Axis::default()
                .bounds([lo, hi])
                .labels(y_labels)
                .style(theme::dim()),
        );
    f.render_widget(chart, chart_area);
}

fn render_timeline(
    f: &mut Frame,
    area: Rect,
    points: &[HistoryPoint],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) {
    let [bar, axis, _, list] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);

    // One cell per time slice, colored by the state in effect at that time.
    let width = bar.width.max(1) as i64;
    let span = (end - start).num_seconds().max(1);
    let cells: Vec<Span> = (0..width)
        .map(|x| {
            let t = start + Duration::seconds(span * x / width);
            let state = points
                .iter()
                .take_while(|p| p.when <= t)
                .last()
                .map(|p| p.state.as_str());
            match state {
                Some(s) => Span::styled("█", Style::new().fg(theme::timeline_color(s))),
                None => Span::styled("·", theme::dim()),
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(vec![Line::from(cells.clone()), Line::from(cells)]),
        bar,
    );

    let fmt = |t: DateTime<Utc>| t.with_timezone(&Local).format("%H:%M").to_string();
    let [l, m, r] = Layout::horizontal([Constraint::Fill(1); 3]).areas(axis);
    f.render_widget(Line::styled(fmt(start), theme::dim()), l);
    f.render_widget(
        Line::styled(fmt(start + Duration::hours(12)), theme::dim()).centered(),
        m,
    );
    f.render_widget(Line::styled("now", theme::dim()).right_aligned(), r);

    // Newest changes first, with how long each state lasted.
    let mut lines = Vec::new();
    for (i, p) in points.iter().enumerate().rev() {
        let until = points.get(i + 1).map(|n| n.when).unwrap_or(end);
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>9}  ", theme::clock(p.when)), theme::dim()),
            Span::styled("■ ", Style::new().fg(theme::timeline_color(&p.state))),
            Span::raw(format!("{:<16}", p.state)),
            Span::styled(duration(until - p.when), theme::dim()),
        ]));
        if lines.len() >= list.height as usize {
            break;
        }
    }
    f.render_widget(Paragraph::new(lines), list);
}

fn duration(d: Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(state: &str) -> HistoryPoint {
        HistoryPoint {
            when: Utc::now(),
            state: state.into(),
        }
    }

    #[test]
    fn numeric_detection() {
        assert_eq!(
            numeric(&[p("1.5"), p("unavailable"), p("2")])
                .unwrap()
                .len(),
            2
        );
        assert!(numeric(&[p("on"), p("off")]).is_none());
        assert!(numeric(&[p("unavailable")]).is_none());
    }
}
