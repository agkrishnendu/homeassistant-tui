//! Home Assistant WebSocket client, run as a background actor.
//!
//! The UI sends [`HaCommand`]s and receives [`HaEvent`]s; it never touches the socket.
//! The actor reconnects with exponential backoff and re-bootstraps on every connection.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tokio_tungstenite::tungstenite::Message;

use super::types::{
    Area, DeviceRegistryEntry, EntityRegistryEntry, EntityState, HistoryPoint, LogbookEntry,
    ServiceCall,
};

const PING_EVERY: Duration = Duration::from_secs(30);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub enum ConnStatus {
    Connecting,
    Connected {
        version: String,
    },
    /// Connection dropped; a retry is scheduled after `retry_in`.
    Disconnected {
        error: String,
        retry_in: Duration,
    },
    /// The token was rejected. The actor stops retrying.
    AuthFailed(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub states: Vec<EntityState>,
    pub areas: Vec<Area>,
    pub devices: Vec<DeviceRegistryEntry>,
    pub entities: Vec<EntityRegistryEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HaEvent {
    Status(ConnStatus),
    Snapshot(Snapshot),
    StateChanged {
        entity_id: String,
        new_state: Option<EntityState>,
    },
    ServiceResult {
        label: String,
        result: Result<(), String>,
    },
    History {
        entity_id: String,
        points: Vec<HistoryPoint>,
    },
    HistoryFailed {
        entity_id: String,
        error: String,
    },
    Logbook(Result<Vec<LogbookEntry>, String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum HaCommand {
    CallService {
        call: ServiceCall,
        label: String,
    },
    History {
        entity_id: String,
        since: DateTime<Utc>,
    },
    Logbook {
        since: DateTime<Utc>,
        entity_id: Option<String>,
    },
    /// Drop the connection and bootstrap again.
    Resync,
}

/// What an outstanding request id is waiting for.
enum Pending {
    States,
    Areas,
    Devices,
    Entities,
    Subscribe,
    Service(String),
    History(String),
    Logbook,
}

enum SessionEnd {
    /// The command channel closed: the UI is gone.
    Shutdown,
    AuthFailed(String),
    Resync,
    Error(String),
}

/// Run the client until the command channel closes.
pub async fn run(
    ws_url: String,
    token: String,
    mut cmds: UnboundedReceiver<HaCommand>,
    events: UnboundedSender<HaEvent>,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let _ = events.send(HaEvent::Status(ConnStatus::Connecting));
        let (end, was_connected) = session(&ws_url, &token, &mut cmds, &events).await;
        match end {
            SessionEnd::Shutdown => return,
            SessionEnd::AuthFailed(msg) => {
                let _ = events.send(HaEvent::Status(ConnStatus::AuthFailed(msg)));
                // Keep draining so the UI gets feedback, but never reconnect.
                while let Some(cmd) = cmds.recv().await {
                    reject(&cmd, &events, "authentication failed");
                }
                return;
            }
            SessionEnd::Resync => {
                backoff = BACKOFF_MIN;
                continue;
            }
            SessionEnd::Error(error) => {
                if was_connected {
                    backoff = BACKOFF_MIN;
                }
                let _ = events.send(HaEvent::Status(ConnStatus::Disconnected {
                    error,
                    retry_in: backoff,
                }));
                let deadline = Instant::now() + backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                // While waiting, fail commands immediately instead of replaying stale actions later.
                loop {
                    tokio::select! {
                        _ = sleep_until(deadline) => break,
                        cmd = cmds.recv() => match cmd {
                            None => return,
                            Some(HaCommand::Resync) => break,
                            Some(cmd) => reject(&cmd, &events, "not connected"),
                        },
                    }
                }
            }
        }
    }
}

fn reject(cmd: &HaCommand, events: &UnboundedSender<HaEvent>, why: &str) {
    let ev = match cmd {
        HaCommand::CallService { label, .. } => HaEvent::ServiceResult {
            label: label.clone(),
            result: Err(why.into()),
        },
        HaCommand::History { entity_id, .. } => HaEvent::HistoryFailed {
            entity_id: entity_id.clone(),
            error: why.into(),
        },
        HaCommand::Logbook { .. } => HaEvent::Logbook(Err(why.into())),
        HaCommand::Resync => return,
    };
    let _ = events.send(ev);
}

/// Returns why the session ended and whether authentication had succeeded.
async fn session(
    ws_url: &str,
    token: &str,
    cmds: &mut UnboundedReceiver<HaCommand>,
    events: &UnboundedSender<HaEvent>,
) -> (SessionEnd, bool) {
    let ws = match tokio_tungstenite::connect_async(ws_url).await {
        Ok((ws, _)) => ws,
        Err(e) => return (SessionEnd::Error(format!("connect: {e}")), false),
    };
    let (mut tx, mut rx) = ws.split();

    // --- Auth handshake ---------------------------------------------------
    let version = loop {
        let msg = match rx.next().await {
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Close(_))) | None => {
                return (SessionEnd::Error("closed during auth".into()), false);
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return (SessionEnd::Error(e.to_string()), false),
        };
        let v: Value = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(e) => return (SessionEnd::Error(format!("bad auth message: {e}")), false),
        };
        match v["type"].as_str() {
            Some("auth_required") => {
                let auth = json!({"type": "auth", "access_token": token}).to_string();
                if let Err(e) = tx.send(Message::text(auth)).await {
                    return (SessionEnd::Error(e.to_string()), false);
                }
            }
            Some("auth_ok") => break v["ha_version"].as_str().unwrap_or("?").to_string(),
            Some("auth_invalid") => {
                let m = v["message"].as_str().unwrap_or("invalid token").to_string();
                return (SessionEnd::AuthFailed(m), false);
            }
            _ => {}
        }
    };
    let _ = events.send(HaEvent::Status(ConnStatus::Connected { version }));

    // --- Bootstrap ----------------------------------------------------------
    let mut req = Requests::default();
    let mut snapshot = Snapshot::default();
    let mut bootstrap_left = 4;
    // Subscribe before fetching states so no change slips between the two.
    req.send(
        json!({"type": "subscribe_events", "event_type": "state_changed"}),
        Pending::Subscribe,
    );
    req.send(json!({"type": "config/area_registry/list"}), Pending::Areas);
    req.send(
        json!({"type": "config/device_registry/list"}),
        Pending::Devices,
    );
    req.send(
        json!({"type": "config/entity_registry/list"}),
        Pending::Entities,
    );
    req.send(json!({"type": "get_states"}), Pending::States);
    // Changes that arrive before `get_states` answers are replayed on top of the snapshot.
    let mut early_changes: Vec<HaEvent> = Vec::new();

    let mut ping = interval(PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping.tick().await;
    let mut ping_outstanding: Option<u64> = None;

    loop {
        for body in req.outbox.drain(..) {
            if let Err(e) = tx.send(Message::text(body.to_string())).await {
                return (SessionEnd::Error(e.to_string()), true);
            }
        }

        tokio::select! {
            msg = rx.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(frame))) => {
                        let why = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                        return (SessionEnd::Error(format!("server closed connection {why}").trim().into()), true);
                    }
                    None => return (SessionEnd::Error("connection lost".into()), true),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return (SessionEnd::Error(e.to_string()), true),
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                // HA may coalesce messages into a JSON array.
                let msgs = match v {
                    Value::Array(a) => a,
                    v => vec![v],
                };
                for v in msgs {
                    match v["type"].as_str() {
                        Some("event") => {
                            if let Some(ev) = parse_state_changed(&v) {
                                if bootstrap_left > 0 {
                                    early_changes.push(ev);
                                } else {
                                    let _ = events.send(ev);
                                }
                            }
                        }
                        Some("pong") => {
                            if v["id"].as_u64() == ping_outstanding {
                                ping_outstanding = None;
                            }
                        }
                        Some("result") => {
                            let Some(kind) = v["id"].as_u64().and_then(|id| req.pending.remove(&id)) else { continue };
                            let ok = v["success"].as_bool().unwrap_or(false);
                            let err = || {
                                v["error"]["message"].as_str().unwrap_or("request failed").to_string()
                            };
                            let result = &v["result"];
                            match kind {
                                Pending::Subscribe => {
                                    if !ok {
                                        return (SessionEnd::Error(format!("subscribe failed: {}", err())), true);
                                    }
                                }
                                Pending::States | Pending::Areas | Pending::Devices | Pending::Entities => {
                                    if !ok {
                                        return (SessionEnd::Error(format!("bootstrap failed: {}", err())), true);
                                    }
                                    let parsed = match kind {
                                        Pending::States => parse_list(result).map(|l| snapshot.states = l),
                                        Pending::Areas => parse_list(result).map(|l| snapshot.areas = l),
                                        Pending::Devices => parse_list(result).map(|l| snapshot.devices = l),
                                        _ => parse_list(result).map(|l| snapshot.entities = l),
                                    };
                                    if let Err(e) = parsed {
                                        return (SessionEnd::Error(format!("bootstrap parse: {e}")), true);
                                    }
                                    bootstrap_left -= 1;
                                    if bootstrap_left == 0 {
                                        let _ = events.send(HaEvent::Snapshot(std::mem::take(&mut snapshot)));
                                        for ev in early_changes.drain(..) {
                                            let _ = events.send(ev);
                                        }
                                    }
                                }
                                Pending::Service(label) => {
                                    let result = if ok { Ok(()) } else { Err(err()) };
                                    let _ = events.send(HaEvent::ServiceResult { label, result });
                                }
                                Pending::History(entity_id) => {
                                    let ev = if ok {
                                        let points = result[&entity_id]
                                            .as_array()
                                            .map(|a| a.iter().filter_map(HistoryPoint::from_value).collect())
                                            .unwrap_or_default();
                                        HaEvent::History { entity_id, points }
                                    } else {
                                        HaEvent::HistoryFailed { entity_id, error: err() }
                                    };
                                    let _ = events.send(ev);
                                }
                                Pending::Logbook => {
                                    let r = if ok {
                                        Ok(result
                                            .as_array()
                                            .map(|a| a.iter().filter_map(LogbookEntry::from_value).collect())
                                            .unwrap_or_default())
                                    } else {
                                        Err(err())
                                    };
                                    let _ = events.send(HaEvent::Logbook(r));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return (SessionEnd::Shutdown, true) };
                match cmd {
                    HaCommand::Resync => return (SessionEnd::Resync, true),
                    HaCommand::CallService { call, label } => {
                        let mut body = json!({
                            "type": "call_service",
                            "domain": call.domain,
                            "service": call.service,
                            "service_data": call.data,
                        });
                        if let Some(e) = call.entity_id {
                            body["target"] = json!({"entity_id": e});
                        }
                        req.send(body, Pending::Service(label));
                    }
                    HaCommand::History { entity_id, since } => {
                        req.send(json!({
                            "type": "history/history_during_period",
                            "start_time": since.to_rfc3339(),
                            "entity_ids": [entity_id],
                            "minimal_response": true,
                            "no_attributes": true,
                            "include_start_time_state": true,
                            "significant_changes_only": false,
                        }), Pending::History(entity_id));
                    }
                    HaCommand::Logbook { since, entity_id } => {
                        let mut body = json!({
                            "type": "logbook/get_events",
                            "start_time": since.to_rfc3339(),
                            "end_time": Utc::now().to_rfc3339(),
                        });
                        if let Some(e) = entity_id {
                            body["entity_ids"] = json!([e]);
                        }
                        req.send(body, Pending::Logbook);
                    }
                }
            }
            _ = ping.tick() => {
                if ping_outstanding.is_some() {
                    return (SessionEnd::Error("ping timed out".into()), true);
                }
                ping_outstanding = Some(req.send_untracked(json!({"type": "ping"})));
            }
        }
    }
}

#[derive(Default)]
struct Requests {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    outbox: Vec<Value>,
}

impl Requests {
    fn send(&mut self, body: Value, kind: Pending) -> u64 {
        let id = self.send_untracked(body);
        self.pending.insert(id, kind);
        id
    }

    fn send_untracked(&mut self, mut body: Value) -> u64 {
        self.next_id += 1;
        body["id"] = self.next_id.into();
        self.outbox.push(body);
        self.next_id
    }
}

fn parse_list<T: serde::de::DeserializeOwned>(v: &Value) -> Result<Vec<T>, serde_json::Error> {
    // Parse item by item so a single odd entry doesn't sink the whole bootstrap.
    let Some(items) = v.as_array() else {
        return serde_json::from_value(v.clone());
    };
    Ok(items
        .iter()
        .filter_map(|i| serde_json::from_value(i.clone()).ok())
        .collect())
}

fn parse_state_changed(v: &Value) -> Option<HaEvent> {
    let ev = &v["event"];
    if ev["event_type"].as_str()? != "state_changed" {
        return None;
    }
    let data = &ev["data"];
    let entity_id = data["entity_id"].as_str()?.to_string();
    let new_state = serde_json::from_value(data["new_state"].clone()).ok();
    Some(HaEvent::StateChanged {
        entity_id,
        new_state,
    })
}
