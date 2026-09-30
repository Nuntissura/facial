//! Transient, presentation-only Match face editor state (WP-084).
//!
//! Identity truth never lives here.  The durable store owns assignments,
//! constraints, and operation history; this module only prevents an immediate-
//! mode UI from carrying a draft or implicit autocomplete choice across assets,
//! tabs, Settings, or immersive Viewer transitions.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ManualRegionDraft {
    pub start_normalized: [f32; 2],
    pub end_normalized: [f32; 2],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchFaceEditorState {
    media_key: Option<String>,
    active: bool,
    selected_face_id: Option<String>,
    hovered_face_id: Option<String>,
    autocomplete_query: String,
    selected_person_id: Option<String>,
    selected_look_id: Option<String>,
    new_look_name: String,
    drawing_manual_region: bool,
    manual_region: Option<ManualRegionDraft>,
}

impl MatchFaceEditorState {
    pub fn active(&self) -> bool {
        self.active
    }

    pub fn media_key(&self) -> Option<&str> {
        self.media_key.as_deref()
    }

    pub fn selected_face_id(&self) -> Option<&str> {
        self.selected_face_id.as_deref()
    }

    pub fn hovered_face_id(&self) -> Option<&str> {
        self.hovered_face_id.as_deref()
    }

    pub fn autocomplete_query(&self) -> &str {
        &self.autocomplete_query
    }

    pub fn selected_person_id(&self) -> Option<&str> {
        self.selected_person_id.as_deref()
    }

    pub fn selected_look_id(&self) -> Option<&str> {
        self.selected_look_id.as_deref()
    }

    pub fn new_look_name(&self) -> &str {
        &self.new_look_name
    }

    pub fn manual_region(&self) -> Option<&ManualRegionDraft> {
        self.manual_region.as_ref()
    }

    pub fn drawing_manual_region(&self) -> bool {
        self.drawing_manual_region
    }

    pub fn enter(&mut self, media_key: &str) -> Result<(), String> {
        if media_key.trim().is_empty() {
            return Err("Edit faces requires a selected media key".to_string());
        }
        self.discard();
        self.media_key = Some(media_key.to_string());
        self.active = true;
        Ok(())
    }

    /// Reconcile the exact context in which an editor is allowed to exist.
    /// Returns true when transient state was discarded.
    pub fn reconcile_context(
        &mut self,
        media_key: Option<&str>,
        media_tab_active: bool,
        settings_open: bool,
        immersive_fullscreen: bool,
    ) -> bool {
        let invalid = !media_tab_active
            || settings_open
            || immersive_fullscreen
            || self.media_key.as_deref() != media_key;
        if self.active && invalid {
            self.discard();
            true
        } else {
            false
        }
    }

    pub fn discard(&mut self) {
        *self = Self::default();
    }

    pub fn select_face(&mut self, face_id: &str) -> Result<(), String> {
        self.require_active()?;
        if face_id.trim().is_empty() {
            return Err("selected FaceId cannot be empty".to_string());
        }
        self.selected_face_id = Some(face_id.to_string());
        self.selected_person_id = None;
        self.selected_look_id = None;
        self.new_look_name.clear();
        Ok(())
    }

    pub fn set_hovered_face(&mut self, face_id: Option<&str>) {
        self.hovered_face_id = face_id
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
    }

    pub fn set_autocomplete_query(&mut self, query: String) -> Result<(), String> {
        self.require_active()?;
        self.autocomplete_query = query;
        // Query changes never preserve or infer a Person selection.
        self.selected_person_id = None;
        self.selected_look_id = None;
        self.new_look_name.clear();
        Ok(())
    }

    /// Only an explicit row activation selects a stable Person ID.
    pub fn choose_person(&mut self, person_id: &str) -> Result<(), String> {
        self.require_active()?;
        if self.selected_face_id.is_none() && self.manual_region.is_none() {
            return Err(
                "choose a face or finish a manual region before choosing a Person".to_string(),
            );
        }
        if person_id.trim().is_empty() {
            return Err("selected Person ID cannot be empty".to_string());
        }
        self.selected_person_id = Some(person_id.to_string());
        self.selected_look_id = None;
        self.new_look_name.clear();
        Ok(())
    }

    pub fn choose_look(&mut self, look_id: &str) -> Result<(), String> {
        self.require_active()?;
        if self.selected_face_id.is_none() {
            return Err("choose a face before choosing a Look".to_string());
        }
        if look_id.trim().is_empty() {
            return Err("selected Look ID cannot be empty".to_string());
        }
        self.selected_look_id = Some(look_id.to_string());
        self.new_look_name.clear();
        Ok(())
    }

    pub fn set_new_look_name(&mut self, name: String) -> Result<(), String> {
        self.require_active()?;
        if self.selected_face_id.is_none() {
            return Err("choose a face before naming a new Look".to_string());
        }
        self.new_look_name = name;
        self.selected_look_id = None;
        Ok(())
    }

    pub fn set_manual_region(&mut self, draft: ManualRegionDraft) -> Result<(), String> {
        self.require_active()?;
        for point in [draft.start_normalized, draft.end_normalized] {
            if point
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            {
                return Err("manual face draft must use finite normalized coordinates".to_string());
            }
        }
        self.manual_region = Some(draft);
        Ok(())
    }

    pub fn begin_manual_region(&mut self) -> Result<(), String> {
        self.require_active()?;
        self.drawing_manual_region = true;
        self.manual_region = None;
        self.selected_face_id = None;
        self.selected_person_id = None;
        self.selected_look_id = None;
        self.new_look_name.clear();
        Ok(())
    }

    pub fn finish_manual_region(&mut self) -> Result<(), String> {
        self.require_active()?;
        if self.manual_region.is_none() {
            return Err("draw a manual face region before continuing".to_string());
        }
        self.drawing_manual_region = false;
        Ok(())
    }

    fn require_active(&self) -> Result<(), String> {
        if self.active {
            Ok(())
        } else {
            Err("Edit faces is not active".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> MatchFaceEditorState {
        let mut state = MatchFaceEditorState::default();
        state.enter("media/a.jpg").unwrap();
        state.select_face("face-a").unwrap();
        state
            .set_autocomplete_query("duplicate name".to_string())
            .unwrap();
        state.choose_person("person-stable-b").unwrap();
        state
            .set_manual_region(ManualRegionDraft {
                start_normalized: [0.1, 0.2],
                end_normalized: [0.7, 0.8],
            })
            .unwrap();
        state
    }

    #[test]
    fn every_context_boundary_discards_the_complete_draft() {
        for (key, media, settings, fullscreen) in [
            (Some("media/b.jpg"), true, false, false),
            (Some("media/a.jpg"), false, false, false),
            (Some("media/a.jpg"), true, true, false),
            (Some("media/a.jpg"), true, false, true),
            (None, true, false, false),
        ] {
            let mut state = populated();
            assert!(state.reconcile_context(key, media, settings, fullscreen));
            assert_eq!(state, MatchFaceEditorState::default());
        }
    }

    #[test]
    fn fullscreen_exit_never_resurrects_a_discarded_editor() {
        let mut state = populated();
        assert!(state.reconcile_context(Some("media/a.jpg"), true, false, true));
        assert!(!state.reconcile_context(Some("media/a.jpg"), true, false, false));
        assert!(!state.active());
    }

    #[test]
    fn autocomplete_has_no_implicit_selection_and_query_changes_clear_choice() {
        let mut state = MatchFaceEditorState::default();
        state.enter("media/a.jpg").unwrap();
        state.select_face("face-a").unwrap();
        state.set_autocomplete_query("Ada".to_string()).unwrap();
        assert_eq!(state.selected_person_id(), None);
        state.choose_person("person-2").unwrap();
        assert_eq!(state.selected_person_id(), Some("person-2"));
        state.set_autocomplete_query("Ada L".to_string()).unwrap();
        assert_eq!(state.selected_person_id(), None);
    }

    #[test]
    fn manual_region_rejects_non_normalized_or_non_finite_coordinates() {
        let mut state = MatchFaceEditorState::default();
        state.enter("media/a.jpg").unwrap();
        for bad in [[-0.1, 0.2], [1.1, 0.2], [f32::NAN, 0.2]] {
            assert!(state
                .set_manual_region(ManualRegionDraft {
                    start_normalized: bad,
                    end_normalized: [0.7, 0.8],
                })
                .is_err());
        }
    }

    #[test]
    fn finished_manual_region_accepts_only_an_explicit_person_choice() {
        let mut state = MatchFaceEditorState::default();
        state.enter("media/a.jpg").unwrap();
        state.begin_manual_region().unwrap();
        state
            .set_manual_region(ManualRegionDraft {
                start_normalized: [0.1, 0.2],
                end_normalized: [0.4, 0.6],
            })
            .unwrap();
        state.finish_manual_region().unwrap();
        assert_eq!(state.selected_person_id(), None);
        state.choose_person("person-stable-b").unwrap();
        assert_eq!(state.selected_person_id(), Some("person-stable-b"));
    }

    #[test]
    fn look_drafts_are_explicit_and_discarded_with_the_editor() {
        let mut state = MatchFaceEditorState::default();
        state.enter("media/a.jpg").unwrap();
        state.select_face("face-a").unwrap();
        state.choose_look("look-stable-a").unwrap();
        assert_eq!(state.selected_look_id(), Some("look-stable-a"));
        state.set_new_look_name("Side profile".to_string()).unwrap();
        assert_eq!(state.selected_look_id(), None);
        assert_eq!(state.new_look_name(), "Side profile");
        state.discard();
        assert_eq!(state, MatchFaceEditorState::default());
    }
}
