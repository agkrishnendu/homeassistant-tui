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

/// Bits of the `supported_features` attribute, per domain (HA `*EntityFeature` enums).
mod feature {
    pub mod cover {
        pub const OPEN: u64 = 1;
        pub const CLOSE: u64 = 2;
        pub const SET_POSITION: u64 = 4;
        pub const STOP: u64 = 8;
    }
    pub mod valve {
        pub const OPEN: u64 = 1;
        pub const CLOSE: u64 = 2;
        pub const STOP: u64 = 8;
    }
    pub mod fan {
        pub const SET_SPEED: u64 = 1;
    }
    pub mod climate {
        pub const TARGET_TEMPERATURE: u64 = 1;
    }
    pub mod lock {
        pub const OPEN: u64 = 1;
    }
    pub mod media_player {
        pub const PAUSE: u64 = 1;
        pub const VOLUME_SET: u64 = 4;
        pub const PREVIOUS_TRACK: u64 = 16;
        pub const NEXT_TRACK: u64 = 32;
        pub const TURN_ON: u64 = 128;
        pub const VOLUME_STEP: u64 = 1024;
        pub const SELECT_SOURCE: u64 = 2048;
        pub const STOP: u64 = 4096;
        pub const PLAY: u64 = 16384;
    }
    pub mod vacuum {
        pub const STOP: u64 = 8;
        pub const RETURN_HOME: u64 = 16;
        pub const START: u64 = 8192;
    }
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
        ("light", Increase | Decrease) => {
            if !supports_brightness(e) {
                return None;
            }
            let pct = if action == Increase {
                BRIGHTNESS_STEP_PCT
            } else {
                -BRIGHTNESS_STEP_PCT
            };
            Some(call("turn_on").with("brightness_step_pct", pct))
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
        ("fan", Increase | SecondaryUp) if supports(e, feature::fan::SET_SPEED) => {
            Some(call("increase_speed"))
        }
        ("fan", Decrease | SecondaryDown) if supports(e, feature::fan::SET_SPEED) => {
            Some(call("decrease_speed"))
        }

        ("climate", Primary) => Some(call("toggle")),
        ("climate", Increase | Decrease) if supports(e, feature::climate::TARGET_TEMPERATURE) => {
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

        ("cover", Primary) if supports(e, feature::cover::OPEN | feature::cover::CLOSE) => {
            Some(call("toggle"))
        }
        ("cover", Open) if supports(e, feature::cover::OPEN) => Some(call("open_cover")),
        ("cover", Close) if supports(e, feature::cover::CLOSE) => Some(call("close_cover")),
        ("cover", Stop) if supports(e, feature::cover::STOP) => Some(call("stop_cover")),
        ("cover", Increase | Decrease) if supports(e, feature::cover::SET_POSITION) => {
            let cur = e.attr_f64("current_position")?;
            let p = step(cur, action == Increase, POSITION_STEP, 0.0, 100.0)?;
            Some(call("set_cover_position").with("position", p as i64))
        }

        ("valve", Primary) if supports(e, feature::valve::OPEN | feature::valve::CLOSE) => {
            Some(call("toggle"))
        }
        ("valve", Open) if supports(e, feature::valve::OPEN) => Some(call("open_valve")),
        ("valve", Close) if supports(e, feature::valve::CLOSE) => Some(call("close_valve")),
        ("valve", Stop) if supports(e, feature::valve::STOP) => Some(call("stop_valve")),

        ("media_player", Primary) if e.state == "off" => {
            supports(e, feature::media_player::TURN_ON).then(|| call("turn_on"))
        }
        ("media_player", Primary) => supports(
            e,
            feature::media_player::PAUSE | feature::media_player::PLAY,
        )
        .then(|| call("media_play_pause")),
        ("media_player", Increase | Decrease) => supports(
            e,
            feature::media_player::VOLUME_STEP | feature::media_player::VOLUME_SET,
        )
        .then(|| {
            call(if action == Increase {
                "volume_up"
            } else {
                "volume_down"
            })
        }),
        ("media_player", Next) if supports(e, feature::media_player::NEXT_TRACK) => {
            Some(call("media_next_track"))
        }
        ("media_player", Prev) if supports(e, feature::media_player::PREVIOUS_TRACK) => {
            Some(call("media_previous_track"))
        }
        ("media_player", Stop) if supports(e, feature::media_player::STOP) => {
            Some(call("media_stop"))
        }
        ("media_player", CycleMode) if supports(e, feature::media_player::SELECT_SOURCE) => {
            let src = e.attr_str("source").unwrap_or_default();
            let next = cycle(&e.attr_list("source_list"), src)?;
            Some(call("select_source").with("source", next))
        }

        ("lock", Primary) => Some(if e.state == "locked" {
            call("unlock")
        } else {
            call("lock")
        }),
        ("lock", Open) if supports(e, feature::lock::OPEN) => Some(call("open")),

        ("vacuum", Primary) if e.state == "cleaning" => {
            supports(e, feature::vacuum::RETURN_HOME).then(|| call("return_to_base"))
        }
        ("vacuum", Primary) => supports(e, feature::vacuum::START).then(|| call("start")),
        ("vacuum", Stop) if supports(e, feature::vacuum::STOP) => Some(call("stop")),

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

/// Whether the entity advertises any of the `flags` in `supported_features`.
/// An entity that doesn't report the attribute at all is assumed capable, so an unknown
/// device still gets its controls and Home Assistant remains the judge.
fn supports(e: &EntityState, flags: u64) -> bool {
    match e
        .attributes
        .get("supported_features")
        .and_then(Value::as_u64)
    {
        Some(bits) => bits & flags != 0,
        None => true,
    }
}

/// `supported_color_modes` when the light reports it.
fn color_modes(e: &EntityState) -> Option<Vec<&str>> {
    e.attributes
        .contains_key("supported_color_modes")
        .then(|| e.attr_list("supported_color_modes"))
}

/// Anything beyond plain on/off can be dimmed.
fn supports_brightness(e: &EntityState) -> bool {
    color_modes(e).is_none_or(|m| m.iter().any(|m| !matches!(*m, "onoff" | "unknown")))
}

fn supports_color_temp(e: &EntityState) -> bool {
    match color_modes(e) {
        Some(modes) => modes.contains(&"color_temp"),
        None => e.attributes.contains_key("color_temp_kelvin"),
    }
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
    fn light_brightness_needs_more_than_onoff() {
        let onoff = ent("light.o", "on", json!({"supported_color_modes": ["onoff"]}));
        assert!(service_for(&onoff, Action::Primary).is_some());
        assert!(service_for(&onoff, Action::Increase).is_none());
        assert!(service_for(&onoff, Action::Decrease).is_none());
        assert!(available(&onoff).iter().all(|(k, _)| *k == "Enter"));

        let dim = ent(
            "light.d",
            "on",
            json!({"supported_color_modes": ["brightness"]}),
        );
        assert!(service_for(&dim, Action::Increase).is_some());
        // Dimmable, but no color temperature.
        assert!(service_for(&dim, Action::SecondaryUp).is_none());

        // Modes are authoritative when reported, even if a stale attribute lingers.
        let rgb = ent(
            "light.r",
            "on",
            json!({"supported_color_modes": ["rgb"], "color_temp_kelvin": null}),
        );
        assert!(service_for(&rgb, Action::Increase).is_some());
        assert!(service_for(&rgb, Action::SecondaryUp).is_none());

        // Unknown capabilities keep the controls.
        let bare = ent("light.b", "on", json!({}));
        assert!(service_for(&bare, Action::Increase).is_some());
    }

    #[test]
    fn cover_controls_follow_supported_features() {
        // OPEN | CLOSE only: no stop, no position.
        let basic = ent(
            "cover.b",
            "open",
            json!({"supported_features": 3, "current_position": 40}),
        );
        assert!(service_for(&basic, Action::Open).is_some());
        assert!(service_for(&basic, Action::Close).is_some());
        assert!(service_for(&basic, Action::Primary).is_some());
        assert!(service_for(&basic, Action::Stop).is_none());
        assert!(service_for(&basic, Action::Increase).is_none());

        // OPEN | CLOSE | SET_POSITION | STOP.
        let full = ent(
            "cover.f",
            "open",
            json!({"supported_features": 15, "current_position": 40}),
        );
        assert_eq!(
            service_for(&full, Action::Increase).unwrap().data["position"],
            50
        );
        assert!(service_for(&full, Action::Stop).is_some());

        let none = ent("cover.n", "open", json!({"supported_features": 0}));
        assert!(service_for(&none, Action::Primary).is_none());
        assert!(available(&none).is_empty());
    }

    #[test]
    fn media_player_controls_follow_supported_features() {
        // PAUSE | PREVIOUS_TRACK | NEXT_TRACK | VOLUME_STEP | SELECT_SOURCE
        let tv = ent(
            "media_player.tv",
            "playing",
            json!({"supported_features": 1 | 16 | 32 | 1024 | 2048, "source_list": ["a", "b"]}),
        );
        assert_eq!(
            service_for(&tv, Action::Primary).unwrap().name(),
            "media_player.media_play_pause"
        );
        assert!(service_for(&tv, Action::Next).is_some());
        assert!(service_for(&tv, Action::Prev).is_some());
        assert!(service_for(&tv, Action::Increase).is_some());
        assert!(service_for(&tv, Action::CycleMode).is_some());
        assert!(service_for(&tv, Action::Stop).is_none());

        // A speaker that only plays/pauses.
        let basic = ent(
            "media_player.s",
            "playing",
            json!({"supported_features": 1}),
        );
        assert!(service_for(&basic, Action::Primary).is_some());
        assert!(service_for(&basic, Action::Next).is_none());
        assert!(service_for(&basic, Action::Prev).is_none());
        assert!(service_for(&basic, Action::Increase).is_none());
        assert!(service_for(&basic, Action::CycleMode).is_none());

        // Off and unable to turn on: nothing to do.
        let off = ent("media_player.o", "off", json!({"supported_features": 1}));
        assert!(service_for(&off, Action::Primary).is_none());
        let off_ok = ent("media_player.o", "off", json!({"supported_features": 128}));
        assert_eq!(
            service_for(&off_ok, Action::Primary).unwrap().name(),
            "media_player.turn_on"
        );
    }

    #[test]
    fn fan_climate_lock_vacuum_valve_follow_supported_features() {
        let fan = ent("fan.f", "on", json!({"supported_features": 8}));
        assert!(service_for(&fan, Action::Increase).is_none());
        assert!(service_for(&fan, Action::SecondaryDown).is_none());
        let fan = ent("fan.f", "on", json!({"supported_features": 1}));
        assert!(service_for(&fan, Action::Increase).is_some());

        let cl = ent(
            "climate.c",
            "heat",
            json!({"temperature": 20, "supported_features": 2}),
        );
        assert!(service_for(&cl, Action::Increase).is_none());
        let cl = ent(
            "climate.c",
            "heat",
            json!({"temperature": 20, "supported_features": 1}),
        );
        assert!(service_for(&cl, Action::Increase).is_some());

        let lock = ent("lock.l", "locked", json!({"supported_features": 0}));
        assert!(service_for(&lock, Action::Open).is_none());
        let lock = ent("lock.l", "locked", json!({"supported_features": 1}));
        assert!(service_for(&lock, Action::Open).is_some());

        // STOP | RETURN_HOME, but not START.
        let vac = ent("vacuum.v", "docked", json!({"supported_features": 8 | 16}));
        assert!(service_for(&vac, Action::Primary).is_none());
        assert!(service_for(&vac, Action::Stop).is_some());
        let vac = ent(
            "vacuum.v",
            "cleaning",
            json!({"supported_features": 8 | 16}),
        );
        assert_eq!(
            service_for(&vac, Action::Primary).unwrap().name(),
            "vacuum.return_to_base"
        );

        let valve = ent("valve.v", "open", json!({"supported_features": 2}));
        assert!(service_for(&valve, Action::Open).is_none());
        assert!(service_for(&valve, Action::Close).is_some());
        assert!(service_for(&valve, Action::Stop).is_none());
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
