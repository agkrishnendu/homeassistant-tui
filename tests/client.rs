//! End-to-end tests of the WebSocket client against the mock Home Assistant.

mod support;

use std::time::Duration;

use chrono::Utc;
use homeassistant_tui::config::ws_url;
use homeassistant_tui::ha::client::{
    self, ConnStatus, HaCommand, HaEvent, RegistryUpdate, Timeouts,
};
use homeassistant_tui::ha::types::ServiceCall;
use support::mock::MockHa;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const TOKEN: &str = "test-token";

async fn connect(
    ha: &MockHa,
    token: &str,
) -> (UnboundedSender<HaCommand>, UnboundedReceiver<HaEvent>) {
    connect_with(ha, token, Timeouts::default()).await
}

async fn connect_with(
    ha: &MockHa,
    token: &str,
    timeouts: Timeouts,
) -> (UnboundedSender<HaCommand>, UnboundedReceiver<HaEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(client::run_with(
        ws_url(&ha.url()),
        token.into(),
        timeouts,
        cmd_rx,
        ev_tx,
    ));
    (cmd_tx, ev_rx)
}

fn toggle_kitchen() -> HaCommand {
    HaCommand::CallService {
        call: ServiceCall::new("light", "toggle", "light.kitchen"),
        label: "toggle kitchen".into(),
    }
}

async fn service_error(rx: &mut UnboundedReceiver<HaEvent>) -> String {
    wait_for(rx, |e| match e {
        HaEvent::ServiceResult { result: Err(e), .. } => Some(e),
        HaEvent::ServiceResult { result: Ok(()), .. } => panic!("service call succeeded"),
        _ => None,
    })
    .await
}

/// Wait for the first event matching `pred`, skipping others.
async fn wait_for<T>(
    rx: &mut UnboundedReceiver<HaEvent>,
    mut pred: impl FnMut(HaEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = rx.recv().await.expect("client stopped");
            if let Some(t) = pred(ev) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

async fn ready(ha: &MockHa) -> (UnboundedSender<HaCommand>, UnboundedReceiver<HaEvent>) {
    let (tx, mut rx) = connect(ha, TOKEN).await;
    wait_for(&mut rx, |e| matches!(e, HaEvent::Snapshot(_)).then_some(())).await;
    (tx, rx)
}

#[tokio::test]
async fn bootstrap_delivers_snapshot() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (_tx, mut rx) = connect(&ha, TOKEN).await;
    let version = wait_for(&mut rx, |e| match e {
        HaEvent::Status(ConnStatus::Connected { version }) => Some(version),
        _ => None,
    })
    .await;
    assert_eq!(version, support::mock::VERSION);
    let snap = wait_for(&mut rx, |e| match e {
        HaEvent::Snapshot(s) => Some(s),
        _ => None,
    })
    .await;
    assert!(snap.states.len() > 15);
    assert_eq!(snap.areas.len(), 5);
    assert!(
        snap.entities
            .iter()
            .any(|e| e.entity_id == "lock.front_door" && e.device_id.is_some())
    );
}

#[tokio::test]
async fn registry_changes_are_refetched_and_coalesced() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (_tx, mut rx) = ready(&ha).await;
    assert_eq!(ha.count_received("config/entity_registry/list"), 1);

    // A burst of edits to one registry becomes a single refetch.
    for area in ["bedroom", "garage", "entrance", "kitchen", "bedroom"] {
        ha.set_entity_area("light.kitchen", Some(area));
    }
    ha.rename_area("bedroom", "Master Bedroom");

    let mut entities = None;
    let mut areas = None;
    while entities.is_none() || areas.is_none() {
        match wait_for(&mut rx, |e| match e {
            HaEvent::Registry(u) => Some(u),
            _ => None,
        })
        .await
        {
            RegistryUpdate::Entities(l) => entities = Some(l),
            RegistryUpdate::Areas(l) => areas = Some(l),
            RegistryUpdate::Devices(_) => panic!("devices did not change"),
        }
    }
    let kitchen = entities
        .unwrap()
        .into_iter()
        .find(|e| e.entity_id == "light.kitchen")
        .unwrap();
    assert_eq!(kitchen.area_id.as_deref(), Some("bedroom"));
    assert!(
        areas
            .unwrap()
            .iter()
            .any(|a| a.area_id == "bedroom" && a.name == "Master Bedroom")
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(ha.count_received("config/entity_registry/list"), 2);
    assert_eq!(ha.count_received("config/area_registry/list"), 2);
    assert_eq!(ha.count_received("config/device_registry/list"), 1);
}

#[tokio::test]
async fn registry_refetch_waits_for_the_snapshot() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    // Hold the bootstrap open (get_states never answers) and change a registry meanwhile.
    ha.silence("get_states");
    let (_tx, _rx) = connect(&ha, TOKEN).await;
    while ha.count_received("get_states") == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ha.set_entity_area("light.kitchen", None);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        ha.count_received("config/entity_registry/list"),
        1,
        "no refetch before the snapshot is in"
    );
}

#[tokio::test]
async fn service_call_round_trip() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (tx, mut rx) = ready(&ha).await;

    tx.send(HaCommand::CallService {
        call: ServiceCall::new("light", "toggle", "light.kitchen"),
        label: "toggle kitchen".into(),
    })
    .unwrap();
    let (result, new_state) = {
        let mut result = None;
        let mut new_state = None;
        while result.is_none() || new_state.is_none() {
            match wait_for(&mut rx, Some).await {
                HaEvent::ServiceResult { label, result: r } => {
                    assert_eq!(label, "toggle kitchen");
                    result = Some(r);
                }
                HaEvent::StateChanged {
                    entity_id,
                    new_state: s,
                } if entity_id == "light.kitchen" => new_state = s,
                _ => {}
            }
        }
        (result.unwrap(), new_state.unwrap())
    };
    assert_eq!(result, Ok(()));
    assert_eq!(new_state.state, "on");
    assert_eq!(ha.calls()[0].0, "light.toggle");

    tx.send(HaCommand::CallService {
        call: ServiceCall::new("light", "explode", "light.kitchen"),
        label: "bad".into(),
    })
    .unwrap();
    let err = wait_for(&mut rx, |e| match e {
        HaEvent::ServiceResult { result: Err(e), .. } => Some(e),
        _ => None,
    })
    .await;
    assert!(err.contains("not found"), "{err}");
}

#[tokio::test]
async fn external_changes_are_pushed() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (_tx, mut rx) = ready(&ha).await;
    ha.set_state("binary_sensor.front_door_contact", "on");
    let s = wait_for(&mut rx, |e| match e {
        HaEvent::StateChanged {
            new_state: Some(s), ..
        } => Some(s),
        _ => None,
    })
    .await;
    assert_eq!(s.entity_id, "binary_sensor.front_door_contact");
    assert_eq!(s.state, "on");
}

#[tokio::test]
async fn history_and_logbook() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (tx, mut rx) = ready(&ha).await;
    let since = Utc::now() - chrono::Duration::hours(24);
    tx.send(HaCommand::History {
        entity_id: "sensor.living_temperature".into(),
        since,
    })
    .unwrap();
    let points = wait_for(&mut rx, |e| match e {
        HaEvent::History { points, .. } => Some(points),
        _ => None,
    })
    .await;
    assert!(points.len() > 90);
    assert!(points[0].state.parse::<f64>().is_ok());

    tx.send(HaCommand::Logbook {
        since,
        entity_id: Some("lock.front_door".into()),
    })
    .unwrap();
    let entries = wait_for(&mut rx, |e| match e {
        HaEvent::Logbook { result, .. } => Some(result),
        _ => None,
    })
    .await
    .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|e| e.entity_id.as_deref() == Some("lock.front_door"))
    );
}

#[tokio::test]
async fn reconnects_after_drop() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (_tx, mut rx) = ready(&ha).await;
    ha.kick_all();
    wait_for(&mut rx, |e| {
        matches!(e, HaEvent::Status(ConnStatus::Disconnected { .. })).then_some(())
    })
    .await;
    wait_for(&mut rx, |e| {
        matches!(e, HaEvent::Status(ConnStatus::Connected { .. })).then_some(())
    })
    .await;
    wait_for(&mut rx, |e| matches!(e, HaEvent::Snapshot(_)).then_some(())).await;
    // The new subscription works.
    ha.set_state("light.kitchen", "on");
    wait_for(&mut rx, |e| {
        matches!(e, HaEvent::StateChanged { .. }).then_some(())
    })
    .await;
}

#[tokio::test]
async fn bad_token_stops_and_rejects_commands() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (tx, mut rx) = connect(&ha, "wrong").await;
    let msg = wait_for(&mut rx, |e| match e {
        HaEvent::Status(ConnStatus::AuthFailed(m)) => Some(m),
        _ => None,
    })
    .await;
    assert!(msg.contains("Invalid"));
    tx.send(HaCommand::CallService {
        call: ServiceCall::new("light", "toggle", "light.kitchen"),
        label: "x".into(),
    })
    .unwrap();
    let r = wait_for(&mut rx, |e| match e {
        HaEvent::ServiceResult { result, .. } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.is_err());
    assert!(ha.calls().is_empty());
}

#[tokio::test]
async fn commands_during_handshake_are_rejected_not_replayed() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    ha.delay_auth(Duration::from_millis(500));
    let (tx, mut rx) = connect(&ha, TOKEN).await;
    tx.send(toggle_kitchen()).unwrap();
    let err = service_error(&mut rx).await;
    assert!(err.contains("not connected"), "{err}");
    wait_for(&mut rx, |e| matches!(e, HaEvent::Snapshot(_)).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(ha.calls().is_empty(), "stale command was replayed");
}

#[tokio::test]
async fn in_flight_requests_fail_when_connection_drops() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let (tx, mut rx) = ready(&ha).await;
    ha.silence("call_service");
    ha.silence("history/history_during_period");
    tx.send(toggle_kitchen()).unwrap();
    tx.send(HaCommand::History {
        entity_id: "sensor.living_temperature".into(),
        since: Utc::now(),
    })
    .unwrap();
    // Let both requests reach the server before dropping the connection.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ha.received().iter().any(|t| t.starts_with("history/")) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("requests never reached the mock");
    ha.kick_all();
    // Both get an outcome, in either order.
    let (mut service, mut history) = (None, false);
    while service.is_none() || !history {
        match wait_for(&mut rx, Some).await {
            HaEvent::ServiceResult { result, .. } => service = Some(result.unwrap_err()),
            HaEvent::HistoryFailed { entity_id, .. } => {
                history = entity_id == "sensor.living_temperature"
            }
            _ => {}
        }
    }
    let err = service.unwrap();
    assert!(err.contains("may or may not have run"), "{err}");
}

#[tokio::test]
async fn unanswered_request_times_out() {
    let ha = MockHa::start(TOKEN, "127.0.0.1:0").await;
    let timeouts = Timeouts {
        request: Duration::from_millis(500),
        ..Timeouts::default()
    };
    let (tx, mut rx) = connect_with(&ha, TOKEN, timeouts).await;
    wait_for(&mut rx, |e| matches!(e, HaEvent::Snapshot(_)).then_some(())).await;
    ha.silence("logbook/get_events");
    tx.send(HaCommand::Logbook {
        since: Utc::now(),
        entity_id: Some("lock.front_door".into()),
    })
    .unwrap();
    let (entity_id, result) = wait_for(&mut rx, |e| match e {
        HaEvent::Logbook { entity_id, result } => Some((entity_id, result)),
        _ => None,
    })
    .await;
    assert_eq!(entity_id.as_deref(), Some("lock.front_door"));
    assert!(result.unwrap_err().contains("no reply"));
    // The connection itself is still fine.
    tx.send(toggle_kitchen()).unwrap();
    wait_for(&mut rx, |e| match e {
        HaEvent::ServiceResult { result, .. } => Some(result),
        _ => None,
    })
    .await
    .unwrap();
}
