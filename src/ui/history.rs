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
    // The popup can stay open past its initial 24h: drop what has scrolled out of view.
    let points = in_window(points, start);
    if let Some(series) = numeric(points, end) {
        render_chart(f, inner, h, &series, start, end);
    } else {
        render_timeline(f, inner, points, start, end);
    }
}

/// The points from the one in effect at `start` onwards.
fn in_window(points: &[HistoryPoint], start: DateTime<Utc>) -> &[HistoryPoint] {
    let first = points
        .partition_point(|p| p.when <= start)
        .saturating_sub(1);
    &points[first..]
}

/// A numeric history, split into runs of readings wherever the sensor had no value.
#[derive(Debug, PartialEq)]
struct Series {
    /// Each run holds (time, value) steps and ends where the value stopped being current.
    runs: Vec<Vec<(DateTime<Utc>, f64)>>,
    /// The current reading, or `None` if the sensor is unavailable now.
    now: Option<f64>,
}

/// The history as numbers, or `None` if the entity isn't numeric.
fn numeric(points: &[HistoryPoint], end: DateTime<Utc>) -> Option<Series> {
    let mut runs = Vec::new();
    let mut run: Vec<(DateTime<Utc>, f64)> = Vec::new();
    let mut any_numeric = false;
    for p in points {
        match p.state.parse::<f64>() {
            Ok(v) if v.is_finite() => {
                any_numeric = true;
                run.push((p.when, v));
            }
            // Unavailable/unknown (or nan/inf) is a gap: the previous reading stops here.
            Ok(_) => close_run(&mut runs, &mut run, p.when),
            Err(_) if matches!(p.state.as_str(), "unavailable" | "unknown" | "") => {
                close_run(&mut runs, &mut run, p.when)
            }
            // Any other text means it's categorical.
            Err(_) => return None,
        }
    }
    let now = run.last().map(|&(_, v)| v);
    close_run(&mut runs, &mut run, end);
    any_numeric.then_some(Series { runs, now })
}

/// End the current run at `at`, holding its last value until then.
fn close_run(
    runs: &mut Vec<Vec<(DateTime<Utc>, f64)>>,
    run: &mut Vec<(DateTime<Utc>, f64)>,
    at: DateTime<Utc>,
) {
    if let Some(&(_, v)) = run.last() {
        run.push((at, v));
        runs.push(std::mem::take(run));
    }
}

fn render_chart(
    f: &mut Frame,
    area: Rect,
    h: &HistoryView,
    series: &Series,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) {
    let secs = |t: DateTime<Utc>| (t.max(start) - start).num_seconds() as f64;
    let data: Vec<Vec<(f64, f64)>> = series
        .runs
        .iter()
        .map(|run| run.iter().map(|(t, v)| (secs(*t), *v)).collect())
        .collect();
    let (min_v, max_v) = data
        .iter()
        .flatten()
        .fold((f64::MAX, f64::MIN), |(lo, hi), (_, v)| {
            (lo.min(*v), hi.max(*v))
        });
    let pad = ((max_v - min_v) * 0.1).max(0.5);
    let (lo, hi) = (min_v - pad, max_v + pad);

    let unit = h.unit.clone().unwrap_or_default();
    let now = match series.now {
        Some(v) => format!("{}{unit}", theme::trim_num(v)),
        None => "unavailable".into(),
    };

    let [summary, chart_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    f.render_widget(
        Line::from(vec![
            Span::styled(" now ", theme::dim()),
            Span::styled(
                now,
                Style::new()
                    .fg(if series.now.is_some() {
                        theme::ACCENT
                    } else {
                        theme::ERROR
                    })
                    .bold(),
            ),
            Span::styled("   min ", theme::dim()),
            Span::raw(format!("{}{unit}", theme::trim_num(min_v))),
            Span::styled("   max ", theme::dim()),
            Span::raw(format!("{}{unit}", theme::trim_num(max_v))),
        ]),
        summary,
    );

    // One dataset per run, so the line breaks where the sensor had no value.
    let datasets: Vec<Dataset> = data
        .iter()
        .map(|run| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(theme::ACCENT))
                .data(run)
        })
        .collect();
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
    let chart = Chart::new(datasets)
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

    fn t(h: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(h * 3600, 0).unwrap()
    }

    fn p(h: i64, state: &str) -> HistoryPoint {
        HistoryPoint {
            when: t(h),
            state: state.into(),
        }
    }

    #[test]
    fn numeric_detection() {
        assert!(numeric(&[p(0, "on"), p(1, "off")], t(2)).is_none());
        assert!(numeric(&[p(0, "unavailable")], t(2)).is_none());
        assert!(numeric(&[p(0, "1.5"), p(1, "unknown")], t(2)).is_some());
    }

    #[test]
    fn gaps_split_runs_and_unavailable_is_not_current() {
        let s = numeric(&[p(0, "21"), p(1, "unavailable")], t(3)).unwrap();
        assert_eq!(s.runs, vec![vec![(t(0), 21.0), (t(1), 21.0)]]);
        assert_eq!(s.now, None);

        let s = numeric(&[p(0, "1"), p(1, "2"), p(2, "unknown"), p(4, "3")], t(5)).unwrap();
        assert_eq!(
            s.runs,
            vec![
                vec![(t(0), 1.0), (t(1), 2.0), (t(2), 2.0)],
                vec![(t(4), 3.0), (t(5), 3.0)],
            ]
        );
        assert_eq!(s.now, Some(3.0));
    }

    #[test]
    fn non_finite_values_are_gaps() {
        let s = numeric(&[p(0, "5"), p(1, "NaN"), p(2, "inf")], t(3)).unwrap();
        assert_eq!(s.runs, vec![vec![(t(0), 5.0), (t(1), 5.0)]]);
        assert_eq!(s.now, None);
    }

    #[test]
    fn window_keeps_the_point_in_effect_at_start() {
        let points = [p(0, "1"), p(1, "2"), p(3, "3")];
        assert_eq!(in_window(&points, t(2)), &points[1..]);
        assert_eq!(in_window(&points, t(0)), &points[..]);
        assert!(in_window(&[], t(0)).is_empty());
    }
}
