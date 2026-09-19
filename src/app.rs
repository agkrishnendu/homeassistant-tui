//! Application state and input handling. Rendering lives in `ui`.

use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use tokio::sync::mpsc::UnboundedSender;

use crate::ha::actions::{self, Action};
use crate::ha::client::{ConnStatus, HaCommand, HaEvent};
use crate::ha::types::{HistoryPoint, LogbookEntry, ServiceCall};
use crate::search::Fuzzy;
use crate::store::{Group, GroupBy, Store};

const TOAST_TTL: Duration = Duration::from_secs(4);
const LOGBOOK_STALE: Duration = Duration::from_secs(60);
const HISTORY_HOURS: i64 = 24;
const LOGBOOK_HOURS: i64 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Entities,
    Scenes,
    Automations,
    Logbook,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Entities, Tab::Scenes, Tab::Automations, Tab::Logbook];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Entities => "Entities",
            Tab::Scenes => "Scenes & Scripts",
            Tab::Automations => "Automations",
            Tab::Logbook => "Logbook",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// Domains listed by the entity-list tabs (`None` = everything).
    fn domains(self) -> Option<&'static [&'static str]> {
        match self {
            Tab::Scenes => Some(&["scene", "script"]),
            Tab::Automations => Some(&["automation"]),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Groups,
    List,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Filter,
    Palette,
    Confirm { call: ServiceCall, label: String },
    Help,
    History,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    at: Instant,
}

#[derive(Debug, Clone)]
pub struct HistoryView {
    pub entity_id: String,
    pub name: String,
    pub unit: Option<String>,
    pub points: Option<Vec<HistoryPoint>>,
    pub error: Option<String>,
    /// Live changes that arrived while `points` was still loading.
    live: Vec<HistoryPoint>,
}

#[derive(Debug, Default)]
pub struct LogbookView {
    pub entries: Vec<LogbookEntry>,
    pub loading: bool,
    pub error: Option<String>,
    fetched_at: Option<Instant>,
    /// Restrict to this entity (toggled with `f`).
    pub entity: Option<String>,
    pub state: TableState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaletteAction {
    GoTo(String),
    Act(String, Action),
    Tab(Tab),
    ToggleGroupBy,
    Resync,
    Help,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaletteItem {
    pub label: String,
    pub hint: String,
    pub action: PaletteAction,
}

#[derive(Debug, Default)]
pub struct Palette {
    pub input: String,
    pub items: Vec<PaletteItem>,
    pub state: ListState,
}

pub struct App {
    pub store: Store,
    pub status: ConnStatus,
    /// When `status` last changed (drives the reconnect countdown).
    pub status_since: Instant,
    pub tab: Tab,
    pub focus: Focus,
    pub mode: Mode,
    pub group_by: GroupBy,
    /// Selected sidebar group (`None` = "All"). Selection is kept by identity, so live
    /// changes that add or reorder rows never move it to another group or entity;
    /// `group_state` and `list_states` hold the derived row, and the last position as a
    /// fallback when the selected item disappears.
    group_key: Option<String>,
    pub group_state: ListState,
    /// Selected entity per entity-list tab (`None` = nothing picked yet: use the row).
    list_sel: [Option<String>; 3],
    /// One table state per entity-list tab.
    pub list_states: [TableState; 3],
    pub filter: String,
    pub palette: Palette,
    pub history: Option<HistoryView>,
    pub logbook: LogbookView,
    pub toast: Option<Toast>,
    pub should_quit: bool,
    fuzzy: Fuzzy,
    cmds: UnboundedSender<HaCommand>,
}

impl App {
    pub fn new(cmds: UnboundedSender<HaCommand>) -> Self {
        Self {
            store: Store::default(),
            status: ConnStatus::Connecting,
            status_since: Instant::now(),
            tab: Tab::Entities,
            focus: Focus::List,
            mode: Mode::Normal,
            group_by: GroupBy::default(),
            group_key: None,
            group_state: ListState::default().with_selected(Some(0)),
            list_sel: Default::default(),
            list_states: Default::default(),
            filter: String::new(),
            palette: Palette::default(),
            history: None,
            logbook: LogbookView::default(),
            toast: None,
            should_quit: false,
            fuzzy: Fuzzy::default(),
            cmds,
        }
    }

    // ----- Derived views --------------------------------------------------

    /// Entity ids shown in the current entity-list tab, in display order.
    pub fn list_ids(&mut self) -> Vec<String> {
        let tab = if self.tab == Tab::Logbook {
            Tab::Entities
        } else {
            self.tab
        };
        let filtering = !self.filter.trim().is_empty();
        // A filter searches everything in the tab, not just the selected group.
        let group = if tab == Tab::Entities && !filtering {
            self.selected_group_key()
        } else {
            None
        };
        let items: Vec<(String, String)> = self
            .store
            .visible(self.group_by, group.as_deref(), tab.domains())
            .into_iter()
            .map(|e| {
                let hay = format!(
                    "{} {} {}",
                    e.friendly_name(),
                    e.entity_id,
                    self.store.area_name(&e.entity_id)
                );
                (e.entity_id.clone(), hay)
            })
            .collect();
        let filter = self.filter.clone();
        self.fuzzy
            .rank(&filter, items, |(_, hay)| hay.clone())
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn selected_group_key(&mut self) -> Option<String> {
        let groups = self.store.groups(self.group_by);
        self.sync_groups(&groups);
        self.group_key.clone()
    }

    /// Point the sidebar row at the selected group. If that group is gone, keep the row
    /// position and select whatever group is there now. Returns the row.
    pub fn sync_groups(&mut self, groups: &[Group]) -> usize {
        let row = groups
            .iter()
            .position(|g| g.key == self.group_key)
            .unwrap_or_else(|| {
                let last = groups.len().saturating_sub(1);
                self.group_state.selected().unwrap_or(0).min(last)
            });
        self.group_state.select(Some(row));
        if let Some(g) = groups.get(row) {
            self.group_key = g.key.clone();
        }
        row
    }

    fn list_slot(&self) -> usize {
        match self.tab {
            Tab::Scenes => 1,
            Tab::Automations => 2,
            _ => 0,
        }
    }

    pub fn list_state(&mut self) -> &mut TableState {
        let i = self.list_slot();
        &mut self.list_states[i]
    }

    /// Point the current list's row at the selected entity. If that entity is gone, keep
    /// the row position and select whatever entity is there now. Returns the row.
    pub fn sync_list(&mut self, ids: &[String]) -> Option<usize> {
        let i = self.list_slot();
        let row = match self.list_sel[i]
            .as_ref()
            .and_then(|id| ids.iter().position(|x| x == id))
        {
            Some(row) => Some(row),
            None if ids.is_empty() => None,
            None => Some(
                self.list_states[i]
                    .selected()
                    .unwrap_or(0)
                    .min(ids.len() - 1),
            ),
        };
        self.list_states[i].select(row);
        self.list_sel[i] = row.map(|r| ids[r].clone());
        row
    }

    pub fn selected_id(&mut self) -> Option<String> {
        let ids = self.list_ids();
        let row = self.sync_list(&ids)?;
        Some(ids[row].clone())
    }

    /// Select the first row of the current list.
    fn reset_list(&mut self) {
        let i = self.list_slot();
        self.list_states[i].select(Some(0));
        self.list_sel[i] = None;
    }

    /// Select the "All" group.
    fn reset_group(&mut self) {
        self.group_state.select(Some(0));
        self.group_key = None;
    }

    // ----- Events -----------------------------------------------------------

    pub fn on_tick(&mut self) {
        if self
            .toast
            .as_ref()
            .is_some_and(|t| t.at.elapsed() > TOAST_TTL)
        {
            self.toast = None;
        }
    }

    pub fn on_ha_event(&mut self, ev: HaEvent) {
        match ev {
            HaEvent::Status(s) => {
                self.status = s;
                self.status_since = Instant::now();
            }
            HaEvent::Snapshot(snap) => {
                // Removals can move the selection to a fallback row: make sure that row is current.
                self.selected_id();
                self.store.load(snap);
                if self.tab == Tab::Logbook {
                    self.fetch_logbook();
                }
            }
            HaEvent::StateChanged {
                entity_id,
                new_state,
            } => {
                if let (Some(h), Some(ns)) = (&mut self.history, &new_state)
                    && h.entity_id == entity_id
                {
                    let p = HistoryPoint {
                        when: ns.last_changed.unwrap_or_else(Utc::now),
                        state: ns.state.clone(),
                    };
                    match &mut h.points {
                        Some(points) => push_live(points, p),
                        None => h.live.push(p),
                    }
                }
                if new_state.is_none() {
                    self.selected_id();
                }
                self.store.apply_change(&entity_id, new_state);
            }
            HaEvent::ServiceResult { label, result } => match result {
                Ok(()) => self.toast(format!("✓ {label}"), false),
                Err(e) => self.toast(format!("✗ {label}: {e}"), true),
            },
            HaEvent::History { entity_id, points } => {
                if let Some(h) = &mut self.history
                    && h.entity_id == entity_id
                {
                    let live = std::mem::take(&mut h.live);
                    let points = h.points.insert(points);
                    for p in live {
                        push_live(points, p);
                    }
                    h.error = None;
                }
            }
            HaEvent::HistoryFailed { entity_id, error } => {
                if let Some(h) = &mut self.history
                    && h.entity_id == entity_id
                {
                    h.error = Some(error);
                }
            }
            HaEvent::Logbook { entity_id, result } => {
                // A reply for a filter that has since changed: the newer request is still coming.
                if entity_id != self.logbook.entity {
                    return;
                }
                self.logbook.loading = false;
                match result {
                    Ok(mut entries) => {
                        entries.sort_by_key(|e| std::cmp::Reverse(e.when));
                        self.logbook.entries = entries;
                        self.logbook.error = None;
                        self.logbook.state.select(Some(0));
                    }
                    Err(e) => self.logbook.error = Some(e),
                }
            }
        }
    }

    pub fn on_terminal_event(&mut self, ev: Event) {
        if let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
        {
            self.on_key(key);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.mode.clone() {
            Mode::Normal => self.on_key_normal(key),
            Mode::Filter => self.on_key_filter(key),
            Mode::Palette => self.on_key_palette(key),
            Mode::Confirm { call, label } => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.send_call(call, label);
                } else {
                    self.toast("Cancelled".into(), false);
                }
            }
            Mode::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
                ) {
                    self.mode = Mode::Normal;
                }
            }
            Mode::History => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('H') | KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    self.history = None;
                }
                KeyCode::Char('r') => {
                    if let Some(id) = self.history.as_ref().map(|h| h.entity_id.clone()) {
                        self.open_history(id);
                    }
                }
                _ => {}
            },
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('p') if ctrl => self.open_palette(),
            KeyCode::Char(':') => self.open_palette(),
            KeyCode::Char('/') if self.tab != Tab::Logbook => {
                self.mode = Mode::Filter;
                self.focus = Focus::List;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.reset_list();
            }
            KeyCode::Char(c @ '1'..='4') => self.set_tab(Tab::ALL[c as usize - '1' as usize]),
            KeyCode::Tab => self.set_tab(Tab::ALL[(self.tab.index() + 1) % 4]),
            KeyCode::BackTab => self.set_tab(Tab::ALL[(self.tab.index() + 3) % 4]),
            KeyCode::Char('r') if self.tab == Tab::Logbook => self.fetch_logbook(),
            KeyCode::Char('r') => {
                let _ = self.cmds.send(HaCommand::Resync);
                self.toast("Resyncing…".into(), false);
            }
            KeyCode::Char('f') if self.tab == Tab::Logbook => {
                self.logbook.entity = match self.logbook.entity.take() {
                    Some(_) => None,
                    None => {
                        let saved = self.tab;
                        self.tab = Tab::Entities;
                        let id = self.selected_id();
                        self.tab = saved;
                        id
                    }
                };
                self.fetch_logbook();
            }
            KeyCode::Char('a') => {
                self.store.show_all = !self.store.show_all;
                let msg = if self.store.show_all {
                    "Showing all entities (incl. hidden & system)"
                } else {
                    "Hiding hidden & system entities"
                };
                self.toast(msg.into(), false);
            }
            KeyCode::Char('g') if self.tab == Tab::Entities => {
                self.group_by = self.group_by.toggle();
                self.reset_group();
            }
            KeyCode::Char('h') | KeyCode::Left if self.tab == Tab::Entities => {
                self.focus = Focus::Groups
            }
            KeyCode::Char('l') | KeyCode::Right if self.tab == Tab::Entities => {
                self.focus = Focus::List
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => self.move_selection(isize::MIN / 2),
            KeyCode::End => self.move_selection(isize::MAX / 2),
            KeyCode::Char('H') if self.tab != Tab::Logbook => {
                if let Some(id) = self.selected_id() {
                    self.open_history(id);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ')
                if self.tab == Tab::Entities && self.focus == Focus::Groups =>
            {
                self.focus = Focus::List;
            }
            code if self.tab != Tab::Logbook => {
                if let Some(action) = key_action(code) {
                    self.act(action);
                }
            }
            _ => {}
        }
    }

    fn on_key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.reset_list();
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
                self.reset_list();
            }
            _ => {}
        }
    }

    fn on_key_palette(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.palette.items.len();
        let sel = self.palette.state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if let Some(item) = self.palette.items.get(sel).cloned() {
                    self.run_palette(item.action);
                }
            }
            KeyCode::Down | KeyCode::Tab => self
                .palette
                .state
                .select(Some((sel + 1).min(n.saturating_sub(1)))),
            KeyCode::Char('n') if ctrl => self
                .palette
                .state
                .select(Some((sel + 1).min(n.saturating_sub(1)))),
            KeyCode::Up | KeyCode::BackTab => {
                self.palette.state.select(Some(sel.saturating_sub(1)))
            }
            KeyCode::Char('p') if ctrl => self.palette.state.select(Some(sel.saturating_sub(1))),
            KeyCode::Backspace => {
                self.palette.input.pop();
                self.refresh_palette();
            }
            KeyCode::Char(c) if !ctrl => {
                self.palette.input.push(c);
                self.refresh_palette();
            }
            _ => {}
        }
    }

    // ----- Commands ---------------------------------------------------------

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.filter.clear();
        if self.mode == Mode::Filter {
            self.mode = Mode::Normal;
        }
        if tab == Tab::Logbook
            && !self.logbook.loading
            && self
                .logbook
                .fetched_at
                .is_none_or(|t| t.elapsed() > LOGBOOK_STALE)
        {
            self.fetch_logbook();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let step = |cur: usize, len: usize| {
            (cur as isize)
                .saturating_add(delta)
                .clamp(0, len as isize - 1) as usize
        };
        match (self.tab, self.focus) {
            (Tab::Logbook, _) => {
                let len = self.logbook.entries.len();
                let next = self
                    .logbook
                    .state
                    .selected()
                    .filter(|_| len > 0)
                    .map(|cur| step(cur.min(len - 1), len))
                    .or((len > 0).then_some(0));
                self.logbook.state.select(next);
            }
            (Tab::Entities, Focus::Groups) => {
                let groups = self.store.groups(self.group_by);
                let cur = self.sync_groups(&groups);
                let next = step(cur, groups.len());
                if next != cur {
                    self.group_state.select(Some(next));
                    self.group_key = groups[next].key.clone();
                    self.reset_list();
                }
            }
            _ => {
                let ids = self.list_ids();
                let Some(cur) = self.sync_list(&ids) else {
                    return;
                };
                let next = step(cur, ids.len());
                let i = self.list_slot();
                self.list_states[i].select(Some(next));
                self.list_sel[i] = Some(ids[next].clone());
            }
        }
    }

    fn act(&mut self, action: Action) {
        let Some(id) = self.selected_id() else { return };
        let Some(e) = self.store.get(&id) else { return };
        let Some(call) = actions::service_for(e, action) else {
            self.toast(
                format!("Nothing to do for {} with that key", e.domain()),
                true,
            );
            return;
        };
        let label = format!("{}: {}", e.friendly_name(), actions::describe(&call));
        if actions::needs_confirm(e, action) {
            self.mode = Mode::Confirm { call, label };
        } else {
            self.send_call(call, label);
        }
    }

    fn send_call(&mut self, call: ServiceCall, label: String) {
        let _ = self.cmds.send(HaCommand::CallService { call, label });
    }

    fn open_history(&mut self, entity_id: String) {
        let e = self.store.get(&entity_id);
        self.history = Some(HistoryView {
            name: e
                .map(|e| e.friendly_name().to_string())
                .unwrap_or_else(|| entity_id.clone()),
            unit: e.and_then(|e| e.unit()).map(str::to_string),
            entity_id: entity_id.clone(),
            points: None,
            error: None,
            live: Vec::new(),
        });
        self.mode = Mode::History;
        let since = Utc::now() - chrono::Duration::hours(HISTORY_HOURS);
        let _ = self.cmds.send(HaCommand::History { entity_id, since });
    }

    fn fetch_logbook(&mut self) {
        self.logbook.loading = true;
        self.logbook.fetched_at = Some(Instant::now());
        let since = Utc::now() - chrono::Duration::hours(LOGBOOK_HOURS);
        let _ = self.cmds.send(HaCommand::Logbook {
            since,
            entity_id: self.logbook.entity.clone(),
        });
    }

    fn toast(&mut self, text: String, error: bool) {
        self.toast = Some(Toast {
            text,
            error,
            at: Instant::now(),
        });
    }

    // ----- Palette ----------------------------------------------------------

    fn open_palette(&mut self) {
        self.palette.input.clear();
        self.mode = Mode::Palette;
        self.refresh_palette();
    }

    fn refresh_palette(&mut self) {
        let mut items: Vec<PaletteItem> = Tab::ALL
            .iter()
            .map(|t| PaletteItem {
                label: format!("Tab: {}", t.title()),
                hint: String::new(),
                action: PaletteAction::Tab(*t),
            })
            .collect();
        let cmd = |label: &str, action| PaletteItem {
            label: label.into(),
            hint: String::new(),
            action,
        };
        items.push(cmd(
            &format!("Group by {}", self.group_by.toggle().label().to_lowercase()),
            PaletteAction::ToggleGroupBy,
        ));
        items.push(cmd("Resync with Home Assistant", PaletteAction::Resync));
        items.push(cmd("Help", PaletteAction::Help));
        items.push(cmd("Quit", PaletteAction::Quit));

        for e in self.store.visible(self.group_by, None, None) {
            let name = e.friendly_name();
            let id = &e.entity_id;
            let verb = match e.domain() {
                "scene" | "script" => Some("Run"),
                "button" | "input_button" => Some("Press"),
                "lock" if e.state == "locked" => Some("Unlock"),
                "lock" => Some("Lock"),
                _ if actions::service_for(e, Action::Primary).is_some() => Some("Toggle"),
                _ => None,
            };
            if let Some(verb) = verb {
                items.push(PaletteItem {
                    label: format!("{verb} {name}"),
                    hint: id.clone(),
                    action: PaletteAction::Act(id.clone(), Action::Primary),
                });
            }
            if e.domain() == "automation" {
                items.push(PaletteItem {
                    label: format!("Trigger {name}"),
                    hint: id.clone(),
                    action: PaletteAction::Act(id.clone(), Action::Trigger),
                });
            }
            items.push(PaletteItem {
                label: format!("Go to {name}"),
                hint: id.clone(),
                action: PaletteAction::GoTo(id.clone()),
            });
        }
        let query = self.palette.input.clone();
        let mut ranked = self
            .fuzzy
            .rank(&query, items, |i| format!("{} {}", i.label, i.hint));
        ranked.truncate(200);
        self.palette.items = ranked;
        self.palette.state.select(Some(0));
    }

    fn run_palette(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Tab(t) => self.set_tab(t),
            PaletteAction::ToggleGroupBy => {
                self.group_by = self.group_by.toggle();
                self.reset_group();
            }
            PaletteAction::Resync => {
                let _ = self.cmds.send(HaCommand::Resync);
            }
            PaletteAction::Help => self.mode = Mode::Help,
            PaletteAction::Quit => self.should_quit = true,
            PaletteAction::GoTo(id) => self.go_to(&id),
            PaletteAction::Act(id, action) => {
                let Some(e) = self.store.get(&id) else { return };
                let Some(call) = actions::service_for(e, action) else {
                    return;
                };
                let label = format!("{}: {}", e.friendly_name(), actions::describe(&call));
                if actions::needs_confirm(e, action) {
                    self.mode = Mode::Confirm { call, label };
                } else {
                    self.send_call(call, label);
                }
            }
        }
    }

    /// Select `id` in the Entities tab's "All" group.
    pub fn go_to(&mut self, id: &str) {
        self.set_tab(Tab::Entities);
        self.focus = Focus::List;
        self.reset_group();
        self.reset_list();
        let i = self.list_slot();
        self.list_sel[i] = Some(id.to_string());
        self.selected_id();
    }
}

/// Append a live change to a history, skipping attribute-only updates and anything the
/// history already covers.
fn push_live(points: &mut Vec<HistoryPoint>, p: HistoryPoint) {
    if points
        .last()
        .is_none_or(|last| last.state != p.state && last.when <= p.when)
    {
        points.push(p);
    }
}

fn key_action(code: KeyCode) -> Option<Action> {
    Some(match code {
        KeyCode::Enter | KeyCode::Char(' ') => Action::Primary,
        KeyCode::Char('+') | KeyCode::Char('=') => Action::Increase,
        KeyCode::Char('-') | KeyCode::Char('_') => Action::Decrease,
        KeyCode::Char(']') => Action::SecondaryUp,
        KeyCode::Char('[') => Action::SecondaryDown,
        KeyCode::Char('m') => Action::CycleMode,
        KeyCode::Char('o') => Action::Open,
        KeyCode::Char('c') => Action::Close,
        KeyCode::Char('s') => Action::Stop,
        KeyCode::Char('x') => Action::Trigger,
        KeyCode::Char('n') => Action::Next,
        KeyCode::Char('p') => Action::Prev,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::client::Snapshot;
    use crossterm::event::KeyEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn app() -> (App, mpsc::UnboundedReceiver<HaCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = App::new(tx);
        let states = serde_json::from_value(json!([
            {"entity_id": "light.kitchen", "state": "off", "attributes": {"friendly_name": "Kitchen Light"}},
            {"entity_id": "light.bed", "state": "on", "attributes": {"friendly_name": "Bed Lamp"}},
            {"entity_id": "lock.front", "state": "locked", "attributes": {"friendly_name": "Front Door"}},
            {"entity_id": "scene.movie", "state": "unknown", "attributes": {"friendly_name": "Movie"}},
            {"entity_id": "automation.night", "state": "on", "attributes": {"friendly_name": "Night"}}
        ]))
        .unwrap();
        app.on_ha_event(HaEvent::Snapshot(Snapshot {
            states,
            ..Default::default()
        }));
        (app, rx)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn filter_then_toggle() {
        let (mut app, mut rx) = app();
        press(&mut app, KeyCode::Char('/'));
        type_str(&mut app, "kitch");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.list_ids(), vec!["light.kitchen"]);
        press(&mut app, KeyCode::Enter);
        match rx.try_recv().unwrap() {
            HaCommand::CallService { call, .. } => assert_eq!(call.name(), "light.toggle"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn lock_requires_confirmation() {
        let (mut app, mut rx) = app();
        app.go_to("lock.front");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::Char('n'));
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        match rx.try_recv().unwrap() {
            HaCommand::CallService { call, .. } => assert_eq!(call.name(), "lock.unlock"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn palette_runs_scene() {
        let (mut app, mut rx) = app();
        press(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "run movie");
        assert_eq!(app.palette.items[0].label, "Run Movie");
        press(&mut app, KeyCode::Enter);
        match rx.try_recv().unwrap() {
            HaCommand::CallService { call, .. } => assert_eq!(call.name(), "scene.turn_on"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tabs_list_their_domains() {
        let (mut app, mut rx) = app();
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.list_ids(), vec!["scene.movie"]);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.list_ids(), vec!["automation.night"]);
        press(&mut app, KeyCode::Char('x'));
        match rx.try_recv().unwrap() {
            HaCommand::CallService { call, .. } => assert_eq!(call.name(), "automation.trigger"),
            other => panic!("unexpected {other:?}"),
        }
        press(&mut app, KeyCode::Char('4'));
        assert!(matches!(rx.try_recv().unwrap(), HaCommand::Logbook { .. }));
    }

    fn set(app: &mut App, id: &str, name: &str) {
        let st = serde_json::from_value(
            json!({"entity_id": id, "state": "on", "attributes": {"friendly_name": name}}),
        )
        .unwrap();
        app.on_ha_event(HaEvent::StateChanged {
            entity_id: id.into(),
            new_state: Some(st),
        });
    }

    fn remove(app: &mut App, id: &str) {
        app.on_ha_event(HaEvent::StateChanged {
            entity_id: id.into(),
            new_state: None,
        });
    }

    #[test]
    fn selection_survives_insertions_and_renames() {
        let (mut app, _rx) = app();
        app.go_to("light.kitchen");
        assert_eq!(app.selected_id().as_deref(), Some("light.kitchen"));
        // Sorts before the selection: the row index shifts, the selected entity must not.
        set(&mut app, "light.aaa", "Aaa");
        assert_eq!(app.selected_id().as_deref(), Some("light.kitchen"));
        set(&mut app, "light.kitchen", "Zzz Kitchen");
        assert_eq!(app.selected_id().as_deref(), Some("light.kitchen"));
        press(&mut app, KeyCode::Up);
        assert_ne!(app.selected_id().as_deref(), Some("light.kitchen"));
    }

    #[test]
    fn deleted_selection_falls_back_to_same_row() {
        let (mut app, _rx) = app();
        // Sorted: Bed Lamp, Front Door, Kitchen Light, Movie, Night.
        app.go_to("lock.front");
        remove(&mut app, "lock.front");
        assert_eq!(app.selected_id().as_deref(), Some("light.kitchen"));
        remove(&mut app, "light.kitchen");
        remove(&mut app, "scene.movie");
        remove(&mut app, "automation.night");
        assert_eq!(app.selected_id().as_deref(), Some("light.bed"));
        remove(&mut app, "light.bed");
        assert_eq!(app.selected_id(), None);
    }

    #[test]
    fn selection_kept_when_toggling_show_all() {
        let (mut app, _rx) = app();
        app.go_to("scene.movie");
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.selected_id().as_deref(), Some("scene.movie"));
    }

    #[test]
    fn group_selection_survives_new_groups() {
        let (mut app, _rx) = app();
        press(&mut app, KeyCode::Char('g')); // group by domain: All, automation, light, lock, scene
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected_group_key().as_deref(), Some("lock"));
        set(&mut app, "fan.attic", "Attic Fan"); // new "fan" group sorts before "lock"
        assert_eq!(app.selected_group_key().as_deref(), Some("lock"));
        assert_eq!(app.list_ids(), vec!["lock.front"]);
        // The group disappears: fall back to the group now in that row.
        remove(&mut app, "lock.front");
        assert_eq!(app.selected_group_key().as_deref(), Some("scene"));
    }

    #[test]
    fn stale_logbook_reply_is_ignored() {
        let (mut app, _rx) = app();
        app.logbook.entity = Some("light.bed".into());
        app.logbook.loading = true;
        let entry = |id: &str| {
            LogbookEntry::from_value(&json!({"when": 1.0, "entity_id": id, "state": "on"})).unwrap()
        };
        app.on_ha_event(HaEvent::Logbook {
            entity_id: None,
            result: Ok(vec![entry("light.kitchen")]),
        });
        assert!(app.logbook.loading);
        assert!(app.logbook.entries.is_empty());
        app.on_ha_event(HaEvent::Logbook {
            entity_id: Some("light.bed".into()),
            result: Ok(vec![entry("light.bed")]),
        });
        assert!(!app.logbook.loading);
        assert_eq!(app.logbook.entries.len(), 1);
    }

    #[test]
    fn history_keeps_changes_made_while_loading() {
        let (mut app, mut rx) = app();
        app.go_to("light.bed");
        press(&mut app, KeyCode::Char('H'));
        assert!(matches!(rx.try_recv().unwrap(), HaCommand::History { .. }));
        let ns = serde_json::from_value(
            json!({"entity_id": "light.bed", "state": "off", "last_changed": Utc::now()}),
        )
        .unwrap();
        app.on_ha_event(HaEvent::StateChanged {
            entity_id: "light.bed".into(),
            new_state: Some(ns),
        });
        let earlier = HistoryPoint {
            when: Utc::now() - chrono::Duration::hours(1),
            state: "on".into(),
        };
        app.on_ha_event(HaEvent::History {
            entity_id: "light.bed".into(),
            points: vec![earlier],
        });
        let states: Vec<_> = app
            .history
            .as_ref()
            .unwrap()
            .points
            .as_ref()
            .unwrap()
            .iter()
            .map(|p| p.state.as_str())
            .collect();
        assert_eq!(states, vec!["on", "off"]);
    }

    #[test]
    fn history_follows_live_changes() {
        let (mut app, mut rx) = app();
        app.go_to("light.bed");
        press(&mut app, KeyCode::Char('H'));
        assert!(matches!(rx.try_recv().unwrap(), HaCommand::History { .. }));
        app.on_ha_event(HaEvent::History {
            entity_id: "light.bed".into(),
            points: vec![],
        });
        let ns = serde_json::from_value(json!({"entity_id": "light.bed", "state": "off"})).unwrap();
        app.on_ha_event(HaEvent::StateChanged {
            entity_id: "light.bed".into(),
            new_state: Some(ns),
        });
        assert_eq!(
            app.history.as_ref().unwrap().points.as_ref().unwrap().len(),
            1
        );
    }
}
