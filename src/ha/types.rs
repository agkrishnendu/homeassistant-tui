use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Attributes = Map<String, Value>;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EntityState {
    pub entity_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: Attributes,
    pub last_changed: Option<DateTime<Utc>>,
    pub last_updated: Option<DateTime<Utc>>,
}

impl EntityState {
    pub fn domain(&self) -> &str {
        domain_of(&self.entity_id)
    }

    pub fn friendly_name(&self) -> &str {
        self.attributes
            .get("friendly_name")
            .and_then(Value::as_str)
            .unwrap_or(&self.entity_id)
    }

    pub fn attr_f64(&self, key: &str) -> Option<f64> {
        self.attributes.get(key).and_then(Value::as_f64)
    }

    pub fn attr_str(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).and_then(Value::as_str)
    }

    pub fn attr_list(&self, key: &str) -> Vec<&str> {
        self.attributes
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    pub fn unit(&self) -> Option<&str> {
        self.attr_str("unit_of_measurement")
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self.state.as_str(), "unavailable" | "unknown")
    }
}

pub fn domain_of(entity_id: &str) -> &str {
    entity_id
        .split_once('.')
        .map(|(d, _)| d)
        .unwrap_or(entity_id)
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Area {
    pub area_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DeviceRegistryEntry {
    pub id: String,
    pub area_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EntityRegistryEntry {
    pub entity_id: String,
    pub area_id: Option<String>,
    pub device_id: Option<String>,
    pub hidden_by: Option<String>,
    pub disabled_by: Option<String>,
    pub entity_category: Option<String>,
    /// Integration that provides the entity, e.g. `hue`, `backup`.
    #[serde(default)]
    pub platform: Option<String>,
}

/// One point from `history/history_during_period` with `minimal_response`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPoint {
    pub when: DateTime<Utc>,
    pub state: String,
}

impl HistoryPoint {
    /// Parse an entry in either the compressed (`s`/`lu`) or the full form.
    pub fn from_value(v: &Value) -> Option<Self> {
        let state = v
            .get("s")
            .or_else(|| v.get("state"))
            .and_then(Value::as_str)?
            .to_string();
        let when = ts_field(v, &["lu", "lc", "last_updated", "last_changed"])?;
        Some(Self { when, state })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogbookEntry {
    pub when: DateTime<Utc>,
    pub entity_id: Option<String>,
    pub name: Option<String>,
    pub message: Option<String>,
    pub state: Option<String>,
}

impl LogbookEntry {
    pub fn from_value(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            when: ts_field(v, &["when"])?,
            entity_id: s("entity_id"),
            name: s("name"),
            message: s("message"),
            state: s("state"),
        })
    }

    pub fn describe(&self) -> String {
        match (&self.message, &self.state) {
            (Some(m), _) => m.clone(),
            (None, Some(s)) => format!("changed to {s}"),
            (None, None) => String::new(),
        }
    }
}

/// Read the first present timestamp field; HA sends either epoch floats or ISO strings.
fn ts_field(v: &Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter().find_map(|k| match v.get(*k)? {
        Value::Number(n) => {
            let secs = n.as_f64()?;
            Utc.timestamp_micros((secs * 1_000_000.0) as i64).single()
        }
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc)),
        _ => None,
    })
}

/// A Home Assistant service invocation targeting (at most) one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    pub domain: String,
    pub service: String,
    pub data: Map<String, Value>,
    pub entity_id: Option<String>,
}

impl ServiceCall {
    pub fn new(domain: &str, service: &str, entity_id: &str) -> Self {
        Self {
            domain: domain.into(),
            service: service.into(),
            data: Map::new(),
            entity_id: Some(entity_id.into()),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.data.insert(key.into(), value.into());
        self
    }

    pub fn name(&self) -> String {
        format!("{}.{}", self.domain, self.service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_state() {
        let s: EntityState = serde_json::from_value(json!({
            "entity_id": "light.kitchen",
            "state": "on",
            "attributes": {"friendly_name": "Kitchen", "brightness": 128},
            "last_changed": "2026-09-19T08:00:00.123456+00:00",
            "last_updated": "2026-09-19T08:00:00.123456+00:00",
            "context": {"id": "x", "parent_id": null, "user_id": null}
        }))
        .unwrap();
        assert_eq!(s.domain(), "light");
        assert_eq!(s.friendly_name(), "Kitchen");
        assert_eq!(s.attr_f64("brightness"), Some(128.0));
        assert!(s.last_changed.is_some());
    }

    #[test]
    fn parses_history_and_logbook() {
        let p = HistoryPoint::from_value(&json!({"s": "21.5", "lu": 1_758_268_800.5})).unwrap();
        assert_eq!(p.state, "21.5");
        assert_eq!(p.when.timestamp(), 1_758_268_800);
        let l = LogbookEntry::from_value(
            &json!({"when": 1_758_268_800.0, "entity_id": "light.a", "name": "A", "state": "off"}),
        )
        .unwrap();
        assert_eq!(l.describe(), "changed to off");
    }
}
