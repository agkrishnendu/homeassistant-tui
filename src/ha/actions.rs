//! Maps a key-level [`Action`] on an entity to the Home Assistant service call it means.
//! Pure functions only, so the whole control surface is unit-testable.

use serde_json::Value;

use super::types::{EntityState, ServiceCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Enter / Space: toggle, activate, press.
    Primary,
    /// `+` / `-`: brightness, target temperature, volume, position, value.
    Increase,
    Decrease,
    /// `]` / `[`: color temperature, fan speed.
    SecondaryUp,
    SecondaryDown,
    /// `m`: HVAC mode, select option, media source.
    CycleMode,
    Open,
    Close,
    Stop,
    /// `x`: trigger an automation.
    Trigger,
    /// `n` / `p`: next / previous media track.
    Next,
    Prev,
}

const BRIGHTNESS_STEP_PCT: i64 = 10;
const COLOR_TEMP_STEP_K: f64 = 250.0;
const POSITION_STEP: f64 = 10.0;

pub fn service_for(e: &EntityState, action: Action) -> Option<ServiceCall> {
    use Action::*;
    let id = e.entity_id.as_str();
    let domain = e.domain();
    let call = |svc: &str| ServiceCall::new(domain, svc, id);

    match (domain, action) {
        ("light", Primary) => Some(call("toggle")),
        ("light", Increase) => {
            Some(call("turn_on").with("brightness_step_pct", BRIGHTNESS_STEP_PCT))
        }
        ("light", Decrease) => {
            Some(call("turn_on").with("brightness_step_pct", -BRIGHTNESS_STEP_PCT))
        }
        ("light", SecondaryUp | SecondaryDown) => {
            if !supports_color_temp(e) {
                return None;
            }
            let min = e.attr_f64("min_color_temp_kelvin").unwrap_or(2000.0);
            let max = e.attr_f64("max_color_temp_kelvin").unwrap_or(6500.0);
            let cur = e.attr_f64("color_temp_kelvin").unwrap_or((min + max) / 2.0);
            let delta = if action == SecondaryUp {
                COLOR_TEMP_STEP_K
            } else {
                -COLOR_TEMP_STEP_K
            };
            if min > max {
                return None;
            }
            let k = (cur + delta).clamp(min, max).round() as i64;
            Some(call("turn_on").with("color_temp_kelvin", k))
        }

        ("switch" | "input_boolean" | "siren" | "humidifier", Primary) => Some(call("toggle")),
        ("humidifier", Increase | Decrease) => {
            let cur = e.attr_f64("humidity")?;
            let min = e.attr_f64("min_humidity").unwrap_or(0.0);
            let max = e.attr_f64("max_humidity").unwrap_or(100.0);
            let v = step(cur, action == Increase, 5.0, min, max)?;
            Some(call("set_humidity").with("humidity", v as i64))
        }

        ("fan", Primary) => Some(call("toggle")),
        ("fan", Increase | SecondaryUp) => Some(call("increase_speed")),
        ("fan", Decrease | SecondaryDown) => Some(call("decrease_speed")),

        ("climate", Primary) => Some(call("toggle")),
        ("climate", Increase | Decrease) => {
            let cur = e.attr_f64("temperature")?;
            let step_by = e.attr_f64("target_temp_step").unwrap_or(0.5);
            let min = e.attr_f64("min_temp").unwrap_or(f64::MIN);
            let max = e.attr_f64("max_temp").unwrap_or(f64::MAX);
            let t = step(cur, action == Increase, step_by, min, max)?;
            Some(call("set_temperature").with("temperature", t))
        }
        ("climate", CycleMode) => {
            let next = cycle(&e.attr_list("hvac_modes"), &e.state)?;
            Some(call("set_hvac_mode").with("hvac_mode", next))
        }

        ("cover", Primary) => Some(call("toggle")),
        ("cover", Open) => Some(call("open_cover")),
        ("cover", Close) => Some(call("close_cover")),
        ("cover", Stop) => Some(call("stop_cover")),
        ("cover", Increase | Decrease) => {
            let cur = e.attr_f64("current_position")?;
            let p = step(cur, action == Increase, POSITION_STEP, 0.0, 100.0)?;
            Some(call("set_cover_position").with("position", p as i64))
        }

        ("valve", Primary) => Some(call("toggle")),
        ("valve", Open) => Some(call("open_valve")),
        ("valve", Close) => Some(call("close_valve")),
        ("valve", Stop) => Some(call("stop_valve")),

        ("media_player", Primary) => Some(if e.state == "off" {
            call("turn_on")
        } else {
            call("media_play_pause")
        }),
        ("media_player", Increase) => Some(call("volume_up")),
        ("media_player", Decrease) => Some(call("volume_down")),
        ("media_player", Next) => Some(call("media_next_track")),
        ("media_player", Prev) => Some(call("media_previous_track")),
        ("media_player", Stop) => Some(call("media_stop")),
        ("media_player", CycleMode) => {
            let src = e.attr_str("source").unwrap_or_default();
            let next = cycle(&e.attr_list("source_list"), src)?;
            Some(call("select_source").with("source", next))
        }

        ("lock", Primary) => Some(if e.state == "locked" {
            call("unlock")
        } else {
            call("lock")
        }),
        ("lock", Open) => Some(call("open")),

        ("vacuum", Primary) => Some(if e.state == "cleaning" {
            call("return_to_base")
        } else {
            call("start")
        }),
        ("vacuum", Stop) => Some(call("stop")),

        ("scene" | "script", Primary) => Some(call("turn_on")),
        ("script", Stop) => Some(call("turn_off")),

        ("automation", Primary) => Some(call("toggle")),
        ("automation", Trigger) => Some(call("trigger")),

        ("button" | "input_button", Primary) => Some(call("press")),

        ("input_number" | "number", Increase | Decrease) => {
            let cur: f64 = e.state.parse().ok()?;
            let step_by = e.attr_f64("step").unwrap_or(1.0);
            let min = e.attr_f64("min").unwrap_or(f64::MIN);
            let max = e.attr_f64("max").unwrap_or(f64::MAX);
            let v = step(cur, action == Increase, step_by, min, max)?;
            Some(call("set_value").with("value", v))
        }

        ("input_select" | "select", Primary | CycleMode) => {
            Some(call("select_next").with("cycle", true))
        }

        // Groups and anything else with an on/off state.
        (_, Primary) if matches!(e.state.as_str(), "on" | "off") && !is_read_only(domain) => {
            Some(ServiceCall::new("homeassistant", "toggle", id))
        }
        _ => None,
    }
}

fn is_read_only(domain: &str) -> bool {
    matches!(
        domain,
        "sensor"
            | "binary_sensor"
            | "sun"
            | "weather"
            | "person"
            | "device_tracker"
            | "zone"
            | "update"
            | "event"
    )
}

fn supports_color_temp(e: &EntityState) -> bool {
    e.attr_list("supported_color_modes").contains(&"color_temp")
        || e.attributes.contains_key("color_temp_kelvin")
}

/// Step `cur` up or down, snapping to the step grid and clamping to `[min, max]`.
/// `None` if the entity reports a step or range that can't be used (zero or negative step,
/// min above max, NaN).
fn step(cur: f64, up: bool, by: f64, min: f64, max: f64) -> Option<f64> {
    if !(by > 0.0 && by.is_finite() && cur.is_finite() && min <= max) {
        return None;
    }
    let v = if up { cur + by } else { cur - by };
    let snapped = (v / by).round() * by;
    // Keep float noise like 21.499999 out of the service call.
    Some(((snapped.clamp(min, max)) * 1000.0).round() / 1000.0)
}

fn cycle<'a>(options: &[&'a str], current: &str) -> Option<&'a str> {
    if options.is_empty() {
        return None;
    }
    let i = options
        .iter()
        .position(|o| *o == current)
        .map_or(0, |i| (i + 1) % options.len());
    Some(options[i])
}

/// Actions that could open a door, unlock something, etc. ask for `y/n` first.
pub fn needs_confirm(e: &EntityState, action: Action) -> bool {
    match e.domain() {
        "lock" => true,
        "cover" | "valve" => {
            matches!(
                action,
                Action::Primary
                    | Action::Open
                    | Action::Close
                    | Action::Increase
                    | Action::Decrease
            ) && matches!(e.attr_str("device_class"), Some("garage" | "gate" | "door"))
        }
        _ => false,
    }
}

/// Key hints for the detail pane: `(key, description)` for every action that applies.
pub fn available(e: &EntityState) -> Vec<(&'static str, String)> {
    const KEYS: &[(Action, &str)] = &[
        (Action::Primary, "Enter"),
        (Action::Increase, "+"),
        (Action::Decrease, "-"),
        (Action::SecondaryUp, "]"),
        (Action::SecondaryDown, "["),
        (Action::CycleMode, "m"),
        (Action::Open, "o"),
        (Action::Close, "c"),
        (Action::Stop, "s"),
        (Action::Trigger, "x"),
        (Action::Next, "n"),
        (Action::Prev, "p"),
    ];
    KEYS.iter()
        .filter_map(|(a, key)| service_for(e, *a).map(|c| (*key, describe(&c))))
        .collect()
}

pub fn describe(c: &ServiceCall) -> String {
    let mut s = c.service.replace('_', " ");
    for (k, v) in &c.data {
        if k == "cycle" {
            continue;
        }
        let v = match v {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        if c.service.ends_with(k.as_str()) {
            // set_value(value=45) reads better as "set value 45".
            s.push_str(&format!(" {v}"));
        } else {
            s.push_str(&format!(" {}={v}", k.replace('_', " ")));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ent(id: &str, state: &str, attrs: Value) -> EntityState {
        serde_json::from_value(json!({
            "entity_id": id, "state": state, "attributes": attrs,
            "last_changed": null, "last_updated": null
        }))
        .unwrap()
    }

    #[test]
    fn light_controls() {
        let l = ent(
            "light.k",
            "on",
            json!({"supported_color_modes": ["color_temp"], "color_temp_kelvin": 6400, "max_color_temp_kelvin": 6500, "min_color_temp_kelvin": 2000}),
        );
        assert_eq!(
            service_for(&l, Action::Primary).unwrap().name(),
            "light.toggle"
        );
        let up = service_for(&l, Action::Increase).unwrap();
        assert_eq!(up.data["brightness_step_pct"], 10);
        let ct = service_for(&l, Action::SecondaryUp).unwrap();
        assert_eq!(ct.data["color_temp_kelvin"], 6500);
        let plain = ent("light.p", "on", json!({"supported_color_modes": ["onoff"]}));
        assert!(service_for(&plain, Action::SecondaryUp).is_none());
    }

    #[test]
    fn climate_steps_and_clamps() {
        let c = ent(
            "climate.h",
            "heat",
            json!({"temperature": 21.5, "target_temp_step": 0.5, "max_temp": 22, "min_temp": 7, "hvac_modes": ["off", "heat", "auto"]}),
        );
        assert_eq!(
            service_for(&c, Action::Increase).unwrap().data["temperature"],
            22.0
        );
        let hot = ent(
            "climate.h",
            "heat",
            json!({"temperature": 22, "max_temp": 22}),
        );
        assert_eq!(
            service_for(&hot, Action::Increase).unwrap().data["temperature"],
            22.0
        );
        assert_eq!(
            service_for(&c, Action::Decrease).unwrap().data["temperature"],
            21.0
        );
        assert_eq!(
            service_for(&c, Action::CycleMode).unwrap().data["hvac_mode"],
            "auto"
        );
        let dual = ent("climate.d", "heat_cool", json!({"target_temp_low": 19}));
        assert!(service_for(&dual, Action::Increase).is_none());
    }

    #[test]
    fn cycle_wraps() {
        assert_eq!(cycle(&["a", "b", "c"], "c"), Some("a"));
        assert_eq!(cycle(&["a", "b"], "zzz"), Some("a"));
        assert_eq!(cycle(&[], "a"), None);
    }

    #[test]
    fn misc_domains() {
        let lock = ent("lock.front", "locked", json!({}));
        assert_eq!(
            service_for(&lock, Action::Primary).unwrap().name(),
            "lock.unlock"
        );
        assert!(needs_confirm(&lock, Action::Primary));

        let garage = ent(
            "cover.garage",
            "closed",
            json!({"device_class": "garage", "current_position": 0}),
        );
        assert!(needs_confirm(&garage, Action::Open));
        assert_eq!(
            service_for(&garage, Action::Increase).unwrap().data["position"],
            10
        );
        let blind = ent("cover.blind", "open", json!({"device_class": "blind"}));
        assert!(!needs_confirm(&blind, Action::Open));
        assert!(service_for(&blind, Action::Increase).is_none());

        let num = ent(
            "input_number.n",
            "4.8",
            json!({"step": 0.2, "min": 0, "max": 5}),
        );
        assert_eq!(
            service_for(&num, Action::Increase).unwrap().data["value"],
            5.0
        );

        // Bad attributes from an integration must not panic or send NaN.
        let zero_step = ent("number.z", "1", json!({"step": 0, "min": 0, "max": 5}));
        assert!(service_for(&zero_step, Action::Increase).is_none());
        let inverted = ent("number.i", "1", json!({"step": 1, "min": 5, "max": 0}));
        assert!(service_for(&inverted, Action::Increase).is_none());
        let light = ent(
            "light.l",
            "on",
            json!({"min_color_temp_kelvin": 6500, "max_color_temp_kelvin": 2000}),
        );
        assert!(service_for(&light, Action::SecondaryUp).is_none());

        let sensor = ent("sensor.t", "on", json!({}));
        assert!(service_for(&sensor, Action::Primary).is_none());
        assert!(available(&sensor).is_empty());

        let group = ent("group.all", "on", json!({}));
        assert_eq!(
            service_for(&group, Action::Primary).unwrap().name(),
            "homeassistant.toggle"
        );

        let auto = ent("automation.a", "on", json!({}));
        assert_eq!(
            service_for(&auto, Action::Trigger).unwrap().name(),
            "automation.trigger"
        );

        let mp = ent("media_player.tv", "off", json!({}));
        assert_eq!(
            service_for(&mp, Action::Primary).unwrap().name(),
            "media_player.turn_on"
        );
    }

    #[test]
    fn describes_calls() {
        let c = ServiceCall::new("light", "turn_on", "light.x").with("brightness_step_pct", 10);
        assert_eq!(describe(&c), "turn on brightness step pct=10");
        let c = ServiceCall::new("climate", "set_hvac_mode", "climate.x").with("hvac_mode", "heat");
        assert_eq!(describe(&c), "set hvac mode heat");
    }
}
