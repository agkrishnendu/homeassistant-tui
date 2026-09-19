//! End-to-end tests of the WebSocket client against the mock Home Assistant.

mod support;

use std::time::Duration;

use chrono::Utc;
use homeassistant_tui::config::ws_url;
use homeassistant_tui::ha::client::{self, ConnStatus, HaCommand, HaEvent};
use homeassistant_tui::ha::types::ServiceCall;
use support::mock::MockHa;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const TOKEN: &str = "test-token";

async fn connect(
    ha: &MockHa,
    token: &str,
) -> (UnboundedSender<HaCommand>, UnboundedReceiver<HaEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(client::run(ws_url(&ha.url()), token.into(), cmd_rx, ev_tx));
    (cmd_tx, ev_rx)
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
        HaEvent::Logbook(r) => Some(r),
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
