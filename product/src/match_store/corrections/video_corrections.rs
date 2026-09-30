use super::super::video::{StoredVideoObservation, VideoTrackSnapshot, VIDEO_OBSERVATION_TABLE};
use super::*;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackCorrectionAction {
    Assign,
    Reassign,
    Remove,
    Ignore,
    NotAPerson,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VideoTrackCorrectionPreview {
    pub preview_id: String,
    pub track: VideoTrackSnapshot,
    pub batch: Option<BatchCorrectionPreview>,
    pub person_id: Option<String>,
    pub action: TrackCorrectionAction,
    pub operation_id: String,
    pub planned_at: String,
    pub fences: Vec<CorrectionFence>,
    pub rows: Vec<CorrectionRowDelta>,
    pub required_reversible_rows: usize,
    pub within_limit: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VideoTrackSplitPreview {
    pub preview_id: String,
    pub track: VideoTrackSnapshot,
    pub selected_observation_ids: Vec<String>,
    pub new_track_id: String,
    pub operation_id: String,
    pub planned_at: String,
    pub rows: Vec<CorrectionRowDelta>,
}
fn video_digest<T: Serialize>(value: &T) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|e| e.to_string())?)
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_video::{VideoDetection, VideoFrame, VideoPolicy, VideoTime, VideoTracker};

    #[test]
    fn wp086_track_assign_split_restart_and_atomic_undo_preserve_faces() {
        let root = std::env::temp_dir().join(format!(
            "facial-wp086-track-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Track Person", Vec::new()).unwrap();
        let fingerprint = "a".repeat(64);
        let mut tracker =
            VideoTracker::new(fingerprint.clone(), 0, VideoPolicy::default()).unwrap();
        let mut observations = Vec::new();
        for pts in [0, 500, 1000] {
            observations.extend(
                tracker
                    .ingest(VideoFrame {
                        stream_index: 0,
                        playback_origin: VideoTime::default(),
                        time: VideoTime {
                            pts,
                            numerator: 1,
                            denominator: 1000,
                        },
                        frame_sha256: format!("{:064x}", pts + 1),
                        scene_score: 0.0,
                        detections: vec![VideoDetection {
                            source_index: 0,
                            bounds: [0.1, 0.1, 0.3, 0.3],
                            quality: 0.9,
                            pose_bucket: "frontal".into(),
                            detector_generation: "fixture-detector".into(),
                        }],
                    })
                    .unwrap()
                    .observations,
            );
        }
        let track_id = observations[0].track_id.clone();
        let mut rows = Vec::new();
        for (i, observation) in observations.iter().enumerate() {
            let face_id = format!("video-face-{}", observation.observation_id);
            let face = FaceObservation {
                face_id: face_id.clone(),
                media_key: "clips/track.mp4".into(),
                media_fingerprint: fingerprint.clone(),
                source_index: i as u32,
                source_width: None,
                source_height: None,
                exif_orientation: None,
                bounds_normalized: observation.detection.bounds.to_vec(),
                landmarks_normalized: Vec::new(),
                alignment_valid: false,
                quality: 0.9,
                pose_bucket: "frontal".into(),
                operator_owned: false,
                schema_generation: MATCH_SCHEMA_GENERATION.into(),
                face_revision: 1,
                created_at: now(),
                updated_at: now(),
            };
            let row = StoredVideoObservation {
                observation_id: observation.observation_id.clone(),
                face_id: face_id.clone(),
                track_id: track_id.clone(),
                media_key: face.media_key.clone(),
                media_fingerprint: fingerprint.clone(),
                revision: 1,
                closed: i == 2,
                exemplar: i == 0,
                payload: serde_json::to_string(observation).unwrap(),
            };
            rows.push((
                FACE_TABLE.to_string(),
                face_id,
                serde_json::to_value(face).unwrap(),
            ));
            rows.push((
                VIDEO_OBSERVATION_TABLE.to_string(),
                row.observation_id.clone(),
                serde_json::to_value(row).unwrap(),
            ));
        }
        {
            let _guard = store.mutation_write_guard("video test fixture").unwrap();
            store.commit_owned_unlocked(&rows, &[]).unwrap();
        }
        assert_eq!(
            store
                .pending_video_exemplars("clips/track.mp4", 0, "fixture-model")
                .unwrap()
                .len(),
            1
        );
        let exemplar_face_id = format!("video-face-{}", observations[0].observation_id);
        let mut face: FaceObservation = store
            .require(FACE_TABLE, &exemplar_face_id, "exemplar Face")
            .unwrap();
        face.alignment_valid = true;
        store
            .upsert_json(FACE_TABLE, &exemplar_face_id, &face)
            .unwrap();
        let mut embedding = FaceEmbedding {
            embedding_id: embedding_id(&exemplar_face_id, "fixture-model"),
            face_id: exemplar_face_id.clone(),
            vector: {
                let mut vector = vec![0.0; 512];
                vector[0] = 1.0;
                vector
            },
            model_generation: "fixture-model".into(),
            schema_generation: MATCH_SCHEMA_GENERATION.into(),
            media_fingerprint: fingerprint.clone(),
            face_revision: 1,
            job_id: "fixture-job".into(),
            active: true,
            created_at: now(),
        };
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        assert!(store
            .pending_video_exemplars("clips/track.mp4", 0, "fixture-model")
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .video_exemplar_face_ids_page("clips/track.mp4", "fixture-model", "", 16)
                .unwrap(),
            vec![exemplar_face_id.clone()]
        );
        embedding.active = false;
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        assert_eq!(
            store
                .pending_video_exemplars("clips/track.mp4", 0, "fixture-model")
                .unwrap()
                .len(),
            1
        );
        embedding.active = true;
        embedding.face_revision = 2;
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        assert_eq!(
            store
                .pending_video_exemplars("clips/track.mp4", 0, "fixture-model")
                .unwrap()
                .len(),
            1
        );
        embedding.face_revision = 1;
        embedding.media_fingerprint = "b".repeat(64);
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        assert_eq!(
            store
                .pending_video_exemplars("clips/track.mp4", 0, "fixture-model")
                .unwrap()
                .len(),
            1
        );
        {
            let _guard = store
                .mutation_write_guard("remove query fixture vector")
                .unwrap();
            store
                .commit_owned_unlocked(&[], &[(EMBEDDING_TABLE.into(), embedding.embedding_id)])
                .unwrap();
        }
        let assign = store
            .preview_video_track_correction(
                &track_id,
                TrackCorrectionAction::Assign,
                Some(&person.person_id),
            )
            .unwrap();
        assert_eq!(assign.track.observation_count, 3);
        let assigned = store.apply_video_track_correction(&assign).unwrap();
        let selected = vec![observations[0].observation_id.clone()];
        let split = store
            .preview_video_track_split(&track_id, &selected)
            .unwrap();
        let split_receipt = store.apply_video_track_split(&split).unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(
            store.video_media_tracks("clips/track.mp4").unwrap().len(),
            2
        );
        for observation in &observations {
            let assignment: Assignment = store
                .require(
                    ASSIGNMENT_TABLE,
                    &format!("video-face-{}", observation.observation_id),
                    "assignment",
                )
                .unwrap();
            assert_eq!(assignment.person_id, person.person_id);
        }
        store.undo_correction(&split_receipt.operation_id).unwrap();
        assert_eq!(
            store.video_media_tracks("clips/track.mp4").unwrap().len(),
            1
        );
        store.undo_correction(&assigned.operation_id).unwrap();
        for observation in &observations {
            assert!(store
                .get_one::<Assignment>(
                    ASSIGNMENT_TABLE,
                    &format!("video-face-{}", observation.observation_id)
                )
                .unwrap()
                .is_none());
        }
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
impl MatchStore {
    pub fn preview_video_track_correction(
        &self,
        track_id: &str,
        action: TrackCorrectionAction,
        person_id: Option<&str>,
    ) -> Result<VideoTrackCorrectionPreview, String> {
        let _guard = self.correction_write_guard("video correction preview")?;
        self.plan_video_correction_unlocked(track_id, action, person_id, new_id("operation"), now())
    }
    fn plan_video_correction_unlocked(
        &self,
        track_id: &str,
        action: TrackCorrectionAction,
        person_id: Option<&str>,
        operation_id: String,
        planned_at: String,
    ) -> Result<VideoTrackCorrectionPreview, String> {
        let track = self.video_track_snapshot_unlocked(track_id)?;
        if !track.closed {
            return Err(
                "finish or pause and finalize video tracking before track correction".into(),
            );
        }
        let persons = person_id.map(|id| vec![id.to_string()]).unwrap_or_default();
        let fences = track
            .face_ids
            .iter()
            .map(|id| self.correction_fence_for_unlocked(id, persons.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut batch = None;
        let mut rows = Vec::new();
        if action == TrackCorrectionAction::Assign {
            let person_id = person_id.ok_or("video Assign requires an explicit Person")?;
            let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
            for (face_id, fence) in track.face_ids.iter().zip(&fences) {
                let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
                if self
                    .get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?
                    .is_some()
                {
                    return Err("video Assign requires unassigned observations; use Reassign or split the track".into());
                }
                if self
                    .get_one_unlocked::<CannotLinkConstraint>(
                        CONSTRAINT_TABLE,
                        &cannot_link_id(face_id, person_id),
                    )?
                    .is_some()
                {
                    return Err("video Assign conflicts with cannot-link".into());
                }
                let assignment = Assignment {
                    assignment_id: face_id.clone(),
                    face_id: face_id.clone(),
                    person_id: person_id.into(),
                    media_key: face.media_key,
                    look_id: None,
                    placement: "unsorted".into(),
                    state: AssignmentState::OperatorConfirmed.as_str().into(),
                    provenance: "video_assign".into(),
                    locked: true,
                    model_generation: None,
                    calibration_generation: None,
                    envelope_hash: None,
                    face_revision: face.face_revision,
                    person_revision: person.revision,
                    operation_id: operation_id.clone(),
                    created_at: planned_at.clone(),
                    updated_at: planned_at.clone(),
                };
                rows.push(CorrectionRowDelta {
                    table: CorrectionTable::Assignment,
                    stable_id: face_id.clone(),
                    before: None,
                    after: Some(serde_json::to_value(assignment).map_err(|e| e.to_string())?),
                });
                self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
            }
        } else {
            let action = match action {
                TrackCorrectionAction::Reassign => BatchCorrectionAction::ChangePerson,
                TrackCorrectionAction::Remove => BatchCorrectionAction::RemoveAssignment,
                TrackCorrectionAction::Ignore => BatchCorrectionAction::IgnoreFace,
                TrackCorrectionAction::NotAPerson => BatchCorrectionAction::NotAFace,
                _ => unreachable!(),
            };
            let source = if matches!(
                action,
                BatchCorrectionAction::ChangePerson | BatchCorrectionAction::RemoveAssignment
            ) {
                let mut sources = BTreeSet::new();
                for face_id in &track.face_ids {
                    let assignment: Assignment = self.require_unlocked(
                        ASSIGNMENT_TABLE,
                        face_id,
                        "assigned track observation",
                    )?;
                    sources.insert(assignment.person_id);
                }
                if sources.len() != 1 {
                    return Err("mixed track assignment requires split before correction".into());
                }
                sources.into_iter().next()
            } else {
                None
            };
            let target = if action == BatchCorrectionAction::ChangePerson {
                Some(
                    person_id
                        .ok_or("video Reassign requires target Person")?
                        .to_string(),
                )
            } else {
                if action == BatchCorrectionAction::RemoveAssignment {
                    if person_id.is_some_and(|id| Some(id) != source.as_deref()) {
                        return Err("video Remove source Person changed".into());
                    }
                } else if person_id.is_some() {
                    return Err("this video correction does not accept a Person".into());
                }
                None
            };
            let plan = self.plan_batch_correction_unlocked(
                action,
                source,
                target,
                track.face_ids.clone(),
                fences.clone(),
                operation_id.clone(),
                planned_at.clone(),
            )?;
            rows = plan.rows;
            batch = Some(plan.preview);
        }
        let required_reversible_rows = rows.len();
        let mut preview = VideoTrackCorrectionPreview {
            preview_id: String::new(),
            track,
            batch,
            person_id: person_id.map(str::to_string),
            action,
            operation_id,
            planned_at,
            fences,
            rows,
            required_reversible_rows,
            within_limit: required_reversible_rows <= CORRECTION_ROW_LIMIT,
        };
        preview.preview_id = video_digest(&preview)?;
        Ok(preview)
    }
    pub fn apply_video_track_correction(
        &self,
        preview: &VideoTrackCorrectionPreview,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("video correction apply")?;
        let recomputed = self.plan_video_correction_unlocked(
            &preview.track.track_id,
            preview.action,
            preview.person_id.as_deref(),
            preview.operation_id.clone(),
            preview.planned_at.clone(),
        )?;
        if recomputed != *preview {
            return Err("stale video track correction preview".into());
        }
        ensure_within_delta_limit(preview.required_reversible_rows)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let kind = preview
            .batch
            .as_ref()
            .map(|b| b.action.as_str())
            .unwrap_or("video_assign");
        self.commit_correction_unlocked(
            &preview.operation_id,
            kind,
            None,
            preview.person_id.as_deref(),
            preview.rows.clone(),
            preview.track.face_ids.clone(),
            vec![preview.track.media_key.clone()],
            true,
            false,
            execution,
        )
    }
    pub fn preview_video_track_split(
        &self,
        track_id: &str,
        observation_ids: &[String],
    ) -> Result<VideoTrackSplitPreview, String> {
        let _guard = self.correction_write_guard("video split preview")?;
        self.plan_video_split_unlocked(track_id, observation_ids, new_id("operation"), now())
    }
    fn plan_video_split_unlocked(
        &self,
        track_id: &str,
        observation_ids: &[String],
        operation_id: String,
        planned_at: String,
    ) -> Result<VideoTrackSplitPreview, String> {
        let track = self.video_track_snapshot_unlocked(track_id)?;
        if !track.closed {
            return Err("video track split requires a closed track".into());
        }
        let selected: BTreeSet<_> = observation_ids.iter().cloned().collect();
        if selected.is_empty()
            || selected.len() != observation_ids.len()
            || selected.len() >= track.observation_count
        {
            return Err("video split requires a distinct proper subset of observations".into());
        }
        let members: BTreeSet<_> = track
            .observations
            .iter()
            .map(|o| o.observation_id.clone())
            .collect();
        if !selected.is_subset(&members) {
            return Err("video split contains observation outside track".into());
        }
        let new_track_id = video_digest(&(track_id, &selected, &operation_id))?;
        let mut rows = Vec::new();
        for mut row in self.video_rows_unlocked("track_id", track_id)? {
            let before = serde_json::to_value(&row).map_err(|e| e.to_string())?;
            row.revision = row
                .revision
                .checked_add(1)
                .ok_or("video track revision overflow")?;
            // Both partitions are closed; no automatic association resumes either partition.
            row.closed = true;
            if selected.contains(&row.observation_id) {
                let mut observation = row.observation()?;
                row.track_id = new_track_id.clone();
                observation.track_id = new_track_id.clone();
                row.payload = serde_json::to_string(&observation).map_err(|e| e.to_string())?;
            }
            rows.push(CorrectionRowDelta {
                table: CorrectionTable::VideoObservation,
                stable_id: row.observation_id.clone(),
                before: Some(before),
                after: Some(serde_json::to_value(row).map_err(|e| e.to_string())?),
            });
        }
        validate_delta_bound(&rows)?;
        let mut preview = VideoTrackSplitPreview {
            preview_id: String::new(),
            track,
            selected_observation_ids: selected.into_iter().collect(),
            new_track_id,
            operation_id,
            planned_at,
            rows,
        };
        preview.preview_id = video_digest(&preview)?;
        Ok(preview)
    }
    pub fn apply_video_track_split(
        &self,
        preview: &VideoTrackSplitPreview,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("video split apply")?;
        if self.plan_video_split_unlocked(
            &preview.track.track_id,
            &preview.selected_observation_ids,
            preview.operation_id.clone(),
            preview.planned_at.clone(),
        )? != *preview
        {
            return Err("stale video split preview".into());
        }
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        self.commit_correction_unlocked(
            &preview.operation_id,
            "video_track_split",
            None,
            None,
            preview.rows.clone(),
            preview.track.face_ids.clone(),
            vec![preview.track.media_key.clone()],
            true,
            false,
            execution,
        )
    }
}
