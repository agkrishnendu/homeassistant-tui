//! Home Assistant WebSocket client, run as a background actor.
//!
//! The UI sends [`HaCommand`]s and receives [`HaEvent`]s; it never touches the socket.
//! The actor reconnects with exponential backoff and re-bootstraps on every connection.
//! Changes to the area, device and entity registries are pushed by Home Assistant as events;
//! the affected list is then refetched (coalescing bursts) and sent as [`HaEvent::Registry`].
//!
//! Every command gets exactly one outcome. Commands are never held back and replayed on a
//! later connection: while disconnected, connecting or authenticating they are rejected, and
//! service calls are also rejected until the state mirror is loaded. Requests still in
//! flight when a connection ends, or that go unanswered for too long, are reported as
//! failed. For service calls the outcome is then unknown, since Home Assistant may already
//! have run them.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::types::{
    Area, DeviceRegistryEntry, EntityRegistryEntry, EntityState, HistoryPoint, LogbookEntry,
    ServiceCall,
};

const PING_EVERY: Duration = Duration::from_secs(30);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Bulk edits send a burst of registry events; refetch once, this long after the first.
const REGISTRY_DEBOUNCE: Duration = Duration::from_millis(300);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsTx = SplitSink<Ws, Message>;
type WsRx = SplitStream<Ws>;

/// How long to wait on Home Assistant before giving up.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Opening the connection plus the auth handshake.
    pub connect: Duration,
    /// The reply to a single request.
    pub request: Duration,
    /// Writing a single message to the socket.
    pub write: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            request: Duration::from_secs(30),
            write: Duration::from_secs(10),
        }
    }
}

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

/// A registry list refetched after Home Assistant reported a change to it.
#[derive(Debug, Clone, PartialEq)]
pub enum RegistryUpdate {
    Areas(Vec<Area>),
    Devices(Vec<DeviceRegistryEntry>),
    Entities(Vec<EntityRegistryEntry>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum HaEvent {
    Status(ConnStatus),
    Snapshot(Snapshot),
    Registry(RegistryUpdate),
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
    Logbook {
        /// The entity filter the request was made with.
        entity_id: Option<String>,
        result: Result<Vec<LogbookEntry>, String>,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Registry {
    Areas,
    Devices,
    Entities,
}

impl Registry {
    const ALL: [Registry; 3] = [Registry::Areas, Registry::Devices, Registry::Entities];

    fn list_type(self) -> &'static str {
        match self {
            Registry::Areas => "config/area_registry/list",
            Registry::Devices => "config/device_registry/list",
            Registry::Entities => "config/entity_registry/list",
        }
    }

    fn updated_event(self) -> &'static str {
        match self {
            Registry::Areas => "area_registry_updated",
            Registry::Devices => "device_registry_updated",
            Registry::Entities => "entity_registry_updated",
        }
    }
}

/// What an outstanding request id is waiting for.
enum Pending {
    States,
    Areas,
    Devices,
    Entities,
    Subscribe,
    /// A registry-change subscription. Optional: a refusal only means no live updates.
    SubscribeRegistry,
    /// A refetch of one registry after a change event.
    Refresh(Registry),
    Service(String),
    History(String),
    Logbook(Option<String>),
}

impl Pending {
    fn is_bootstrap(&self) -> bool {
        matches!(
            self,
            Pending::States
                | Pending::Areas
                | Pending::Devices
                | Pending::Entities
                | Pending::Subscribe
        )
    }
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
    cmds: UnboundedReceiver<HaCommand>,
    events: UnboundedSender<HaEvent>,
) {
    run_with(ws_url, token, Timeouts::default(), cmds, events).await
}

/// [`run`] with custom timeouts.
pub async fn run_with(
    ws_url: String,
    token: String,
    timeouts: Timeouts,
    mut cmds: UnboundedReceiver<HaCommand>,
    events: UnboundedSender<HaEvent>,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let _ = events.send(HaEvent::Status(ConnStatus::Connecting));
        let (end, was_connected) = session(&ws_url, &token, timeouts, &mut cmds, &events).await;
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
        HaCommand::Logbook { entity_id, .. } => HaEvent::Logbook {
            entity_id: entity_id.clone(),
            result: Err(why.into()),
        },
        HaCommand::Resync => return,
    };
    let _ = events.send(ev);
}

/// Report a request that will never get its reply.
fn fail(kind: Pending, events: &UnboundedSender<HaEvent>, why: &str) {
    let ev = match kind {
        Pending::Service(label) => HaEvent::ServiceResult {
            label,
            result: Err(format!("{why}; it may or may not have run")),
        },
        Pending::History(entity_id) => HaEvent::HistoryFailed {
            entity_id,
            error: why.into(),
        },
        Pending::Logbook(entity_id) => HaEvent::Logbook {
            entity_id,
            result: Err(why.into()),
        },
        _ => return,
    };
    let _ = events.send(ev);
}

async fn write(tx: &mut WsTx, body: String, within: Duration) -> Result<(), String> {
    match timeout(within, tx.send(Message::text(body))).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("timed out writing to Home Assistant".into()),
    }
}

/// Returns why the session ended and whether authentication had succeeded.
async fn session(
    ws_url: &str,
    token: &str,
    t: Timeouts,
    cmds: &mut UnboundedReceiver<HaCommand>,
    events: &UnboundedSender<HaEvent>,
) -> (SessionEnd, bool) {
    // Keep answering commands while connecting: anything left queued would run whenever a
    // slow connection finally completes, possibly minutes after the key press.
    let handshake = timeout(t.connect, handshake(ws_url, token, t.write));
    tokio::pin!(handshake);
    let (mut tx, mut rx, version) = loop {
        tokio::select! {
            r = &mut handshake => match r {
                Ok(Ok(conn)) => break conn,
                Ok(Err(end)) => return (end, false),
                Err(_) => return (SessionEnd::Error("timed out connecting".into()), false),
            },
            cmd = cmds.recv() => match cmd {
                None => return (SessionEnd::Shutdown, false),
                Some(HaCommand::Resync) => {}
                Some(cmd) => reject(&cmd, events, "not connected"),
            },
        }
    };
    let _ = events.send(HaEvent::Status(ConnStatus::Connected { version }));

    let mut req = Requests::default();
    let end = serve(&mut tx, &mut rx, &mut req, t, cmds, events).await;
    let why = match end {
        SessionEnd::Resync => "cancelled by resync",
        _ => "connection lost",
    };
    for (kind, _) in req.pending.into_values() {
        fail(kind, events, why);
    }
    (end, true)
}

/// Open the socket and authenticate. Returns the socket halves and the HA version.
async fn handshake(
    ws_url: &str,
    token: &str,
    write_timeout: Duration,
) -> Result<(WsTx, WsRx, String), SessionEnd> {
    let (ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| SessionEnd::Error(format!("connect: {e}")))?;
    let (mut tx, mut rx) = ws.split();
    loop {
        let msg = match rx.next().await {
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Close(_))) | None => {
                return Err(SessionEnd::Error("closed during auth".into()));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(SessionEnd::Error(e.to_string())),
        };
        let v: Value = serde_json::from_str(&msg)
            .map_err(|e| SessionEnd::Error(format!("bad auth message: {e}")))?;
        match v["type"].as_str() {
            Some("auth_required") => {
                let auth = json!({"type": "auth", "access_token": token}).to_string();
                write(&mut tx, auth, write_timeout)
                    .await
                    .map_err(SessionEnd::Error)?;
            }
            Some("auth_ok") => {
                let version = v["ha_version"].as_str().unwrap_or("?").to_string();
                return Ok((tx, rx, version));
            }
            Some("auth_invalid") => {
                let m = v["message"].as_str().unwrap_or("invalid token").to_string();
                return Err(SessionEnd::AuthFailed(m));
            }
            _ => {}
        }
    }
}

/// Bootstrap, then relay commands and events until the connection ends. Requests still
/// pending on return are left in `req` for the caller to fail.
async fn serve(
    tx: &mut WsTx,
    rx: &mut WsRx,
    req: &mut Requests,
    t: Timeouts,
    cmds: &mut UnboundedReceiver<HaCommand>,
    events: &UnboundedSender<HaEvent>,
) -> SessionEnd {
    // --- Bootstrap ----------------------------------------------------------
    let mut snapshot = Snapshot::default();
    let mut bootstrap_left = 4;
    // Subscribe before fetching states so no change slips between the two.
    req.send(
        json!({"type": "subscribe_events", "event_type": "state_changed"}),
        Pending::Subscribe,
    );
    for registry in Registry::ALL {
        req.send(
            json!({"type": "subscribe_events", "event_type": registry.updated_event()}),
            Pending::SubscribeRegistry,
        );
    }
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
    // Registries reported changed and not yet refetched, and when to refetch them. Nothing is
    // refetched before the snapshot is in, or a reply could be overwritten by older data.
    let mut stale: Vec<Registry> = Vec::new();
    let mut refresh_at: Option<Instant> = None;

    let mut ping = interval(PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping.tick().await;
    let mut ping_outstanding: Option<u64> = None;
    let mut sweep = interval(Duration::from_secs(1));
    sweep.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        for body in req.outbox.drain(..) {
            if let Err(e) = write(tx, body.to_string(), t.write).await {
                return SessionEnd::Error(e);
            }
        }

        tokio::select! {
            msg = rx.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(frame))) => {
                        let why = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                        return SessionEnd::Error(format!("server closed connection {why}").trim().into());
                    }
                    None => return SessionEnd::Error("connection lost".into()),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return SessionEnd::Error(e.to_string()),
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
                            } else if let Some(registry) = parse_registry_event(&v) {
                                if !stale.contains(&registry) {
                                    stale.push(registry);
                                }
                                refresh_at.get_or_insert_with(|| Instant::now() + REGISTRY_DEBOUNCE);
                            }
                        }
                        Some("pong") => {
                            if v["id"].as_u64() == ping_outstanding {
                                ping_outstanding = None;
                            }
                        }
                        Some("result") => {
                            let Some((kind, _)) = v["id"].as_u64().and_then(|id| req.pending.remove(&id)) else { continue };
                            let ok = v["success"].as_bool().unwrap_or(false);
                            let err = || {
                                v["error"]["message"].as_str().unwrap_or("request failed").to_string()
                            };
                            let result = &v["result"];
                            match kind {
                                Pending::Subscribe => {
                                    if !ok {
                                        return SessionEnd::Error(format!("subscribe failed: {}", err()));
                                    }
                                }
                                Pending::SubscribeRegistry => {}
                                Pending::Refresh(registry) => {
                                    // A failed refetch keeps the previous data; the next change
                                    // event or a manual resync brings it up to date.
                                    if !ok {
                                        continue;
                                    }
                                    let update = match registry {
                                        Registry::Areas => parse_list(result).map(RegistryUpdate::Areas),
                                        Registry::Devices => parse_list(result).map(RegistryUpdate::Devices),
                                        Registry::Entities => parse_list(result).map(RegistryUpdate::Entities),
                                    };
                                    if let Ok(update) = update {
                                        let _ = events.send(HaEvent::Registry(update));
                                    }
                                }
                                Pending::States | Pending::Areas | Pending::Devices | Pending::Entities => {
                                    if !ok {
                                        return SessionEnd::Error(format!("bootstrap failed: {}", err()));
                                    }
                                    let parsed = match kind {
                                        Pending::States => parse_list(result).map(|l| snapshot.states = l),
                                        Pending::Areas => parse_list(result).map(|l| snapshot.areas = l),
                                        Pending::Devices => parse_list(result).map(|l| snapshot.devices = l),
                                        _ => parse_list(result).map(|l| snapshot.entities = l),
                                    };
                                    if let Err(e) = parsed {
                                        return SessionEnd::Error(format!("bootstrap parse: {e}"));
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
                                Pending::Logbook(entity_id) => {
                                    let result = if ok {
                                        Ok(result
                                            .as_array()
                                            .map(|a| a.iter().filter_map(LogbookEntry::from_value).collect())
                                            .unwrap_or_default())
                                    } else {
                                        Err(err())
                                    };
                                    let _ = events.send(HaEvent::Logbook { entity_id, result });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return SessionEnd::Shutdown };
                // Until the snapshot is in, the UI still shows the previous session's states.
                if bootstrap_left > 0 && matches!(cmd, HaCommand::CallService { .. }) {
                    reject(&cmd, events, "still loading from Home Assistant");
                    continue;
                }
                match cmd {
                    HaCommand::Resync => return SessionEnd::Resync,
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
                        if let Some(e) = &entity_id {
                            body["entity_ids"] = json!([e]);
                        }
                        req.send(body, Pending::Logbook(entity_id));
                    }
                }
            }
            _ = sleep_until(refresh_at.unwrap_or_else(Instant::now)), if refresh_at.is_some() && bootstrap_left == 0 => {
                refresh_at = None;
                for registry in stale.drain(..) {
                    req.send(json!({"type": registry.list_type()}), Pending::Refresh(registry));
                }
            }
            _ = ping.tick() => {
                if ping_outstanding.is_some() {
                    return SessionEnd::Error("ping timed out".into());
                }
                ping_outstanding = Some(req.send_untracked(json!({"type": "ping"})));
            }
            _ = sweep.tick() => {
                for kind in req.expired(t.request) {
                    if kind.is_bootstrap() {
                        return SessionEnd::Error("timed out loading from Home Assistant".into());
                    }
                    fail(kind, events, "no reply from Home Assistant");
                }
            }
        }
    }
}

#[derive(Default)]
struct Requests {
    next_id: u64,
    /// What each outstanding request is waiting for, and when it was sent.
    pending: HashMap<u64, (Pending, Instant)>,
    outbox: Vec<Value>,
}

impl Requests {
    fn send(&mut self, body: Value, kind: Pending) -> u64 {
        let id = self.send_untracked(body);
        self.pending.insert(id, (kind, Instant::now()));
        id
    }

    /// Remove and return the requests sent more than `after` ago.
    fn expired(&mut self, after: Duration) -> Vec<Pending> {
        let now = Instant::now();
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, (_, at))| now - *at >= after)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| self.pending.remove(&id).map(|(kind, _)| kind))
            .collect()
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
    // Only an explicit null means the entity was removed. A malformed update is dropped, so
    // the entity keeps its last known state instead of disappearing.
    let new_state = match data.get("new_state")? {
        Value::Null => None,
        v => Some(serde_json::from_value(v.clone()).ok()?),
    };
    Some(HaEvent::StateChanged {
        entity_id,
        new_state,
    })
}

/// Which registry a `*_registry_updated` event says changed.
fn parse_registry_event(v: &Value) -> Option<Registry> {
    let event_type = v["event"]["event_type"].as_str()?;
    Registry::ALL
        .into_iter()
        .find(|r| r.updated_event() == event_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(new_state: Option<Value>) -> Value {
        let mut data = json!({"entity_id": "light.x"});
        if let Some(ns) = new_state {
            data["new_state"] = ns;
        }
        json!({"type": "event", "event": {"event_type": "state_changed", "data": data}})
    }

    #[test]
    fn state_changed_null_is_removal_and_malformed_is_dropped() {
        let removed = parse_state_changed(&event(Some(Value::Null)));
        assert!(matches!(
            removed,
            Some(HaEvent::StateChanged {
                new_state: None,
                ..
            })
        ));
        let updated =
            parse_state_changed(&event(Some(json!({"entity_id": "light.x", "state": "on"}))));
        assert!(matches!(
            updated,
            Some(HaEvent::StateChanged {
                new_state: Some(_),
                ..
            })
        ));
        assert_eq!(parse_state_changed(&event(Some(json!({"state": 5})))), None);
        assert_eq!(parse_state_changed(&event(Some(json!("garbage")))), None);
        assert_eq!(parse_state_changed(&event(None)), None);
    }

    #[test]
    fn registry_events_are_recognised() {
        let ev = |t: &str| json!({"type": "event", "event": {"event_type": t, "data": {}}});
        assert_eq!(
            parse_registry_event(&ev("area_registry_updated")),
            Some(Registry::Areas)
        );
        assert_eq!(
            parse_registry_event(&ev("device_registry_updated")),
            Some(Registry::Devices)
        );
        assert_eq!(
            parse_registry_event(&ev("entity_registry_updated")),
            Some(Registry::Entities)
        );
        assert_eq!(parse_registry_event(&ev("state_changed")), None);
        assert_eq!(parse_registry_event(&json!({"type": "event"})), None);
    }

    #[test]
    fn expired_requests_are_removed() {
        let mut req = Requests::default();
        req.send(json!({}), Pending::Logbook(None));
        assert!(req.expired(Duration::from_secs(60)).is_empty());
        assert_eq!(req.expired(Duration::ZERO).len(), 1);
        assert!(req.pending.is_empty());
    }
}
