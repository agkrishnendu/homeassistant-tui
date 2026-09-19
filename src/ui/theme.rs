//! Colors and value formatting shared by all views.

use chrono::{DateTime, Local, Utc};
use ratatui::style::{Color, Modifier, Style};

use crate::ha::types::EntityState;

pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
pub const ERROR: Color = Color::LightRed;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;

pub fn selected() -> Style {
    Style::new()
        .bg(Color::Indexed(237))
        .add_modifier(Modifier::BOLD)
}

pub fn title() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn state_color(state: &str) -> Color {
    match state {
        "on" | "open" | "opening" | "playing" | "home" | "cleaning" | "heat" | "cool"
        | "heat_cool" | "auto" | "dry" | "fan_only" | "active" | "above_horizon" => Color::Yellow,
        "locked" => Color::Green,
        "unlocked" | "jammed" | "problem" | "triggered" => ERROR,
        "off" | "closed" | "closing" | "idle" | "paused" | "standby" | "not_home" | "docked"
        | "below_horizon" => Color::Gray,
        "unavailable" | "unknown" => Color::Red,
        _ => Color::White,
    }
}

/// Stable color for arbitrary (non-standard) states on history timelines.
pub fn timeline_color(state: &str) -> Color {
    match state_color(state) {
        Color::White => {
            const PALETTE: [Color; 6] = [
                Color::Cyan,
                Color::Magenta,
                Color::Blue,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightMagenta,
            ];
            let h = state
                .bytes()
                .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
            PALETTE[h % PALETTE.len()]
        }
        Color::Gray => Color::DarkGray,
        c => c,
    }
}

/// "5s", "3m", "2h", "4d".
pub fn ago(t: DateTime<Utc>) -> String {
    let s = (Utc::now() - t).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Local clock time, with the weekday if it isn't today.
pub fn clock(t: DateTime<Utc>) -> String {
    let l = t.with_timezone(&Local);
    if l.date_naive() == Local::now().date_naive() {
        l.format("%H:%M:%S").to_string()
    } else {
        l.format("%a %H:%M").to_string()
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Human summary of an entity's state, with the most useful attribute folded in.
pub fn format_state(e: &EntityState) -> String {
    let s = e.state.as_str();
    if e.is_unavailable() {
        return s.to_string();
    }
    match e.domain() {
        "light" if s == "on" => match e.attr_f64("brightness") {
            Some(b) => format!("on {:.0}%", b / 255.0 * 100.0),
            None => "on".into(),
        },
        "fan" if s == "on" => match e.attr_f64("percentage") {
            Some(p) => format!("on {p:.0}%"),
            None => "on".into(),
        },
        "cover" => match e.attr_f64("current_position") {
            Some(p) if s == "open" => format!("open {p:.0}%"),
            _ => s.into(),
        },
        "climate" => {
            let unit = e.unit().unwrap_or("°");
            let target = e
                .attr_f64("temperature")
                .map(|t| format!(" → {}{unit}", trim_num(t)));
            let cur = e
                .attr_f64("current_temperature")
                .map(|t| format!(" ({}{unit})", trim_num(t)));
            format!(
                "{s}{}{}",
                target.unwrap_or_default(),
                cur.unwrap_or_default()
            )
        }
        "media_player" if matches!(s, "playing" | "paused") => match e.attr_str("media_title") {
            Some(t) => format!("{s} · {t}"),
            None => s.into(),
        },
        "scene" | "button" | "input_button" => parse_ts(s)
            .map(|t| format!("{} ago", ago(t)))
            .unwrap_or_else(|| s.into()),
        "sensor" if e.attr_str("device_class") == Some("timestamp") => {
            parse_ts(s).map(clock).unwrap_or_else(|| s.into())
        }
        _ => match e.unit() {
            Some(u) => format!("{s} {u}"),
            None => s.into(),
        },
    }
}

pub fn trim_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

pub fn last_triggered(e: &EntityState) -> String {
    e.attr_str("last_triggered")
        .and_then(parse_ts)
        .map(|t| format!("{} ago", ago(t)))
        .unwrap_or_else(|| "never".into())
}
