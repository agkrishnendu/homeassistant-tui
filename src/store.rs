//! In-memory mirror of Home Assistant state plus the registries needed to group it.

use std::collections::HashMap;

use crate::ha::client::{RegistryUpdate, Snapshot};
use crate::ha::types::{Area, DeviceRegistryEntry, EntityRegistryEntry, EntityState, domain_of};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupBy {
    #[default]
    Area,
    Domain,
}

impl GroupBy {
    pub fn toggle(self) -> Self {
        match self {
            GroupBy::Area => GroupBy::Domain,
            GroupBy::Domain => GroupBy::Area,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GroupBy::Area => "Areas",
            GroupBy::Domain => "Domains",
        }
    }
}

/// A sidebar group. `key == None` is the "All" group.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub key: Option<String>,
    pub title: String,
    pub count: usize,
}

pub const UNASSIGNED: &str = "Unassigned";

/// Housekeeping domains and integrations that clutter a device list.
const SYSTEM_DOMAINS: &[&str] = &[
    "ai_task",
    "conversation",
    "event",
    "stt",
    "tts",
    "update",
    "wake_word",
    "zone",
];
const SYSTEM_PLATFORMS: &[&str] = &["backup"];

#[derive(Debug, Default)]
pub struct Store {
    pub entities: HashMap<String, EntityState>,
    areas: HashMap<String, Area>,
    devices: HashMap<String, DeviceRegistryEntry>,
    registry: HashMap<String, EntityRegistryEntry>,
    pub loaded: bool,
    /// Also list hidden, config/diagnostic and system entities.
    pub show_all: bool,
}

impl Store {
    pub fn load(&mut self, snap: Snapshot) {
        self.entities = snap
            .states
            .into_iter()
            .map(|s| (s.entity_id.clone(), s))
            .collect();
        self.areas = snap
            .areas
            .into_iter()
            .map(|a| (a.area_id.clone(), a))
            .collect();
        self.devices = snap
            .devices
            .into_iter()
            .map(|d| (d.id.clone(), d))
            .collect();
        self.registry = snap
            .entities
            .into_iter()
            .map(|e| (e.entity_id.clone(), e))
            .collect();
        self.loaded = true;
    }

    /// Replace one registry with a freshly fetched copy.
    pub fn apply_registry(&mut self, update: RegistryUpdate) {
        match update {
            RegistryUpdate::Areas(l) => {
                self.areas = l.into_iter().map(|a| (a.area_id.clone(), a)).collect();
            }
            RegistryUpdate::Devices(l) => {
                self.devices = l.into_iter().map(|d| (d.id.clone(), d)).collect();
            }
            RegistryUpdate::Entities(l) => {
                self.registry = l.into_iter().map(|e| (e.entity_id.clone(), e)).collect();
            }
        }
    }

    pub fn apply_change(&mut self, entity_id: &str, new_state: Option<EntityState>) {
        match new_state {
            Some(s) => {
                self.entities.insert(entity_id.to_string(), s);
            }
            None => {
                self.entities.remove(entity_id);
            }
        }
    }

    pub fn get(&self, entity_id: &str) -> Option<&EntityState> {
        self.entities.get(entity_id)
    }

    /// Entity's own area, falling back to its device's area.
    pub fn area_of(&self, entity_id: &str) -> Option<&Area> {
        let reg = self.registry.get(entity_id)?;
        let area_id = reg.area_id.as_ref().or_else(|| {
            reg.device_id
                .as_ref()
                .and_then(|d| self.devices.get(d))
                .and_then(|d| d.area_id.as_ref())
        })?;
        self.areas.get(area_id)
    }

    pub fn area_name(&self, entity_id: &str) -> &str {
        self.area_of(entity_id)
            .map(|a| a.name.as_str())
            .unwrap_or(UNASSIGNED)
    }

    /// Hidden, config/diagnostic and system entities are left out of the default views
    /// (like the Home Assistant dashboard) unless `show_all` is set.
    pub fn is_visible(&self, entity_id: &str) -> bool {
        if self.show_all {
            return true;
        }
        if SYSTEM_DOMAINS.contains(&domain_of(entity_id)) {
            return false;
        }
        match self.registry.get(entity_id) {
            Some(r) => {
                r.hidden_by.is_none()
                    && r.disabled_by.is_none()
                    && r.entity_category.is_none()
                    && !r
                        .platform
                        .as_deref()
                        .is_some_and(|p| SYSTEM_PLATFORMS.contains(&p))
            }
            None => true,
        }
    }

    fn group_key(&self, e: &EntityState, by: GroupBy) -> String {
        match by {
            GroupBy::Area => self
                .area_of(&e.entity_id)
                .map(|a| a.area_id.clone())
                .unwrap_or_default(),
            GroupBy::Domain => e.domain().to_string(),
        }
    }

    fn group_title(&self, key: &str, by: GroupBy) -> String {
        match by {
            GroupBy::Area => self
                .areas
                .get(key)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| UNASSIGNED.into()),
            GroupBy::Domain => key.to_string(),
        }
    }

    /// Visible entities, sorted by friendly name, optionally restricted to a group or domain set.
    pub fn visible(
        &self,
        by: GroupBy,
        group: Option<&str>,
        domains: Option<&[&str]>,
    ) -> Vec<&EntityState> {
        let mut v: Vec<&EntityState> = self
            .entities
            .values()
            .filter(|e| self.is_visible(&e.entity_id))
            .filter(|e| domains.is_none_or(|d| d.contains(&e.domain())))
            .filter(|e| group.is_none_or(|g| self.group_key(e, by) == g))
            .collect();
        sort_by_name(&mut v);
        v
    }

    /// Sidebar groups: "All" first, then groups sorted by title, "Unassigned" last.
    pub fn groups(&self, by: GroupBy) -> Vec<Group> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut total = 0;
        for e in self
            .entities
            .values()
            .filter(|e| self.is_visible(&e.entity_id))
        {
            *counts.entry(self.group_key(e, by)).or_default() += 1;
            total += 1;
        }
        let mut groups: Vec<Group> = counts
            .into_iter()
            .map(|(key, count)| Group {
                title: self.group_title(&key, by),
                key: Some(key),
                count,
            })
            .collect();
        groups.sort_by(|a, b| {
            let a_un = a.key.as_deref() == Some("");
            let b_un = b.key.as_deref() == Some("");
            a_un.cmp(&b_un)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        groups.insert(
            0,
            Group {
                key: None,
                title: "All".into(),
                count: total,
            },
        );
        groups
    }
}

pub fn sort_by_name(v: &mut [&EntityState]) {
    v.sort_by_cached_key(|e| (e.friendly_name().to_lowercase(), e.entity_id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn state(id: &str, st: &str, name: &str) -> EntityState {
        serde_json::from_value(json!({
            "entity_id": id, "state": st,
            "attributes": {"friendly_name": name},
            "last_changed": null, "last_updated": null
        }))
        .unwrap()
    }

    fn fixture() -> Store {
        let snap = Snapshot {
            states: vec![
                state("light.kitchen", "on", "Kitchen Light"),
                state("light.bed", "off", "Bed Lamp"),
                state("sensor.temp", "21.5", "Temperature"),
                state("switch.hidden", "on", "Hidden"),
                state("sensor.diag", "1", "Diag"),
                state("zone.home", "0", "Home"),
                state("sensor.backup_state", "idle", "Backup"),
            ],
            areas: serde_json::from_value(json!([
                {"area_id": "kitchen", "name": "Kitchen"},
                {"area_id": "bedroom", "name": "Bedroom"}
            ]))
            .unwrap(),
            devices: serde_json::from_value(json!([
                {"id": "dev1", "area_id": "bedroom"}
            ]))
            .unwrap(),
            entities: serde_json::from_value(json!([
                {"entity_id": "light.kitchen", "area_id": "kitchen", "device_id": null, "hidden_by": null, "disabled_by": null, "entity_category": null},
                {"entity_id": "light.bed", "area_id": null, "device_id": "dev1", "hidden_by": null, "disabled_by": null, "entity_category": null},
                {"entity_id": "switch.hidden", "area_id": null, "device_id": null, "hidden_by": "user", "disabled_by": null, "entity_category": null},
                {"entity_id": "sensor.diag", "area_id": null, "device_id": null, "hidden_by": null, "disabled_by": null, "entity_category": "diagnostic"},
                {"entity_id": "sensor.backup_state", "area_id": null, "device_id": null, "hidden_by": null, "disabled_by": null, "entity_category": null, "platform": "backup"}
            ]))
            .unwrap(),
        };
        let mut s = Store::default();
        s.load(snap);
        s
    }

    #[test]
    fn resolves_area_via_device() {
        let s = fixture();
        assert_eq!(s.area_name("light.kitchen"), "Kitchen");
        assert_eq!(s.area_name("light.bed"), "Bedroom");
        assert_eq!(s.area_name("sensor.temp"), UNASSIGNED);
    }

    #[test]
    fn hides_hidden_and_diagnostic() {
        let s = fixture();
        let ids: Vec<_> = s
            .visible(GroupBy::Area, None, None)
            .iter()
            .map(|e| e.entity_id.as_str())
            .collect();
        assert_eq!(ids, vec!["light.bed", "light.kitchen", "sensor.temp"]);
        let mut s = s;
        s.show_all = true;
        assert_eq!(s.visible(GroupBy::Area, None, None).len(), 7);
    }

    #[test]
    fn groups_sorted_with_unassigned_last() {
        let s = fixture();
        let titles: Vec<_> = s
            .groups(GroupBy::Area)
            .into_iter()
            .map(|g| (g.title, g.count))
            .collect();
        assert_eq!(
            titles,
            vec![
                ("All".into(), 3),
                ("Bedroom".into(), 1),
                ("Kitchen".into(), 1),
                (UNASSIGNED.into(), 1)
            ]
        );
        let domains: Vec<_> = s
            .groups(GroupBy::Domain)
            .into_iter()
            .map(|g| g.title)
            .collect();
        assert_eq!(domains, vec!["All", "light", "sensor"]);
        let bedroom = s.visible(GroupBy::Area, Some("bedroom"), None);
        assert_eq!(bedroom.len(), 1);
    }

    #[test]
    fn applies_changes() {
        let mut s = fixture();
        s.apply_change("light.bed", Some(state("light.bed", "on", "Bed Lamp")));
        assert_eq!(s.get("light.bed").unwrap().state, "on");
        s.apply_change("light.bed", None);
        assert!(s.get("light.bed").is_none());
        s.apply_change("light.new", Some(state("light.new", "on", "New")));
        assert_eq!(s.visible(GroupBy::Domain, Some("light"), None).len(), 2);
    }
}
