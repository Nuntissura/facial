//! In-app folder browser (WP-014).
//!
//! Pure-egui replacement for the native `rfd` dialog: CODEX section 7 forbids
//! standard UI controls from launching external file-manager windows, and the
//! native picker also stole focus and was invisible to the headless inspector.
//! This dialog is an ordinary `egui::Window`, so it renders through
//! `FacialApp::render_ui`, never leaves the app, and snapshots like any widget.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::media_explorer;
use crate::theme;

/// Hard cap on listed subdirectories so a pathological folder cannot bloat the
/// frame; the UI reports when the cap truncates the listing.
const MAX_ENTRIES: usize = 1000;

/// What the caller intends to do with the folder it is picking (WP-074).
/// The dialog body itself is purpose-agnostic; only the title and the caller's
/// handling differ, so a destination pick can never fall into the historical
/// "set this lane's folder and rescan" sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerPurpose {
    /// Historical behavior: choose the folder a lane browses.
    LaneFolder,
    /// Choose a destination for a pending file operation.
    MoveDestination,
    CopyDestination,
    /// Choose the parent folder a new destination folder is created inside.
    CopyIntoNewFolderParent,
}

impl PickerPurpose {
    fn title(&self, lane_id: usize) -> String {
        match self {
            Self::LaneFolder => format!("Select folder — lane {}", lane_id + 1),
            Self::MoveDestination => "Move to folder".to_string(),
            Self::CopyDestination => "Copy to folder".to_string(),
            Self::CopyIntoNewFolderParent => "Copy into new folder — choose parent".to_string(),
        }
    }

    fn commit_label(&self) -> &'static str {
        match self {
            Self::LaneFolder => "Use this folder",
            Self::MoveDestination => "Move here",
            Self::CopyDestination => "Copy here",
            Self::CopyIntoNewFolderParent => "Create here",
        }
    }
}

#[derive(Default)]
pub struct FolderPicker {
    /// Lane id the picker is currently open for (None = closed).
    open_for: Option<usize>,
    /// Why the picker is open; decides the caller's handling of `Picked`.
    purpose: Option<PickerPurpose>,
    current: PathBuf,
    path_input: String,
    entries: Vec<String>,
    truncated: bool,
    error: String,
    drives: Vec<String>,
}

/// One picker interaction result delivered from `show`.
pub enum PickerEvent {
    /// Operator confirmed a folder. `purpose` tells the caller what it is for.
    Picked {
        lane_id: usize,
        folder: PathBuf,
        purpose: PickerPurpose,
    },
    /// Dialog stays open / nothing chosen this frame.
    None,
}

impl FolderPicker {
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// Open the browser to choose the lane's browsing folder.
    pub fn open(&mut self, lane_id: usize, start: &str) {
        self.open_for_purpose(lane_id, start, PickerPurpose::LaneFolder);
    }

    /// Open the browser for an explicit purpose (WP-074 destinations), starting
    /// at `start` when it exists, otherwise at the first available drive root.
    pub fn open_for_purpose(&mut self, lane_id: usize, start: &str, purpose: PickerPurpose) {
        self.purpose = Some(purpose);
        self.drives = media_explorer::filesystem_roots();
        let start_path = Path::new(start.trim());
        let initial = if !start.trim().is_empty() && start_path.is_dir() {
            start_path.to_path_buf()
        } else {
            self.drives
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        self.open_for = Some(lane_id);
        self.navigate(initial);
    }

    pub fn close(&mut self) {
        self.open_for = None;
        self.purpose = None;
        self.entries.clear();
        self.error.clear();
    }

    fn navigate(&mut self, target: PathBuf) {
        self.current = target;
        self.path_input = self.current.to_string_lossy().to_string();
        self.refresh_entries();
    }

    fn refresh_entries(&mut self) {
        self.entries.clear();
        self.truncated = false;
        self.error.clear();
        let read = match std::fs::read_dir(&self.current) {
            Ok(read) => read,
            Err(err) => {
                self.error = format!("Cannot read folder: {err}");
                return;
            }
        };
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if self.entries.len() >= MAX_ENTRIES {
                self.truncated = true;
                break;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                self.entries.push(name.to_string());
            }
        }
        self.entries
            .sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    }

    /// Render the dialog (when open). Returns Picked when the operator
    /// confirms a folder; the dialog closes itself on pick/cancel.
    ///
    /// Sizing rule: every width/height in here is pinned to constants.
    /// egui windows auto-size to their content each frame, so any content
    /// sized from `available_*` (or a fill-available scroll area) makes
    /// content >= window every frame and the window grows without bound.
    pub fn show(&mut self, ctx: &egui::Context) -> PickerEvent {
        let Some(lane_id) = self.open_for else {
            return PickerEvent::None;
        };
        let purpose = self.purpose.clone().unwrap_or(PickerPurpose::LaneFolder);

        const DIALOG_W: f32 = 540.0;
        const LIST_H: f32 = 300.0;
        // Reserve for "⬆ Up" + "Go" buttons + item spacing in the path row.
        const PATH_ROW_RESERVE: f32 = 150.0;

        let mut picked: Option<PathBuf> = None;
        let mut cancel = false;
        let mut nav_target: Option<PathBuf> = None;
        let mut open_flag = true;

        let center = ctx.screen_rect().center();
        egui::Window::new(purpose.title(lane_id))
            .id(egui::Id::new("folder_picker_window"))
            .open(&mut open_flag)
            .collapsible(false)
            .resizable(false)
            .default_pos(center - egui::vec2(DIALOG_W / 2.0, 220.0))
            .show(ctx, |ui| {
                // Pin the content width once; everything below lays out
                // inside this fixed box, so the window cannot feedback-grow.
                ui.set_width(DIALOG_W);

                // Drive row (wraps inside the pinned width).
                ui.horizontal_wrapped(|ui| {
                    for drive in &self.drives {
                        if ui.button(drive.trim_end_matches('\\')).clicked() {
                            nav_target = Some(PathBuf::from(drive));
                        }
                    }
                });
                theme::hairline(ui);

                // Path row: Up + editable path + Go.
                ui.horizontal(|ui| {
                    if ui
                        .button(format!("{} Up", egui_phosphor::regular::ARROW_UP))
                        .clicked()
                    {
                        if let Some(parent) = self.current.parent() {
                            nav_target = Some(parent.to_path_buf());
                        }
                    }
                    let edit = egui::TextEdit::singleline(&mut self.path_input)
                        .desired_width(DIALOG_W - PATH_ROW_RESERVE);
                    let resp = ui.add(edit);
                    let go = ui.button("Go").clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go {
                        let target = PathBuf::from(self.path_input.trim());
                        if target.is_dir() {
                            nav_target = Some(target);
                        } else {
                            self.error = "Not a folder.".to_string();
                        }
                    }
                });

                if !self.error.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.error).color(theme::error_ink()),
                        )
                        .truncate(true),
                    );
                }

                // Subfolder list: fixed height, width bounded by the pinned box.
                theme::well_frame().show(ui, |ui| {
                    ui.set_width(DIALOG_W - 10.0);
                    ui.set_height(LIST_H);
                    egui::ScrollArea::vertical()
                        .id_source("folder_picker_list")
                        .max_height(LIST_H)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.entries.is_empty() && self.error.is_empty() {
                                ui.label(
                                    egui::RichText::new("(no subfolders)")
                                        .color(theme::ink_faint()),
                                );
                            }
                            for name in &self.entries {
                                if ui
                                    .add(egui::SelectableLabel::new(
                                        false,
                                        egui::RichText::new(format!(
                                            "{} {name}",
                                            egui_phosphor::regular::FOLDER
                                        )),
                                    ))
                                    .clicked()
                                {
                                    nav_target = Some(self.current.join(name));
                                }
                            }
                            if self.truncated {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "(listing capped at {MAX_ENTRIES} folders)"
                                    ))
                                    .color(theme::ink_faint()),
                                );
                            }
                        });
                });

                // Action row.
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, purpose.commit_label()).clicked() {
                        picked = Some(self.current.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    ui.label(
                        egui::RichText::new("Click a folder to enter it")
                            .small()
                            .color(theme::ink_faint()),
                    );
                });
            });

        if let Some(target) = nav_target {
            self.navigate(target);
        }
        if let Some(folder) = picked {
            self.close();
            return PickerEvent::Picked {
                lane_id,
                folder,
                purpose,
            };
        }
        if cancel || !open_flag {
            self.close();
        }
        PickerEvent::None
    }
}
