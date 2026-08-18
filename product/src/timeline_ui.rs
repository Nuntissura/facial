use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
};

use chrono::{DateTime, Datelike, NaiveDate};
use eframe::egui::{self, Align, Layout, RichText, ScrollArea, TextEdit};
use serde::Deserialize;
use serde_json::Value;

use crate::{theme, timeline_ledger};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TimelineView {
    #[default]
    Overview,
    Events,
    Planned,
    Sources,
    Coverage,
}

impl TimelineView {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Events,
        Self::Planned,
        Self::Sources,
        Self::Coverage,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Events => "Events",
            Self::Planned => "Planned",
            Self::Sources => "Sources",
            Self::Coverage => "Coverage",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EventDetailTab {
    #[default]
    Summary,
    People,
    Media,
    Evidence,
}

impl EventDetailTab {
    const ALL: [Self; 4] = [Self::Summary, Self::People, Self::Media, Self::Evidence];

    fn label(self) -> &'static str {
        match self {
            Self::Summary => "Summary",
            Self::People => "People",
            Self::Media => "Media",
            Self::Evidence => "Evidence",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct TimelineDashboard {
    root: PathBuf,
    groups: Vec<GroupRow>,
    events: Vec<EventRow>,
    planned: Vec<PlannedRow>,
    canonical_sources: Vec<SourceRow>,
    captures: Vec<CaptureRow>,
    capture_error: Option<String>,
    coverage_lanes: Vec<CoverageRow>,
    coverage_error: Option<String>,
    rejections: Vec<RejectionDiagnostic>,
    rejection_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct GroupRow {
    id: String,
    name: String,
    members: Vec<MemberRow>,
}

#[derive(Clone, Debug, Default)]
struct MemberRow {
    id: String,
    name: String,
}

#[derive(Clone, Debug, Default)]
struct EventRow {
    id: String,
    group_id: String,
    title: String,
    time_value: String,
    time_precision: String,
    time_kind: String,
    sort_key: ChronologicalKey,
    status: String,
    location: String,
    categories: Vec<String>,
    member_ids: Vec<String>,
    people: Vec<PersonRow>,
    media: Vec<MediaRow>,
    evidence: Vec<String>,
    summary: String,
}

#[derive(Clone, Debug, Default)]
struct PersonRow {
    member_id: String,
    actual_status: String,
    planned_status: String,
    presence_mode: String,
    roles: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct MediaRow {
    title: String,
    url: String,
    platform: String,
    published_at: String,
    source_tier: String,
    attribution: String,
}

#[derive(Clone, Debug, Default)]
struct PlannedRow {
    id: String,
    group_id: String,
    title: String,
    scheduled: String,
    precision: String,
    transition: String,
    location: String,
    member_ids: Vec<String>,
    evidence: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct ChronologicalKey {
    day: i32,
    precision_rank: u8,
    instant_millis: i64,
    raw: String,
}

#[derive(Clone, Debug, Default)]
struct SourceRow {
    id: String,
    title: String,
    url: String,
    platform: String,
    tier: String,
    published_at: String,
    availability: String,
}

#[derive(Clone, Debug, Default)]
struct CaptureRow {
    proposal_id: String,
    job_id: String,
    url: String,
    source_kind: String,
    state: String,
    byte_length: u64,
}

#[derive(Clone, Debug, Default)]
struct CoverageRow {
    lane_id: String,
    group_id: String,
    subject_ids: Vec<String>,
    platform: String,
    source_surface_id: String,
    range_start: String,
    range_end: String,
    cursor_type: String,
    cursor_value: String,
    status: String,
    last_checked_at: String,
    items_seen: u64,
    candidates_added: u64,
    failures: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct RejectionDiagnostic {
    audit_id: String,
    job_id: String,
    code: String,
    detail: String,
}

#[derive(Debug, Default, Deserialize)]
struct CoverageFile {
    #[serde(default)]
    source_surfaces: Vec<CoverageSurfaceFile>,
    #[serde(default)]
    lanes: Vec<CoverageLaneFile>,
}

#[derive(Debug, Default, Deserialize)]
struct CoverageSurfaceFile {
    source_surface_id: String,
    group_id: String,
    platform: String,
}

#[derive(Debug, Default, Deserialize)]
struct CoverageLaneFile {
    lane_id: String,
    #[serde(default)]
    subject_ids: Vec<String>,
    source_surface_id: String,
    #[serde(default)]
    range_start: Option<String>,
    #[serde(default)]
    range_end: Option<String>,
    #[serde(default)]
    cursor: CoverageCursorFile,
    status: String,
    #[serde(default)]
    last_checked_at: Option<String>,
    #[serde(default)]
    result: CoverageResultFile,
    #[serde(default)]
    failures: Vec<serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct CoverageCursorFile {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    value: Option<serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct CoverageResultFile {
    #[serde(default)]
    items_seen: u64,
    #[serde(default)]
    candidates_added: u64,
}

#[derive(Debug, Deserialize)]
struct RosterFile {
    group_id: String,
    display_name: String,
    #[serde(default)]
    members: Vec<RosterMember>,
}

#[derive(Debug, Deserialize)]
struct RosterMember {
    member_id: String,
    display_name: String,
}

pub(crate) struct TimelineUiState {
    project_input: String,
    loaded_root: String,
    dashboard: Option<Arc<TimelineDashboard>>,
    error: String,
    loading: bool,
    load_rx: Option<mpsc::Receiver<Result<TimelineDashboard, String>>>,
    selected_group: Option<String>,
    selected_member: Option<String>,
    view: TimelineView,
    search: String,
    newest_first: bool,
    expanded: BTreeSet<String>,
    detail_tabs: BTreeMap<String, EventDetailTab>,
    event_filter_key: String,
    filtered_event_indices: Vec<usize>,
    event_page: usize,
}

impl TimelineUiState {
    pub(crate) fn new(workspace_root: &str) -> Self {
        let project_input = std::env::var("FACIAL_TIMELINE_ROOT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| workspace_root.to_string());
        let mut state = Self {
            project_input,
            loaded_root: String::new(),
            dashboard: None,
            error: String::new(),
            loading: false,
            load_rx: None,
            selected_group: None,
            selected_member: None,
            view: TimelineView::Overview,
            search: String::new(),
            newest_first: true,
            expanded: BTreeSet::new(),
            detail_tabs: BTreeMap::new(),
            event_filter_key: String::new(),
            filtered_event_indices: Vec::new(),
            event_page: 0,
        };
        if timeline_ledger::discover_project_root(Path::new(&state.project_input)).is_ok() {
            state.begin_load();
        }
        state
    }

    pub(crate) fn load_fixture(&mut self) {
        let mut dashboard = fixture_dashboard();
        dashboard.root = PathBuf::from(r"D:\fixture\idol-timeline-project");
        self.project_input = dashboard.root.to_string_lossy().to_string();
        self.loaded_root = self.project_input.clone();
        self.selected_group = Some("KTL-GRP-ive".to_string());
        self.selected_member = None;
        self.view = TimelineView::Events;
        self.expanded.clear();
        self.expanded
            .insert("KTL-OCC-fixture-music-station".to_string());
        self.detail_tabs.insert(
            "KTL-OCC-fixture-music-station".to_string(),
            EventDetailTab::Media,
        );
        self.dashboard = Some(Arc::new(dashboard));
        self.loading = false;
        self.error.clear();
        self.load_rx = None;
        self.event_filter_key.clear();
        self.filtered_event_indices.clear();
        self.event_page = 0;
    }

    pub(crate) fn load_fixture_preset(&mut self, preset: &str) -> Result<(), String> {
        self.load_fixture();
        let event_id = "KTL-OCC-fixture-music-station".to_string();
        match preset {
            "media" => {}
            "summary" => {
                self.detail_tabs.insert(event_id, EventDetailTab::Summary);
            }
            "people" => {
                self.detail_tabs.insert(event_id, EventDetailTab::People);
            }
            "evidence" => {
                self.detail_tabs.insert(event_id, EventDetailTab::Evidence);
            }
            "member" => {
                self.selected_member = Some("KTL-MBR-ive-wonyoung".to_string());
                self.newest_first = false;
                self.detail_tabs.insert(event_id, EventDetailTab::People);
            }
            "planned" => self.view = TimelineView::Planned,
            "sources" => self.view = TimelineView::Sources,
            "coverage" => self.view = TimelineView::Coverage,
            other => return Err(format!("unknown timeline inspector preset: {other}")),
        }
        self.event_filter_key.clear();
        self.filtered_event_indices.clear();
        self.event_page = 0;
        Ok(())
    }

    fn begin_load(&mut self) {
        if self.loading {
            return;
        }
        let start = PathBuf::from(self.project_input.trim());
        let (tx, rx) = mpsc::channel();
        self.loading = true;
        self.error.clear();
        self.loaded_root.clear();
        self.dashboard = None;
        self.selected_group = None;
        self.selected_member = None;
        self.event_filter_key.clear();
        self.filtered_event_indices.clear();
        self.event_page = 0;
        self.load_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_dashboard(&start));
        });
    }

    fn poll_load(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.load_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(dashboard)) => {
                self.loaded_root = dashboard.root.to_string_lossy().to_string();
                let first_group = dashboard.groups.first().map(|group| group.id.clone());
                if self.selected_group.as_ref().is_none_or(|selected| {
                    !dashboard.groups.iter().any(|group| &group.id == selected)
                }) {
                    self.selected_group = first_group;
                    self.selected_member = None;
                }
                self.dashboard = Some(Arc::new(dashboard));
                self.event_filter_key.clear();
                self.filtered_event_indices.clear();
                self.event_page = 0;
                self.loading = false;
                self.load_rx = None;
            }
            Ok(Err(error)) => {
                self.error = error;
                self.loading = false;
                self.load_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100))
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.error = "Timeline loader stopped before returning a result".to_string();
                self.loading = false;
                self.load_rx = None;
            }
        }
    }

    pub(crate) fn draw(&mut self, ui: &mut egui::Ui) {
        self.poll_load(ui.ctx());
        self.draw_title(ui);
        ui.add_space(8.0);
        let Some(dashboard) = self.dashboard.clone() else {
            theme::sheet_frame().show(ui, |ui| {
                ui.heading("Connect a timeline project");
                ui.label(
                    "Select any folder at or below a project containing timeline-maintenance.yaml.",
                );
                if !self.error.is_empty() {
                    ui.add_space(6.0);
                    ui.colored_label(theme::accent(), &self.error);
                }
            });
            return;
        };

        let height = ui.available_height();
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(226.0, height),
                Layout::top_down(Align::Min),
                |ui| self.draw_subject_rail(ui, &dashboard),
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), height),
                Layout::top_down(Align::Min),
                |ui| self.draw_content(ui, &dashboard),
            );
        });
    }

    fn draw_title(&mut self, ui: &mut egui::Ui) {
        let compact = ui.available_width() < 1_100.0;
        let heading = |ui: &mut egui::Ui| {
            ui.vertical(|ui| {
                ui.heading("Timeline intelligence");
                ui.label(
                    RichText::new("Canonical activity, planned schedules, media, evidence, and research intake")
                        .small()
                        .color(theme::ink_faint()),
                );
            });
        };
        let mut project_picker = |ui: &mut egui::Ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if self.loading {
                    "Loading…"
                } else {
                    "Load / refresh"
                };
                if theme::primary_button_enabled(ui, !self.loading, label).clicked() {
                    self.begin_load();
                }
                ui.add_sized(
                    [360.0, 24.0],
                    TextEdit::singleline(&mut self.project_input)
                        .hint_text("Timeline project folder"),
                );
            });
        };
        if compact {
            heading(ui);
            ui.add_space(6.0);
            ui.horizontal(|ui| project_picker(ui));
        } else {
            ui.horizontal(|ui| {
                heading(ui);
                project_picker(ui);
            });
        }
        if !self.loaded_root.is_empty() {
            ui.label(
                RichText::new(format!("Loaded · {}", self.loaded_root))
                    .small()
                    .color(theme::ink_faint()),
            );
        }
        if !self.error.is_empty() {
            ui.colored_label(theme::accent(), &self.error);
        }
    }

    fn draw_subject_rail(&mut self, ui: &mut egui::Ui, dashboard: &TimelineDashboard) {
        theme::kicker(ui, "Groups & members");
        ui.add_space(4.0);
        ScrollArea::vertical()
            .id_source("timeline_subject_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for group in &dashboard.groups {
                    let selected = self.selected_group.as_deref() == Some(group.id.as_str());
                    let count = dashboard
                        .events
                        .iter()
                        .filter(|event| event.group_id == group.id)
                        .count();
                    if ui
                        .selectable_label(selected, format!("{}  {}", group.name, count))
                        .on_hover_text(&group.id)
                        .clicked()
                    {
                        self.selected_group = Some(group.id.clone());
                        self.selected_member = None;
                    }
                    if selected {
                        ui.indent(("timeline_members", &group.id), |ui| {
                            if ui
                                .selectable_label(self.selected_member.is_none(), "All members")
                                .clicked()
                            {
                                self.selected_member = None;
                            }
                            for member in &group.members {
                                let selected_member =
                                    self.selected_member.as_deref() == Some(member.id.as_str());
                                if ui
                                    .selectable_label(selected_member, &member.name)
                                    .on_hover_text(&member.id)
                                    .clicked()
                                {
                                    self.selected_member = Some(member.id.clone());
                                }
                            }
                        });
                    }
                    ui.add_space(4.0);
                }
            });
    }

    fn draw_content(&mut self, ui: &mut egui::Ui, dashboard: &TimelineDashboard) {
        let group = self
            .selected_group
            .as_deref()
            .and_then(|id| dashboard.groups.iter().find(|group| group.id == id));
        let subject_name = self
            .selected_member
            .as_deref()
            .and_then(|member_id| {
                group.and_then(|group| group.members.iter().find(|member| member.id == member_id))
            })
            .map(|member| member.name.as_str())
            .or_else(|| group.map(|group| group.name.as_str()))
            .unwrap_or("Timeline");

        ui.horizontal(|ui| {
            ui.heading(subject_name);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.newest_first, "Newest first");
                ui.add_sized(
                    [250.0, 24.0],
                    TextEdit::singleline(&mut self.search).hint_text("Search timeline"),
                );
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for view in TimelineView::ALL {
                if theme::tab_item(ui, self.view == view, view.label()) {
                    self.view = view;
                }
                ui.add_space(18.0);
            }
        });
        ui.add_space(8.0);
        theme::hairline(ui);
        ui.add_space(6.0);

        match self.view {
            TimelineView::Overview => self.draw_overview(ui, dashboard),
            TimelineView::Events => self.draw_events(ui, dashboard, None),
            TimelineView::Planned => self.draw_planned(ui, dashboard),
            TimelineView::Sources => self.draw_sources(ui, dashboard),
            TimelineView::Coverage => self.draw_coverage(ui, dashboard),
        }
    }

    fn draw_overview(&mut self, ui: &mut egui::Ui, dashboard: &TimelineDashboard) {
        self.ensure_event_filter_cache(dashboard);
        let event_count = self.filtered_event_indices.len();
        let planned_count = self.filtered_planned(dashboard).len();
        ui.horizontal(|ui| {
            metric(ui, "CANONICAL EVENTS", event_count);
            metric(ui, "PLANNED", planned_count);
            metric(ui, "CANONICAL SOURCES", dashboard.canonical_sources.len());
            metric(ui, "INTAKE CAPTURES", dashboard.captures.len());
        });
        if let Some(error) = &dashboard.capture_error {
            ui.colored_label(
                theme::accent(),
                format!("Research intake unavailable; canonical timeline remains loaded: {error}"),
            );
        }
        ui.add_space(10.0);
        theme::kicker(ui, "Recent canonical activity");
        self.draw_events(ui, dashboard, Some(5));
    }

    fn current_event_filter_key(&self, dashboard: &TimelineDashboard) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}:{}",
            dashboard.root.display(),
            dashboard.events.len(),
            self.selected_group.as_deref().unwrap_or_default(),
            self.selected_member.as_deref().unwrap_or_default(),
            self.newest_first,
            self.search.trim().to_lowercase()
        )
    }

    fn ensure_event_filter_cache(&mut self, dashboard: &TimelineDashboard) {
        let key = self.current_event_filter_key(dashboard);
        if key == self.event_filter_key {
            return;
        }
        let needle = self.search.trim().to_lowercase();
        let mut rows = dashboard
            .events
            .iter()
            .enumerate()
            .filter(|event| {
                self.selected_group
                    .as_ref()
                    .is_none_or(|group| &event.1.group_id == group)
            })
            .filter(|event| {
                self.selected_member
                    .as_ref()
                    .is_none_or(|member| event.1.member_ids.contains(member))
            })
            .filter(|event| {
                needle.is_empty() || searchable_event(event.1).to_lowercase().contains(&needle)
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            left.1
                .sort_key
                .cmp(&right.1.sort_key)
                .then(left.1.id.cmp(&right.1.id))
        });
        if self.newest_first {
            rows.reverse();
        }
        self.filtered_event_indices = rows.into_iter().map(|(index, _)| index).collect();
        self.event_filter_key = key;
        self.event_page = 0;
    }

    fn draw_events(
        &mut self,
        ui: &mut egui::Ui,
        dashboard: &TimelineDashboard,
        limit: Option<usize>,
    ) {
        self.ensure_event_filter_cache(dashboard);
        let page_size = 25usize;
        let total = self.filtered_event_indices.len();
        let page_count = total.div_ceil(page_size).max(1);
        self.event_page = self.event_page.min(page_count - 1);
        let start = if limit.is_some() {
            0
        } else {
            self.event_page * page_size
        };
        let rows = self
            .filtered_event_indices
            .iter()
            .skip(start)
            .take(limit.unwrap_or(page_size))
            .copied()
            .collect::<Vec<_>>();
        if rows.is_empty() {
            empty_state(ui, "No canonical events match this subject and search.");
            return;
        }
        if limit.is_none() {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.event_page > 0, egui::Button::new("Previous"))
                    .clicked()
                {
                    self.event_page -= 1;
                }
                ui.label(format!(
                    "Page {} of {} · {} canonical rows",
                    self.event_page + 1,
                    page_count,
                    total
                ));
                if ui
                    .add_enabled(self.event_page + 1 < page_count, egui::Button::new("Next"))
                    .clicked()
                {
                    self.event_page += 1;
                }
            });
            ui.add_space(5.0);
        }
        ScrollArea::vertical()
            .id_source(if limit.is_some() {
                "timeline_overview_events"
            } else {
                "timeline_events"
            })
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for index in rows {
                    self.draw_event_card(ui, &dashboard.events[index], dashboard);
                    ui.add_space(7.0);
                }
            });
    }

    fn draw_event_card(
        &mut self,
        ui: &mut egui::Ui,
        event: &EventRow,
        dashboard: &TimelineDashboard,
    ) {
        let expanded = self.expanded.contains(&event.id);
        theme::sheet_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                let arrow = if expanded { "▼" } else { "▶" };
                if ui.small_button(arrow).clicked() {
                    if expanded {
                        self.expanded.remove(&event.id);
                    } else {
                        self.expanded.insert(event.id.clone());
                    }
                }
                ui.label(RichText::new(&event.title).strong());
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(38.0);
                ui.label(RichText::new(&event.time_value).strong());
                ui.label(
                    RichText::new(format!("{} · {}", event.time_kind, event.time_precision))
                        .small()
                        .color(theme::ink_faint()),
                );
                status_label(ui, &event.status);
                ui.label(
                    RichText::new(format!(
                        "{} evidence · {} media",
                        event.evidence.len(),
                        event.media.len()
                    ))
                    .small()
                    .color(theme::ink_faint()),
                );
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(38.0);
                ui.label(
                    RichText::new(format!("{} · {}", event.location, event.id))
                        .small()
                        .color(theme::ink_faint()),
                );
            });

            if expanded {
                ui.add_space(8.0);
                theme::hairline(ui);
                ui.add_space(7.0);
                let active = self.detail_tabs.entry(event.id.clone()).or_default();
                ui.horizontal(|ui| {
                    for tab in EventDetailTab::ALL {
                        if theme::tab_item(ui, *active == tab, tab.label()) {
                            *active = tab;
                        }
                        ui.add_space(16.0);
                    }
                });
                ui.add_space(8.0);
                match *active {
                    EventDetailTab::Summary => {
                        ui.label(&event.summary);
                        if !event.categories.is_empty() {
                            ui.label(
                                RichText::new(event.categories.join(" · "))
                                    .small()
                                    .color(theme::ink_faint()),
                            );
                        }
                        ui.add_space(4.0);
                        ui.label(format!("Location: {}", event.location));
                    }
                    EventDetailTab::People => self.draw_people(ui, event, dashboard),
                    EventDetailTab::Media => self.draw_event_media(ui, event),
                    EventDetailTab::Evidence => self.draw_evidence(ui, event),
                }
            }
        });
    }

    fn draw_people(&self, ui: &mut egui::Ui, event: &EventRow, dashboard: &TimelineDashboard) {
        if event.people.is_empty() {
            empty_state(
                ui,
                "No participant-specific canonical records are attached.",
            );
            return;
        }
        for person in &event.people {
            let display_name = dashboard
                .groups
                .iter()
                .flat_map(|group| &group.members)
                .find(|member| member.id == person.member_id)
                .map(|member| member.name.as_str())
                .unwrap_or(person.member_id.as_str());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(display_name).strong())
                    .on_hover_text(&person.member_id);
                ui.label(format!("Actual · {}", person.actual_status));
                ui.label(format!("Planned · {}", person.planned_status));
                ui.label(format!("Mode · {}", person.presence_mode));
                if !person.roles.is_empty() {
                    ui.label(
                        RichText::new(format!("Roles · {}", person.roles.join(", ")))
                            .small()
                            .color(theme::ink_faint()),
                    );
                }
            });
        }
    }

    fn draw_event_media(&self, ui: &mut egui::Ui, event: &EventRow) {
        if event.media.is_empty() {
            empty_state(ui, "No canonical media is linked to this event.");
            ui.label(
                RichText::new("Unlinked research captures remain in Sources and are never promoted here implicitly.")
                    .small()
                    .color(theme::ink_faint()),
            );
            return;
        }
        ScrollArea::vertical()
            .id_source(("timeline_event_media", &event.id))
            .max_height(220.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for media in &event.media {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&media.title).strong());
                            ui.label(
                                RichText::new(format!(
                                    "Published {} · {} · {} · {}",
                                    unknown(&media.published_at),
                                    unknown(&media.platform),
                                    unknown(&media.source_tier),
                                    unknown(&media.attribution)
                                ))
                                .small()
                                .color(theme::ink_faint()),
                            );
                            ui.label(RichText::new(&media.url).small());
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Copy link").clicked() {
                                ui.output_mut(|output| output.copied_text = media.url.clone());
                            }
                        });
                    });
                    theme::hairline(ui);
                    ui.add_space(4.0);
                }
            });
    }

    fn draw_evidence(&self, ui: &mut egui::Ui, event: &EventRow) {
        if event.evidence.is_empty() {
            empty_state(ui, "No claim-level evidence is attached.");
            return;
        }
        for item in &event.evidence {
            ui.label(format!("• {item}"));
        }
    }

    fn filtered_planned<'a>(&self, dashboard: &'a TimelineDashboard) -> Vec<&'a PlannedRow> {
        let needle = self.search.trim().to_lowercase();
        let mut rows = dashboard
            .planned
            .iter()
            .filter(|row| {
                self.selected_group
                    .as_ref()
                    .is_none_or(|group| &row.group_id == group)
            })
            .filter(|row| {
                self.selected_member
                    .as_ref()
                    .is_none_or(|member| row.member_ids.contains(member))
            })
            .filter(|row| {
                needle.is_empty()
                    || format!(
                        "{} {} {} {}",
                        row.title, row.id, row.location, row.transition
                    )
                    .to_lowercase()
                    .contains(&needle)
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            chronological_key(&left.scheduled)
                .cmp(&chronological_key(&right.scheduled))
                .then(left.id.cmp(&right.id))
        });
        if self.newest_first {
            rows.reverse();
        }
        rows
    }

    fn draw_planned(&mut self, ui: &mut egui::Ui, dashboard: &TimelineDashboard) {
        let rows = self.filtered_planned(dashboard);
        if rows.is_empty() {
            empty_state(ui, "No planned events match this subject and search.");
            return;
        }
        ScrollArea::vertical()
            .id_source("timeline_planned")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in rows {
                    theme::sheet_frame().show(ui, |ui| {
                        ui.label(RichText::new(&row.title).strong());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&row.scheduled).strong());
                            ui.label(
                                RichText::new(format!("Scheduled · {}", row.precision))
                                    .small()
                                    .color(theme::ink_faint()),
                            );
                            status_label(ui, &row.transition);
                            ui.label(
                                RichText::new(format!("{} evidence", row.evidence.len()))
                                    .small()
                                    .color(theme::ink_faint()),
                            );
                        });
                        ui.label(
                            RichText::new(format!("{} · {}", row.location, row.id))
                                .small()
                                .color(theme::ink_faint()),
                        );
                    });
                    ui.add_space(7.0);
                }
            });
    }

    fn draw_sources(&mut self, ui: &mut egui::Ui, dashboard: &TimelineDashboard) {
        let needle = self.search.trim().to_lowercase();
        ScrollArea::vertical()
            .id_source("timeline_sources")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::kicker(ui, "Canonical source registry");
                for source in dashboard.canonical_sources.iter().filter(|source| {
                    needle.is_empty()
                        || format!("{} {} {} {}", source.title, source.url, source.platform, source.id)
                            .to_lowercase()
                            .contains(&needle)
                }) {
                    source_row(ui, &source.title, &source.url, &format!(
                        "{} · {} · published {} · {}",
                        unknown(&source.platform), unknown(&source.tier), unknown(&source.published_at), unknown(&source.availability)
                    ));
                }
                ui.add_space(12.0);
                theme::kicker(ui, "Research intake · not promoted");
                ui.label(
                    RichText::new("Captured source proposals are raw intake. They do not prove an event, date, location, or member participation.")
                        .small()
                        .color(theme::ink_faint()),
                );
                if let Some(error) = &dashboard.capture_error {
                    ui.colored_label(theme::accent(), format!("Ledger intake unavailable: {error}"));
                }
                ui.add_space(5.0);
                ui.label(
                    RichText::new(format!(
                        "{} captured proposals · {} rejection audits",
                        dashboard.captures.len(),
                        dashboard.rejections.len()
                    ))
                    .small()
                    .strong(),
                );
                for capture in dashboard.captures.iter().filter(|capture| {
                    needle.is_empty()
                        || format!("{} {} {} {}", capture.job_id, capture.url, capture.source_kind, capture.proposal_id)
                            .to_lowercase()
                            .contains(&needle)
                }) {
                    source_row(ui, &capture.job_id, &capture.url, &format!(
                        "INTAKE ONLY · {} · {} · {} bytes · {}",
                        capture.source_kind, capture.state, capture.byte_length, capture.proposal_id
                    ));
                }
                ui.add_space(12.0);
                theme::kicker(ui, "Rejection audit · bounded newest-first view");
                if let Some(error) = &dashboard.rejection_error {
                    ui.colored_label(theme::accent(), format!("Rejection audit unavailable: {error}"));
                }
                let mut shown = 0usize;
                for rejection in dashboard.rejections.iter().filter(|rejection| {
                    needle.is_empty()
                        || format!(
                            "{} {} {} {}",
                            rejection.audit_id, rejection.job_id, rejection.code, rejection.detail
                        )
                        .to_lowercase()
                        .contains(&needle)
                }) {
                    if shown >= 100 {
                        break;
                    }
                    shown += 1;
                    source_row(
                        ui,
                        &format!("{} · {}", rejection.code, rejection.job_id),
                        "",
                        &format!("{} · {}", rejection.audit_id, rejection.detail),
                    );
                }
                if dashboard.rejections.len() > shown {
                    ui.label(
                        RichText::new(format!(
                            "Showing {shown} rejection rows; refine Search to inspect another bounded subset."
                        ))
                        .small()
                        .color(theme::ink_faint()),
                    );
                }
            });
    }

    fn draw_coverage(&self, ui: &mut egui::Ui, dashboard: &TimelineDashboard) {
        ScrollArea::vertical()
            .id_source("timeline_coverage")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Counts describe the loaded canonical stores; they are not a completeness claim.")
                        .small()
                        .color(theme::ink_faint()),
                );
                ui.add_space(7.0);
                if let Some(error) = &dashboard.coverage_error {
                    ui.colored_label(theme::accent(), format!("Coverage state unavailable: {error}"));
                }
                for group in &dashboard.groups {
                    let events = dashboard.events.iter().filter(|event| event.group_id == group.id).count();
                    let planned = dashboard.planned.iter().filter(|event| event.group_id == group.id).count();
                    let media = dashboard
                        .events
                        .iter()
                        .filter(|event| event.group_id == group.id)
                        .map(|event| event.media.len())
                        .sum::<usize>();
                    theme::sheet_frame().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&group.name).strong());
                            ui.label(format!("{} members", group.members.len()));
                            ui.label(format!("{events} events"));
                            ui.label(format!("{media} linked media"));
                            ui.label(format!("{planned} planned"));
                        });
                        let group_lanes = dashboard
                            .coverage_lanes
                            .iter()
                            .filter(|lane| lane.group_id == group.id)
                            .collect::<Vec<_>>();
                        let exhausted = group_lanes
                            .iter()
                            .filter(|lane| lane.status == "exhausted")
                            .count();
                        let failed = group_lanes
                            .iter()
                            .filter(|lane| !lane.failures.is_empty())
                            .count();
                        ui.label(
                            RichText::new(format!(
                                "{} lanes · {exhausted} exhausted · {failed} with failures",
                                group_lanes.len()
                            ))
                            .small()
                            .color(theme::ink_faint()),
                        );
                    });
                    ui.add_space(7.0);
                }
                ui.add_space(5.0);
                theme::kicker(ui, "Source-lane cursor and failure diagnostics");
                let needle = self.search.trim().to_lowercase();
                let mut shown = 0usize;
                for lane in dashboard.coverage_lanes.iter().filter(|lane| {
                    self.selected_group
                        .as_ref()
                        .is_none_or(|group| &lane.group_id == group)
                        && self
                            .selected_member
                            .as_ref()
                            .is_none_or(|member| lane.subject_ids.contains(member))
                        && (needle.is_empty()
                            || format!(
                                "{} {} {} {} {} {}",
                                lane.lane_id,
                                lane.platform,
                                lane.source_surface_id,
                                lane.subject_ids.join(" "),
                                lane.status,
                                lane.failures.join(" ")
                            )
                            .to_lowercase()
                            .contains(&needle))
                }) {
                    if shown >= 250 {
                        break;
                    }
                    shown += 1;
                    theme::sheet_frame().show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&lane.platform).strong());
                            status_label(ui, &lane.status);
                            ui.label(
                                RichText::new(format!(
                                    "{} seen · {} yielded",
                                    lane.items_seen, lane.candidates_added
                                ))
                                .small(),
                            );
                            ui.label(
                                RichText::new(format!("{} failure(s)", lane.failures.len()))
                                    .small()
                                    .color(if lane.failures.is_empty() {
                                        theme::ink_faint()
                                    } else {
                                        theme::accent()
                                    }),
                            );
                        });
                        ui.label(
                            RichText::new(format!(
                                "{} · subjects {} · {} → {}",
                                lane.lane_id,
                                lane.subject_ids.join(", "),
                                lane.range_start,
                                lane.range_end
                            ))
                            .small()
                            .color(theme::ink_faint()),
                        );
                        ui.label(
                            RichText::new(format!(
                                "cursor {}={} · last checked {} · surface {}",
                                lane.cursor_type,
                                lane.cursor_value,
                                lane.last_checked_at,
                                lane.source_surface_id
                            ))
                            .small(),
                        );
                        for failure in lane.failures.iter().take(3) {
                            ui.colored_label(theme::accent(), format!("Failure · {failure}"));
                        }
                        if lane.failures.len() > 3 {
                            ui.label(
                                RichText::new(format!(
                                    "+{} more failures in coverage-state.yaml",
                                    lane.failures.len() - 3
                                ))
                                .small()
                                .color(theme::ink_faint()),
                            );
                        }
                    });
                    ui.add_space(6.0);
                }
                if shown == 0 {
                    empty_state(ui, "No source lanes match this group, member, and search.");
                } else if dashboard.coverage_lanes.len() > shown {
                    ui.label(
                        RichText::new(format!(
                            "Showing {shown} lanes in this bounded view; use group, member, or Search filters for more."
                        ))
                        .small()
                        .color(theme::ink_faint()),
                    );
                }
            });
    }
}

fn metric(ui: &mut egui::Ui, label: &str, value: usize) {
    theme::sheet_frame().show(ui, |ui| {
        ui.label(RichText::new(value.to_string()).size(22.0).strong());
        ui.label(RichText::new(label).small().color(theme::ink_faint()));
    });
}

fn source_row(ui: &mut egui::Ui, title: &str, url: &str, metadata: &str) {
    theme::sheet_frame().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong());
                ui.label(RichText::new(metadata).small().color(theme::ink_faint()));
                if !url.is_empty() {
                    ui.label(RichText::new(url).small());
                }
            });
            if !url.is_empty() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Copy link").clicked() {
                        ui.output_mut(|output| output.copied_text = url.to_string());
                    }
                });
            }
        });
    });
    ui.add_space(6.0);
}

fn empty_state(ui: &mut egui::Ui, text: &str) {
    theme::sheet_frame().show(ui, |ui| {
        ui.label(RichText::new(text).color(theme::ink_faint()));
    });
}

fn status_label(ui: &mut egui::Ui, status: &str) {
    let color = if status.contains("verified") || status == "captured" {
        egui::Color32::from_rgb(22, 104, 63)
    } else if status.contains("cancel") || status.contains("retract") {
        theme::accent()
    } else {
        theme::ink_soft()
    };
    ui.label(
        RichText::new(status.to_uppercase())
            .small()
            .strong()
            .color(color),
    );
}

fn unknown(value: &str) -> &str {
    if value.trim().is_empty() {
        "unknown"
    } else {
        value
    }
}

fn searchable_event(event: &EventRow) -> String {
    let media = event
        .media
        .iter()
        .map(|item| format!("{} {}", item.title, item.url))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {} {} {} {} {} {}",
        event.id,
        event.title,
        event.location,
        event.status,
        event.categories.join(" "),
        event.evidence.join(" "),
        media
    )
}

fn load_dashboard(start: &Path) -> Result<TimelineDashboard, String> {
    let root = timeline_ledger::discover_project_root(start)?;
    let groups = load_groups(&root)?;
    let member_to_group = groups
        .iter()
        .flat_map(|group| {
            group
                .members
                .iter()
                .map(|member| (member.id.clone(), group.id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let data_root = root.join("timeline-data");
    let sources = replay_jsonl(&data_root.join("source-registry.jsonl"), "source_id")?;
    let artifacts = replay_jsonl(&data_root.join("artifact-registry.jsonl"), "artifact_id")?;
    let occurrences = replay_jsonl(&data_root.join("event-registry.jsonl"), "occurrence_id")?;
    let planned_values = replay_jsonl(&data_root.join("planned-events.jsonl"), "occurrence_id")?;

    let canonical_sources = sources.values().map(source_from_value).collect::<Vec<_>>();
    let source_map = canonical_sources
        .iter()
        .map(|source| (source.id.clone(), source.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut artifacts_by_occurrence: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let mut standalone = Vec::new();
    for artifact in artifacts.values() {
        let ids = string_array(artifact.get("occurrence_ids"));
        if ids.is_empty() {
            standalone.push(artifact);
        } else {
            for occurrence_id in ids {
                artifacts_by_occurrence
                    .entry(occurrence_id)
                    .or_default()
                    .push(artifact);
            }
        }
    }

    let mut events = occurrences
        .values()
        .map(|value| {
            occurrence_from_value(
                value,
                artifacts_by_occurrence
                    .get(string(value, "occurrence_id").as_str())
                    .cloned()
                    .unwrap_or_default(),
                &source_map,
            )
        })
        .collect::<Vec<_>>();
    events.extend(
        standalone
            .into_iter()
            .map(|artifact| artifact_event_from_value(artifact, &source_map, &member_to_group)),
    );

    let planned = planned_values
        .values()
        .map(|value| planned_from_value(value, &member_to_group))
        .collect::<Vec<_>>();
    let (captures, capture_error) = match timeline_ledger::load_captured_sources(&root) {
        Ok(rows) => (
            rows.into_iter()
                .map(|row| CaptureRow {
                    proposal_id: row.proposal_id,
                    job_id: row.job_id,
                    url: row.canonical_url,
                    source_kind: row.source_kind,
                    state: row.state,
                    byte_length: row.byte_length,
                })
                .collect(),
            None,
        ),
        Err(error) => (Vec::new(), Some(error)),
    };
    let (rejections, rejection_error) = match timeline_ledger::load_rejection_audits(&root) {
        Ok(rows) => (
            rows.into_iter()
                .map(|row| RejectionDiagnostic {
                    audit_id: row.audit_id,
                    job_id: row.job_id,
                    code: row.code,
                    detail: row.detail,
                })
                .collect(),
            None,
        ),
        Err(error) => (Vec::new(), Some(error)),
    };
    let (coverage_lanes, coverage_error) = match load_coverage(&data_root) {
        Ok(rows) => (rows, None),
        Err(error) => (Vec::new(), Some(error)),
    };

    Ok(TimelineDashboard {
        root,
        groups,
        events,
        planned,
        canonical_sources,
        captures,
        capture_error,
        coverage_lanes,
        coverage_error,
        rejections,
        rejection_error,
    })
}

fn load_coverage(data_root: &Path) -> Result<Vec<CoverageRow>, String> {
    let path = data_root.join("coverage-state.yaml");
    let text =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let file: CoverageFile = serde_yaml::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let surfaces = file
        .source_surfaces
        .into_iter()
        .map(|surface| (surface.source_surface_id.clone(), surface))
        .collect::<BTreeMap<_, _>>();
    let mut lanes = file
        .lanes
        .into_iter()
        .map(|lane| {
            let surface = surfaces.get(&lane.source_surface_id);
            CoverageRow {
                lane_id: lane.lane_id,
                group_id: surface
                    .map(|surface| surface.group_id.clone())
                    .unwrap_or_default(),
                subject_ids: lane.subject_ids,
                platform: surface
                    .map(|surface| surface.platform.clone())
                    .unwrap_or_default(),
                source_surface_id: lane.source_surface_id,
                range_start: lane.range_start.unwrap_or_else(|| "unknown".to_string()),
                range_end: lane.range_end.unwrap_or_else(|| "open".to_string()),
                cursor_type: fallback(lane.cursor.kind, "none"),
                cursor_value: lane
                    .cursor
                    .value
                    .as_ref()
                    .map(yaml_value_text)
                    .unwrap_or_else(|| "none".to_string()),
                status: fallback(lane.status, "unknown"),
                last_checked_at: lane.last_checked_at.unwrap_or_else(|| "never".to_string()),
                items_seen: lane.result.items_seen,
                candidates_added: lane.result.candidates_added,
                failures: lane.failures.iter().map(yaml_value_text).collect(),
            }
        })
        .collect::<Vec<_>>();
    lanes.sort_by(|left, right| {
        left.group_id
            .cmp(&right.group_id)
            .then(left.platform.cmp(&right.platform))
            .then(left.lane_id.cmp(&right.lane_id))
    });
    Ok(lanes)
}

fn yaml_value_text(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::Null => "none".to_string(),
        serde_yaml::Value::String(value) => value.clone(),
        _ => serde_yaml::to_string(value)
            .unwrap_or_else(|_| "unreadable".to_string())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn load_groups(root: &Path) -> Result<Vec<GroupRow>, String> {
    let mut groups = Vec::new();
    let entries =
        fs::read_dir(root).map_err(|error| format!("read {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read project child: {error}"))?;
        if !entry.path().is_dir() {
            continue;
        }
        let roster_path = entry.path().join("group-roster.yaml");
        if !roster_path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&roster_path)
            .map_err(|error| format!("read {}: {error}", roster_path.display()))?;
        let roster: RosterFile = serde_yaml::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", roster_path.display()))?;
        let mut members = roster
            .members
            .into_iter()
            .map(|member| MemberRow {
                id: member.member_id,
                name: member.display_name,
            })
            .collect::<Vec<_>>();
        members.sort_by(|left, right| left.name.cmp(&right.name));
        groups.push(GroupRow {
            id: roster.group_id,
            name: roster.display_name,
            members,
        });
    }
    groups.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(groups)
}

fn replay_jsonl(path: &Path, entity_field: &str) -> Result<BTreeMap<String, Value>, String> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut latest = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("parse {} line {}: {error}", path.display(), index + 1))?;
        let id = string(&value, entity_field);
        if id.is_empty() {
            return Err(format!(
                "{} line {} has no {entity_field}",
                path.display(),
                index + 1
            ));
        }
        latest.insert(id, value);
    }
    Ok(latest)
}

fn occurrence_from_value(
    value: &Value,
    artifacts: Vec<&Value>,
    sources: &BTreeMap<String, SourceRow>,
) -> EventRow {
    let id = string(value, "occurrence_id");
    let people = value
        .get("participants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|person| PersonRow {
            member_id: string(person, "member_id"),
            actual_status: fallback(string(person, "actual_status"), "unknown"),
            planned_status: fallback(string(person, "planned_status"), "unknown"),
            presence_mode: fallback(string(person, "presence_mode"), "unknown"),
            roles: string_array(person.get("roles")),
        })
        .collect::<Vec<_>>();
    let mut member_ids = people
        .iter()
        .map(|person| person.member_id.clone())
        .collect::<Vec<_>>();
    let mut media = Vec::new();
    for artifact in artifacts {
        member_ids.extend(string_array(artifact.get("member_ids")));
        media.extend(media_from_artifact(artifact, sources));
    }
    member_ids.sort();
    member_ids.dedup();
    let time_value = nested_string(value, &["temporal", "start", "value"]);
    EventRow {
        id,
        group_id: string(value, "group_id"),
        title: fallback(string(value, "title"), "Untitled occurrence"),
        sort_key: chronological_key(&time_value),
        time_value: fallback(time_value, "Unknown date"),
        time_precision: fallback(
            nested_string(value, &["temporal", "start", "precision"]),
            "unknown precision",
        ),
        time_kind: "Occurred".to_string(),
        status: fallback(string(value, "occurrence_status"), "unresolved"),
        location: location_label(value.get("location")),
        categories: string_array(value.get("categories")),
        member_ids,
        people,
        media,
        evidence: evidence_labels(value.get("evidence")),
        summary: fallback(string(value, "summary"), "Canonical occurrence record."),
    }
}

fn artifact_event_from_value(
    value: &Value,
    sources: &BTreeMap<String, SourceRow>,
    member_to_group: &BTreeMap<String, String>,
) -> EventRow {
    let member_ids = string_array(value.get("member_ids"));
    let group_id = string(value, "group_id");
    let group_id = if group_id.is_empty() {
        member_ids
            .iter()
            .find_map(|member| member_to_group.get(member))
            .cloned()
            .unwrap_or_default()
    } else {
        group_id
    };
    let published = nested_string(value, &["publication_time", "value"]);
    EventRow {
        id: string(value, "artifact_id"),
        group_id,
        title: fallback(string(value, "title"), "Untitled public artifact"),
        time_value: fallback(published.clone(), "Publication time unknown"),
        time_precision: fallback(nested_string(value, &["publication_time", "precision"]), "unknown precision"),
        time_kind: "Published".to_string(),
        sort_key: chronological_key(&published),
        status: fallback(string(value, "artifact_status"), "unverified"),
        location: "No occurrence location (artifact-only)".to_string(),
        categories: vec![fallback(string(value, "artifact_type"), "public-artifact")],
        member_ids,
        people: Vec::new(),
        media: media_from_artifact(value, sources),
        evidence: evidence_labels(value.get("evidence")),
        summary: "Standalone published artifact. Publication does not establish when or where depicted activity occurred.".to_string(),
    }
}

fn media_from_artifact(value: &Value, sources: &BTreeMap<String, SourceRow>) -> Vec<MediaRow> {
    let title = fallback(string(value, "title"), "Untitled media");
    let published = nested_string(value, &["publication_time", "value"]);
    let attribution = fallback(string(value, "attribution_status"), "canonical artifact");
    string_array(value.get("source_ids"))
        .into_iter()
        .filter_map(|source_id| sources.get(&source_id))
        .map(|source| MediaRow {
            title: title.clone(),
            url: source.url.clone(),
            platform: source.platform.clone(),
            published_at: if published.is_empty() {
                source.published_at.clone()
            } else {
                published.clone()
            },
            source_tier: source.tier.clone(),
            attribution: attribution.clone(),
        })
        .collect()
}

fn planned_from_value(value: &Value, member_to_group: &BTreeMap<String, String>) -> PlannedRow {
    let member_ids = value
        .get("participants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|person| string(person, "member_id"))
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    let group_id = string(value, "group_id");
    let group_id = if group_id.is_empty() {
        member_ids
            .iter()
            .find_map(|member| member_to_group.get(member))
            .cloned()
            .unwrap_or_default()
    } else {
        group_id
    };
    PlannedRow {
        id: string(value, "occurrence_id"),
        group_id,
        title: fallback(string(value, "title"), "Untitled planned event"),
        scheduled: fallback(
            nested_string(value, &["schedule", "start", "value"]),
            "Schedule unknown",
        ),
        precision: fallback(
            nested_string(value, &["schedule", "start", "precision"]),
            "unknown precision",
        ),
        transition: fallback(string(value, "transition"), "unresolved"),
        location: location_label(value.get("location")),
        member_ids,
        evidence: evidence_labels(value.get("evidence")),
    }
}

fn source_from_value(value: &Value) -> SourceRow {
    SourceRow {
        id: string(value, "source_id"),
        title: fallback(string(value, "title"), "Untitled source"),
        url: string(value, "canonical_url"),
        platform: string(value, "platform"),
        tier: string(value, "source_tier"),
        published_at: value
            .get("published_at")
            .and_then(time_value)
            .unwrap_or_default(),
        availability: string(value, "availability"),
    }
}

fn time_value(value: &Value) -> Option<String> {
    value.as_str().map(ToString::to_string).or_else(|| {
        value
            .get("value")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

fn chronological_key(value: &str) -> ChronologicalKey {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        let utc = parsed.to_utc();
        return ChronologicalKey {
            day: utc.date_naive().num_days_from_ce(),
            precision_rank: 4,
            instant_millis: utc.timestamp_millis(),
            raw: value.to_string(),
        };
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return ChronologicalKey {
            day: date.num_days_from_ce(),
            precision_rank: 3,
            instant_millis: 0,
            raw: value.to_string(),
        };
    }
    if let Ok(date) = NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d") {
        return ChronologicalKey {
            day: date.num_days_from_ce(),
            precision_rank: 2,
            instant_millis: 0,
            raw: value.to_string(),
        };
    }
    if let Ok(date) = NaiveDate::parse_from_str(&format!("{value}-01-01"), "%Y-%m-%d") {
        return ChronologicalKey {
            day: date.num_days_from_ce(),
            precision_rank: 1,
            instant_millis: 0,
            raw: value.to_string(),
        };
    }
    ChronologicalKey {
        day: i32::MIN,
        precision_rank: 0,
        instant_millis: i64::MIN,
        raw: value.to_string(),
    }
}

fn location_label(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "Location not publicly stated".to_string();
    };
    let status = string(value, "status");
    let parts = ["venue", "city", "region", "country"]
        .into_iter()
        .map(|key| string(value, key))
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        if status.is_empty() {
            "Location not publicly stated".to_string()
        } else {
            status
        }
    } else {
        format!(
            "{} [{}]",
            parts.join(", "),
            fallback(status, "status unknown")
        )
    }
}

fn evidence_labels(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|evidence| {
            let claim = fallback(string(evidence, "claim_type"), "untyped claim");
            let subject = string(evidence, "subject_id");
            let sources = string_array(evidence.get("source_ids"));
            format!(
                "{} · {} · {}",
                claim,
                unknown(&subject),
                if sources.is_empty() {
                    "no source IDs".to_string()
                } else {
                    sources.join(", ")
                }
            )
        })
        .collect()
}

fn nested_string(value: &Value, path: &[&str]) -> String {
    let mut current = value;
    for key in path {
        let Some(next) = current.get(*key) else {
            return String::new();
        };
        current = next;
    }
    current.as_str().unwrap_or_default().to_string()
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn fallback(value: String, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn fixture_dashboard() -> TimelineDashboard {
    let members = ["Yujin", "Gaeul", "Rei", "Wonyoung", "Liz", "Leeseo"]
        .into_iter()
        .map(|name| MemberRow {
            id: format!("KTL-MBR-ive-{}", name.to_lowercase()),
            name: name.to_string(),
        })
        .collect::<Vec<_>>();
    let member_ids = members
        .iter()
        .map(|member| member.id.clone())
        .collect::<Vec<_>>();
    let media = (1..=7)
        .map(|index| MediaRow {
            title: format!("Official Music Station camera {index}"),
            url: format!("https://example.test/ive/music-station/{index}"),
            platform: "official-broadcaster".to_string(),
            published_at: "2024-10-18T13:00:00+09:00".to_string(),
            source_tier: "tier_1".to_string(),
            attribution: "direct-member-proven".to_string(),
        })
        .collect::<Vec<_>>();
    TimelineDashboard {
        groups: vec![
            GroupRow { id: "KTL-GRP-ive".to_string(), name: "IVE".to_string(), members },
            GroupRow { id: "KTL-GRP-itzy".to_string(), name: "ITZY".to_string(), members: vec![] },
            GroupRow { id: "KTL-GRP-odd-youth".to_string(), name: "ODD YOUTH".to_string(), members: vec![] },
        ],
        events: vec![
            EventRow {
                id: "KTL-OCC-fixture-music-station".to_string(),
                group_id: "KTL-GRP-ive".to_string(),
                title: "Music Station — ACCENDIO performance and interview".to_string(),
                time_value: "2024-10-18".to_string(),
                time_precision: "date".to_string(),
                time_kind: "Occurred".to_string(),
                sort_key: chronological_key("2024-10-18"),
                status: "occurred-verified".to_string(),
                location: "TV Asahi studios, Tokyo, Japan [verified]".to_string(),
                categories: vec!["tv".to_string(), "performance".to_string(), "interview".to_string()],
                member_ids: member_ids.clone(),
                people: member_ids.iter().map(|id| PersonRow {
                    member_id: id.clone(),
                    actual_status: "present".to_string(),
                    planned_status: "confirmed".to_string(),
                    presence_mode: "in-person".to_string(),
                    roles: vec!["performer".to_string()],
                }).collect(),
                media,
                evidence: vec![
                    "broadcast-aired · KTL-GRP-ive · KTL-SRC-fixture-broadcast".to_string(),
                    "member-present · all six members · KTL-SRC-fixture-camera".to_string(),
                ],
                summary: "Verified broadcast occurrence with separately recorded publication artifacts and member-specific participation evidence.".to_string(),
            },
            EventRow {
                id: "KTL-ART-fixture-art-film".to_string(),
                group_id: "KTL-GRP-ive".to_string(),
                title: "IVE EMPATHY concept film".to_string(),
                time_value: "2025-01-24T23:00:00+09:00".to_string(),
                time_precision: "second".to_string(),
                time_kind: "Published".to_string(),
                sort_key: chronological_key("2025-01-24T23:00:00+09:00"),
                status: "verified".to_string(),
                location: "No occurrence location (artifact-only)".to_string(),
                categories: vec!["public-artifact".to_string(), "release".to_string()],
                member_ids: member_ids.clone(),
                summary: "Publication is verified; filming date and location remain unknown.".to_string(),
                ..Default::default()
            },
        ],
        planned: vec![PlannedRow {
            id: "KTL-OCC-fixture-world-tour".to_string(),
            group_id: "KTL-GRP-ive".to_string(),
            title: "World tour — Brussels session".to_string(),
            scheduled: "2026-09-12T19:30:00+02:00".to_string(),
            precision: "minute".to_string(),
            transition: "confirmed".to_string(),
            location: "Brussels, Belgium [verified public announcement]".to_string(),
            member_ids,
            evidence: vec!["official public announcement".to_string()],
        }],
        canonical_sources: vec![SourceRow {
            id: "KTL-SRC-fixture-broadcast".to_string(),
            title: "Music Station official broadcast page".to_string(),
            url: "https://example.test/music-station/ive".to_string(),
            platform: "official-web".to_string(),
            tier: "tier_1".to_string(),
            published_at: "2024-10-18".to_string(),
            availability: "available".to_string(),
        }],
        captures: vec![CaptureRow {
            proposal_id: "KTL-PROP-fixture-intake".to_string(),
            job_id: "IVE-BACKTRACK-OFFICIAL-VIDEO".to_string(),
            url: "https://example.test/unreviewed-source".to_string(),
            source_kind: "official-group".to_string(),
            state: "captured".to_string(),
            byte_length: 42137,
        }],
        coverage_lanes: vec![CoverageRow {
            lane_id: "KTL-LANE-fixture-youtube-yujin".to_string(),
            group_id: "KTL-GRP-ive".to_string(),
            subject_ids: vec!["KTL-MBR-ive-yujin".to_string()],
            platform: "youtube".to_string(),
            source_surface_id: "KTL-SURF-fixture-youtube".to_string(),
            range_start: "2021-12-01".to_string(),
            range_end: "2026-08-15".to_string(),
            cursor_type: "page-token".to_string(),
            cursor_value: "terminal".to_string(),
            status: "exhausted".to_string(),
            last_checked_at: "2026-08-15T22:00:00+02:00".to_string(),
            items_seen: 696,
            candidates_added: 696,
            failures: Vec::new(),
        }, CoverageRow {
            lane_id: "KTL-LANE-fixture-instagram-yujin".to_string(),
            group_id: "KTL-GRP-ive".to_string(),
            subject_ids: vec!["KTL-MBR-ive-yujin".to_string()],
            platform: "instagram".to_string(),
            source_surface_id: "KTL-SURF-fixture-instagram".to_string(),
            range_start: "2021-12-01".to_string(),
            range_end: "2026-08-15".to_string(),
            cursor_type: "native-id".to_string(),
            cursor_value: "blocked".to_string(),
            status: "blocked".to_string(),
            last_checked_at: "2026-08-15T22:10:00+02:00".to_string(),
            items_seen: 0,
            candidates_added: 0,
            failures: vec!["SOURCE_LOGIN_REQUIRED · public surface returned a login shell".to_string()],
        }],
        rejections: vec![RejectionDiagnostic {
            audit_id: "KTL-REJ-fixture-http-status".to_string(),
            job_id: "IVE-BROADCASTER-KBS-0001".to_string(),
            code: "SOURCE_HTTP_STATUS".to_string(),
            detail: "429 Too Many Requests".to_string(),
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_member_filter_is_deterministic() {
        let dashboard = fixture_dashboard();
        let mut state = TimelineUiState::new("");
        state.dashboard = Some(Arc::new(dashboard.clone()));
        state.selected_group = Some("KTL-GRP-ive".to_string());
        state.selected_member = Some("KTL-MBR-ive-yujin".to_string());
        state.ensure_event_filter_cache(&dashboard);
        assert_eq!(state.filtered_event_indices.len(), 2);
        state.selected_member = Some("KTL-MBR-ive-nobody".to_string());
        state.ensure_event_filter_cache(&dashboard);
        assert!(state.filtered_event_indices.is_empty());
    }

    #[test]
    fn artifact_rows_remain_publication_only() {
        let dashboard = fixture_dashboard();
        let artifact = dashboard
            .events
            .iter()
            .find(|event| event.id.starts_with("KTL-ART"))
            .unwrap();
        assert_eq!(artifact.time_kind, "Published");
        assert!(artifact.location.contains("artifact-only"));
        assert!(!artifact.status.contains("occurred"));
    }

    #[test]
    fn replay_uses_latest_record_for_entity() {
        let root =
            std::env::temp_dir().join(format!("facial-timeline-ui-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("events.jsonl");
        fs::write(
            &path,
            "{\"occurrence_id\":\"KTL-OCC-one\",\"title\":\"old\"}\n{\"occurrence_id\":\"KTL-OCC-one\",\"title\":\"new\"}\n",
        )
        .unwrap();
        let values = replay_jsonl(&path, "occurrence_id").unwrap();
        assert_eq!(values["KTL-OCC-one"]["title"], "new");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn chronological_key_orders_rfc3339_by_instant_not_local_text() {
        let earlier = chronological_key("2024-01-01T00:30:00+14:00");
        let later = chronological_key("2023-12-31T23:00:00-12:00");
        assert!(earlier < later);
    }

    #[test]
    #[ignore = "set FACIAL_TIMELINE_TEST_ROOT to probe a real project"]
    fn live_project_probe() {
        let root = std::env::var("FACIAL_TIMELINE_TEST_ROOT")
            .expect("FACIAL_TIMELINE_TEST_ROOT is required for this ignored probe");
        let dashboard = load_dashboard(Path::new(&root)).unwrap();
        println!(
            "root={} groups={} events={} planned={} canonical_sources={} captures={} capture_error={:?}",
            dashboard.root.display(),
            dashboard.groups.len(),
            dashboard.events.len(),
            dashboard.planned.len(),
            dashboard.canonical_sources.len(),
            dashboard.captures.len(),
            dashboard.capture_error
        );
        assert!(!dashboard.groups.is_empty());
    }
}
