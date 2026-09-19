//! A small in-process fake of Home Assistant's WebSocket API.
//!
//! It speaks the auth handshake, bootstrap commands, `state_changed` subscriptions,
//! `call_service` (with simple semantics for common domains), history and logbook.
//! Used by the integration tests and by `cargo run --example mock_ha` for demos.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

pub const VERSION: &str = "2026.9.0-mock";

#[derive(Default)]
struct Inner {
    states: BTreeMap<String, Value>,
    /// Outgoing channels of authenticated connections with a state_changed subscription.
    subscribers: Vec<(u64, mpsc::UnboundedSender<Message>)>,
    calls: Vec<(String, String, Value)>,
}

#[derive(Clone)]
pub struct MockHa {
    pub addr: SocketAddr,
    token: String,
    inner: Arc<Mutex<Inner>>,
    kick: broadcast::Sender<()>,
}

impl MockHa {
    pub async fn start(token: &str, bind: &str) -> Self {
        let listener = TcpListener::bind(bind).await.expect("bind mock HA");
        let addr = listener.local_addr().unwrap();
        let (kick, _) = broadcast::channel(4);
        let ha = MockHa {
            addr,
            token: token.into(),
            inner: Arc::new(Mutex::new(Inner {
                states: fixture_states(),
                ..Default::default()
            })),
            kick,
        };
        let server = ha.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                        server.connection(ws).await;
                    }
                });
            }
        });
        ha
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Drop every open connection (to exercise reconnects).
    pub fn kick_all(&self) {
        let _ = self.kick.send(());
    }

    pub fn state(&self, id: &str) -> Option<Value> {
        self.inner.lock().unwrap().states.get(id).cloned()
    }

    pub fn calls(&self) -> Vec<(String, String, Value)> {
        self.inner.lock().unwrap().calls.clone()
    }

    /// Change a state from "outside" (as if a device reported it) and notify subscribers.
    pub fn set_state(&self, id: &str, state: &str) {
        let mut inner = self.inner.lock().unwrap();
        let Some(old) = inner.states.get(id).cloned() else {
            return;
        };
        let mut new = old.clone();
        new["state"] = state.into();
        touch(&mut new);
        inner.states.insert(id.into(), new.clone());
        broadcast_change(&mut inner, id, Some(old), Some(new));
    }

    async fn connection(&self, ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let mut kick = self.kick.subscribe();
        let writer = tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let send = |v: Value| {
            let _ = out_tx.send(Message::text(v.to_string()));
        };

        send(json!({"type": "auth_required", "ha_version": VERSION}));
        let mut authed = false;
        loop {
            let msg = tokio::select! {
                m = stream.next() => m,
                _ = kick.recv() => break,
            };
            let Some(Ok(Message::Text(text))) = msg else {
                if matches!(msg, Some(Ok(_))) {
                    continue;
                }
                break;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if !authed {
                if v["type"] == "auth" && v["access_token"] == self.token.as_str() {
                    authed = true;
                    send(json!({"type": "auth_ok", "ha_version": VERSION}));
                    continue;
                }
                send(
                    json!({"type": "auth_invalid", "message": "Invalid access token or password"}),
                );
                break;
            }
            let id = v["id"].as_u64().unwrap_or(0);
            let ok = |result: Value| json!({"id": id, "type": "result", "success": true, "result": result});
            let fail = |code: &str, message: &str| json!({"id": id, "type": "result", "success": false, "error": {"code": code, "message": message}});
            match v["type"].as_str().unwrap_or("") {
                "ping" => send(json!({"id": id, "type": "pong"})),
                "subscribe_events" => {
                    self.inner
                        .lock()
                        .unwrap()
                        .subscribers
                        .push((id, out_tx.clone()));
                    send(ok(Value::Null));
                }
                "get_states" => {
                    let states: Vec<Value> = self
                        .inner
                        .lock()
                        .unwrap()
                        .states
                        .values()
                        .cloned()
                        .collect();
                    send(ok(Value::Array(states)));
                }
                "config/area_registry/list" => send(ok(fixture_areas())),
                "config/device_registry/list" => send(ok(fixture_devices())),
                "config/entity_registry/list" => send(ok(fixture_registry())),
                "call_service" => {
                    let domain = v["domain"].as_str().unwrap_or("").to_string();
                    let service = v["service"].as_str().unwrap_or("").to_string();
                    let data = v["service_data"].clone();
                    let target = v["target"]["entity_id"].as_str().unwrap_or("").to_string();
                    let mut inner = self.inner.lock().unwrap();
                    inner
                        .calls
                        .push((format!("{domain}.{service}"), target.clone(), data.clone()));
                    let Some(old) = inner.states.get(&target).cloned() else {
                        send(fail("not_found", &format!("Entity {target} not found")));
                        continue;
                    };
                    let mut new = old.clone();
                    match apply_service(&domain, &service, &data, &mut new) {
                        Ok(()) => {
                            touch(&mut new);
                            inner.states.insert(target.clone(), new.clone());
                            send(ok(json!({"context": {"id": "mock"}})));
                            broadcast_change(&mut inner, &target, Some(old), Some(new));
                        }
                        Err(e) => send(fail("service_validation_error", &e)),
                    }
                }
                "history/history_during_period" => {
                    let ids: Vec<String> = v["entity_ids"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut out = serde_json::Map::new();
                    for id in ids {
                        let cur = self.state(&id);
                        out.insert(id.clone(), fake_history(&id, cur.as_ref()));
                    }
                    send(ok(Value::Object(out)));
                }
                "logbook/get_events" => send(ok(fake_logbook(&v["entity_ids"]))),
                other => send(fail("unknown_command", &format!("Unknown command {other}"))),
            }
        }
        self.inner
            .lock()
            .unwrap()
            .subscribers
            .retain(|(_, tx)| !tx.same_channel(&out_tx));
        drop(out_tx);
        let _ = writer.await;
    }
}

fn touch(state: &mut Value) {
    let now = Utc::now().to_rfc3339();
    state["last_updated"] = now.clone().into();
    state["last_changed"] = now.into();
}

fn broadcast_change(inner: &mut Inner, id: &str, old: Option<Value>, new: Option<Value>) {
    inner.subscribers.retain(|(sub_id, tx)| {
        let ev = json!({
            "id": sub_id,
            "type": "event",
            "event": {
                "event_type": "state_changed",
                "data": {"entity_id": id, "old_state": old, "new_state": new},
                "origin": "LOCAL",
                "time_fired": Utc::now().to_rfc3339(),
            }
        });
        tx.send(Message::text(ev.to_string())).is_ok()
    });
}

fn apply_service(domain: &str, service: &str, data: &Value, st: &mut Value) -> Result<(), String> {
    let cur = st["state"].as_str().unwrap_or("").to_string();
    let attrs = &mut st["attributes"];
    let f = |v: &Value| v.as_f64().unwrap_or(0.0);
    let new_state: Option<String> = match (domain, service) {
        (_, "toggle") => Some(
            match cur.as_str() {
                "on" => "off",
                "off" => "on",
                "open" => "closed",
                "closed" => "open",
                "heat" | "cool" | "auto" => "off",
                s => return Err(format!("can't toggle from {s}")),
            }
            .into(),
        ),
        ("light", "turn_on") => {
            let mut b = if cur == "on" {
                f(&attrs["brightness"])
            } else {
                0.0
            };
            if let Some(step) = data["brightness_step_pct"].as_f64() {
                b = (b + step * 2.55).clamp(0.0, 255.0);
            } else if b == 0.0 {
                b = 255.0;
            }
            if let Some(k) = data["color_temp_kelvin"].as_f64() {
                attrs["color_temp_kelvin"] = k.into();
                if b == 0.0 {
                    b = 255.0;
                }
            }
            attrs["brightness"] = b.round().into();
            Some(if b > 0.0 { "on" } else { "off" }.into())
        }
        ("scene" | "input_button" | "button", "turn_on" | "press") => Some(Utc::now().to_rfc3339()),
        ("script", "turn_on") => {
            attrs["last_triggered"] = Utc::now().to_rfc3339().into();
            None
        }
        (_, "turn_on") => Some("on".into()),
        (_, "turn_off") => Some("off".into()),
        ("climate", "set_temperature") => {
            attrs["temperature"] = data["temperature"].clone();
            None
        }
        ("climate", "set_hvac_mode") => data["hvac_mode"].as_str().map(String::from),
        ("cover", "open_cover") => {
            attrs["current_position"] = 100.into();
            Some("open".into())
        }
        ("cover", "close_cover") => {
            attrs["current_position"] = 0.into();
            Some("closed".into())
        }
        ("cover", "stop_cover") => None,
        ("cover", "set_cover_position") => {
            let p = f(&data["position"]);
            attrs["current_position"] = p.into();
            Some(if p > 0.0 { "open" } else { "closed" }.into())
        }
        ("lock", "lock") => Some("locked".into()),
        ("lock", "unlock") => Some("unlocked".into()),
        ("fan", "increase_speed" | "decrease_speed") => {
            let d = if service == "increase_speed" {
                33.0
            } else {
                -33.0
            };
            let p = (f(&attrs["percentage"]) + d).clamp(0.0, 100.0);
            attrs["percentage"] = p.round().into();
            Some(if p > 0.0 { "on" } else { "off" }.into())
        }
        ("media_player", "media_play_pause") => Some(
            if cur == "playing" {
                "paused"
            } else {
                "playing"
            }
            .into(),
        ),
        ("media_player", "volume_up" | "volume_down") => {
            let d = if service == "volume_up" { 0.05 } else { -0.05 };
            attrs["volume_level"] =
                (((f(&attrs["volume_level"]) + d).clamp(0.0, 1.0) * 100.0).round() / 100.0).into();
            None
        }
        ("media_player", "select_source") => {
            attrs["source"] = data["source"].clone();
            None
        }
        ("media_player", "media_next_track" | "media_previous_track" | "media_stop") => None,
        ("automation", "trigger") => {
            attrs["last_triggered"] = Utc::now().to_rfc3339().into();
            None
        }
        ("input_number" | "number", "set_value") => {
            Some(data["value"].as_f64().unwrap_or(0.0).to_string())
        }
        ("input_select" | "select", "select_next") => {
            let opts: Vec<String> = attrs["options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let i = opts
                .iter()
                .position(|o| *o == cur)
                .map_or(0, |i| (i + 1) % opts.len().max(1));
            opts.get(i).cloned()
        }
        _ => return Err(format!("Service {domain}.{service} not found")),
    };
    if let Some(s) = new_state {
        st["state"] = s.into();
    }
    Ok(())
}

fn fake_history(id: &str, cur: Option<&Value>) -> Value {
    let now = Utc::now();
    let start = now - Duration::hours(24);
    let cur_state = cur
        .and_then(|c| c["state"].as_str())
        .unwrap_or("unknown")
        .to_string();
    let ts = |t: chrono::DateTime<Utc>| t.timestamp_millis() as f64 / 1000.0;
    let mut out = Vec::new();
    if let Ok(base) = cur_state.parse::<f64>() {
        // A daily sine curve, sampled every 15 minutes.
        for i in 0..=96 {
            let t = start + Duration::minutes(15 * i);
            let phase = (i as f64 / 96.0) * std::f64::consts::TAU;
            let v = base + 2.0 * (phase - 1.0).sin() + ((i * 7919) % 10) as f64 * 0.03;
            out.push(json!({"s": format!("{v:.1}"), "lu": ts(t)}));
        }
    } else {
        let alt = match cur_state.as_str() {
            "on" => "off",
            "off" => "on",
            "locked" => "unlocked",
            "unlocked" => "locked",
            "open" => "closed",
            "closed" => "open",
            "playing" => "paused",
            _ => "unknown",
        };
        let mut s = if id.len().is_multiple_of(2) {
            cur_state.as_str()
        } else {
            alt
        };
        for h in [0, 3, 7, 8, 12, 17, 19, 22] {
            out.push(json!({"s": s, "lu": ts(start + Duration::hours(h) + Duration::minutes((h * 13) % 60))}));
            s = if s == cur_state { alt } else { &cur_state };
        }
        out.push(json!({"s": cur_state, "lu": ts(now - Duration::minutes(20))}));
    }
    Value::Array(out)
}

fn fake_logbook(filter: &Value) -> Value {
    let now = Utc::now();
    let ts = |mins: i64| (now - Duration::minutes(mins)).timestamp_millis() as f64 / 1000.0;
    let all = vec![
        json!({"when": ts(300), "entity_id": "automation.porch_lights_at_sunset", "name": "Porch lights at sunset", "message": "triggered by state of sun.sun", "domain": "automation"}),
        json!({"when": ts(240), "entity_id": "light.living_room", "name": "Living Room", "state": "on"}),
        json!({"when": ts(180), "entity_id": "lock.front_door", "name": "Front Door", "state": "unlocked"}),
        json!({"when": ts(178), "entity_id": "binary_sensor.front_door_contact", "name": "Front Door Contact", "state": "on"}),
        json!({"when": ts(176), "entity_id": "lock.front_door", "name": "Front Door", "state": "locked"}),
        json!({"when": ts(90), "entity_id": "media_player.living_tv", "name": "Living Room TV", "state": "playing"}),
        json!({"when": ts(30), "entity_id": "scene.movie_night", "name": "Movie Night", "message": "activated"}),
        json!({"when": ts(5), "name": "Home Assistant", "message": "started"}),
    ];
    let ids: Vec<&str> = filter
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Value::Array(
        all.into_iter()
            .filter(|e| {
                ids.is_empty() || e["entity_id"].as_str().is_some_and(|id| ids.contains(&id))
            })
            .collect(),
    )
}

fn fixture_areas() -> Value {
    json!([
        {"area_id": "living_room", "name": "Living Room"},
        {"area_id": "kitchen", "name": "Kitchen"},
        {"area_id": "bedroom", "name": "Bedroom"},
        {"area_id": "garage", "name": "Garage"},
        {"area_id": "entrance", "name": "Entrance"}
    ])
}

fn fixture_devices() -> Value {
    json!([
        {"id": "dev_tv", "area_id": "living_room", "name": "TV"},
        {"id": "dev_thermostat", "area_id": "living_room", "name": "Thermostat"},
        {"id": "dev_lock", "area_id": "entrance", "name": "Front lock"}
    ])
}

/// (entity_id, area_id, device_id, entity_category)
type RegistryRow = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

const REGISTRY: &[RegistryRow] = &[
    ("light.living_room", Some("living_room"), None, None),
    ("light.kitchen", Some("kitchen"), None, None),
    ("light.bedroom_lamp", Some("bedroom"), None, None),
    ("switch.coffee_maker", Some("kitchen"), None, None),
    ("fan.bedroom", Some("bedroom"), None, None),
    ("climate.thermostat", None, Some("dev_thermostat"), None),
    ("cover.garage_door", Some("garage"), None, None),
    ("cover.living_blinds", Some("living_room"), None, None),
    ("lock.front_door", None, Some("dev_lock"), None),
    ("media_player.living_tv", None, Some("dev_tv"), None),
    ("sensor.living_temperature", Some("living_room"), None, None),
    (
        "sensor.thermostat_signal",
        None,
        Some("dev_thermostat"),
        Some("diagnostic"),
    ),
    (
        "binary_sensor.front_door_contact",
        Some("entrance"),
        None,
        None,
    ),
];

fn fixture_registry() -> Value {
    Value::Array(
        REGISTRY
            .iter()
            .map(|(id, area, dev, cat)| {
                json!({"entity_id": id, "area_id": area, "device_id": dev, "hidden_by": null,
                       "disabled_by": null, "entity_category": cat, "platform": "mock"})
            })
            .collect(),
    )
}

fn fixture_states() -> BTreeMap<String, Value> {
    let now = Utc::now();
    let ago = |m: i64| (now - Duration::minutes(m)).to_rfc3339();
    let s = |id: &str, state: &str, attrs: Value, m: i64| {
        (
            id.to_string(),
            json!({"entity_id": id, "state": state, "attributes": attrs,
                   "last_changed": ago(m), "last_updated": ago(m),
                   "context": {"id": "c", "parent_id": null, "user_id": null}}),
        )
    };
    [
        s("light.living_room", "on", json!({"friendly_name": "Living Room", "brightness": 180, "color_temp_kelvin": 3000, "min_color_temp_kelvin": 2200, "max_color_temp_kelvin": 6500, "supported_color_modes": ["color_temp"], "color_mode": "color_temp"}), 240),
        s("light.kitchen", "off", json!({"friendly_name": "Kitchen Ceiling", "supported_color_modes": ["brightness"]}), 55),
        s("light.bedroom_lamp", "off", json!({"friendly_name": "Bedroom Lamp", "supported_color_modes": ["brightness"]}), 600),
        s("switch.coffee_maker", "off", json!({"friendly_name": "Coffee Maker"}), 420),
        s("fan.bedroom", "on", json!({"friendly_name": "Bedroom Fan", "percentage": 33, "percentage_step": 33.33}), 75),
        s("climate.thermostat", "heat", json!({"friendly_name": "Thermostat", "temperature": 21, "current_temperature": 20.4, "hvac_modes": ["off", "heat", "cool", "auto"], "min_temp": 7, "max_temp": 30, "target_temp_step": 0.5}), 800),
        s("cover.garage_door", "closed", json!({"friendly_name": "Garage Door", "device_class": "garage", "current_position": 0}), 1300),
        s("cover.living_blinds", "open", json!({"friendly_name": "Living Room Blinds", "device_class": "blind", "current_position": 60}), 200),
        s("lock.front_door", "locked", json!({"friendly_name": "Front Door"}), 176),
        s("media_player.living_tv", "playing", json!({"friendly_name": "Living Room TV", "media_title": "Planet Earth III", "volume_level": 0.3, "source": "Netflix", "source_list": ["HDMI 1", "Netflix", "YouTube"]}), 90),
        s("sensor.living_temperature", "21.3", json!({"friendly_name": "Living Room Temperature", "unit_of_measurement": "°C", "device_class": "temperature", "state_class": "measurement"}), 3),
        s("sensor.outdoor_humidity", "64", json!({"friendly_name": "Outdoor Humidity", "unit_of_measurement": "%", "device_class": "humidity"}), 12),
        s("sensor.thermostat_signal", "-61", json!({"friendly_name": "Thermostat Signal", "unit_of_measurement": "dBm"}), 1),
        s("binary_sensor.front_door_contact", "off", json!({"friendly_name": "Front Door Contact", "device_class": "door"}), 178),
        s("scene.movie_night", &ago(30), json!({"friendly_name": "Movie Night"}), 30),
        s("scene.good_morning", &ago(900), json!({"friendly_name": "Good Morning"}), 900),
        s("script.bedtime", "off", json!({"friendly_name": "Bedtime", "last_triggered": ago(1400), "mode": "single"}), 1400),
        s("automation.porch_lights_at_sunset", "on", json!({"friendly_name": "Porch lights at sunset", "last_triggered": ago(300), "mode": "single"}), 5000),
        s("automation.morning_coffee", "off", json!({"friendly_name": "Morning coffee", "last_triggered": null, "mode": "restart"}), 9000),
        s("input_number.alarm_volume", "40.0", json!({"friendly_name": "Alarm Volume", "min": 0, "max": 100, "step": 5, "mode": "slider"}), 3000),
        s("input_select.house_mode", "Home", json!({"friendly_name": "House Mode", "options": ["Home", "Away", "Night"]}), 700),
        s("sun.sun", "above_horizon", json!({"friendly_name": "Sun", "elevation": 34.2}), 400),
    ]
    .into_iter()
    .collect()
}
