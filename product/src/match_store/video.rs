//! Canonical video observations, bounded track projections and resumable sampling.
use super::*;
use crate::match_video::{
    VideoCheckpoint, VideoFrame, VideoObservation, VideoPolicy, VideoTime, VideoTracker,
};

pub(super) const VIDEO_OBSERVATION_TABLE: &str = "match_video_observation";
pub(super) const VIDEO_CHECKPOINT_TABLE: &str = "match_video_checkpoint";
pub(super) const WP086_VIDEO_SCHEMA_SQL: &str = r#"
DEFINE TABLE OVERWRITE match_video_observation SCHEMAFULL;
DEFINE FIELD OVERWRITE observation_id ON match_video_observation TYPE string;
DEFINE FIELD OVERWRITE face_id ON match_video_observation TYPE string;
DEFINE FIELD OVERWRITE track_id ON match_video_observation TYPE string;
DEFINE FIELD OVERWRITE media_key ON match_video_observation TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON match_video_observation TYPE string;
DEFINE FIELD OVERWRITE revision ON match_video_observation TYPE int;
DEFINE FIELD OVERWRITE closed ON match_video_observation TYPE bool;
DEFINE FIELD OVERWRITE exemplar ON match_video_observation TYPE bool;
DEFINE FIELD OVERWRITE payload ON match_video_observation TYPE string;
DEFINE INDEX OVERWRITE match_video_observation_id ON match_video_observation FIELDS observation_id UNIQUE;
DEFINE INDEX OVERWRITE match_video_observation_face ON match_video_observation FIELDS face_id UNIQUE;
DEFINE INDEX OVERWRITE match_video_observation_track ON match_video_observation FIELDS track_id, observation_id;
DEFINE INDEX OVERWRITE match_video_observation_media ON match_video_observation FIELDS media_key, observation_id;
DEFINE TABLE OVERWRITE match_video_checkpoint SCHEMAFULL;
DEFINE FIELD OVERWRITE checkpoint_id ON match_video_checkpoint TYPE string;
DEFINE FIELD OVERWRITE media_key ON match_video_checkpoint TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON match_video_checkpoint TYPE string;
DEFINE FIELD OVERWRITE policy_json ON match_video_checkpoint TYPE string;
DEFINE FIELD OVERWRITE payload ON match_video_checkpoint TYPE string;
DEFINE INDEX OVERWRITE match_video_checkpoint_id ON match_video_checkpoint FIELDS checkpoint_id UNIQUE;
"#;
const TRACK_ROWS_LIMIT: usize = 4096;
// Hard tracker limits: 128 active tracks, first/last plus 16 exemplars per
// track, each bounded like a canonical observation, plus track/header fields.
const CHECKPOINT_PAYLOAD_BYTES_LIMIT: usize = 128 * (18 * 16 * 1024 + 1024) + 4096;

#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoredVideoObservation {
    pub observation_id: String,
    pub face_id: String,
    pub track_id: String,
    pub media_key: String,
    pub media_fingerprint: String,
    pub revision: u64,
    pub closed: bool,
    pub exemplar: bool,
    pub payload: String,
}
impl StoredVideoObservation {
    pub fn observation(&self) -> Result<VideoObservation, String> {
        if self.payload.len() > 16 * 1024 {
            return Err("video observation payload exceeds bound".into());
        }
        let observation: VideoObservation =
            serde_json::from_str(&self.payload).map_err(|e| e.to_string())?;
        observation.validate_for_media(
            canonical_media_sha256(&self.media_fingerprint)
                .ok_or("invalid video media fingerprint")?,
        )?;
        if observation.observation_id != self.observation_id
            || observation.track_id != self.track_id
            || self.face_id != format!("video-face-{}", self.observation_id)
            || self.revision == 0
            || canonical_media_sha256(&self.media_fingerprint).is_none()
            || self.media_key.is_empty()
        {
            return Err("video observation stable identity mismatch".into());
        }
        Ok(observation)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue)]
struct StoredVideoCheckpoint {
    checkpoint_id: String,
    media_key: String,
    media_fingerprint: String,
    policy_json: String,
    payload: String,
}
impl StoredVideoCheckpoint {
    fn tracker_for(&self, media_key: &str, stream_index: u32) -> Result<VideoTracker, String> {
        if self.policy_json.len() > 4096 || self.payload.len() > CHECKPOINT_PAYLOAD_BYTES_LIMIT {
            return Err("video checkpoint encoded payload exceeds bound".into());
        }
        if self.checkpoint_id != format!("{media_key}:{stream_index}")
            || self.media_key != media_key
        {
            return Err("video checkpoint record identity mismatch".into());
        }
        let tracker = VideoTracker::restore(
            serde_json::from_str(&self.policy_json).map_err(|e| e.to_string())?,
            serde_json::from_str(&self.payload).map_err(|e| e.to_string())?,
        )?;
        let state = tracker.checkpoint();
        if canonical_media_sha256(&self.media_fingerprint) != Some(state.media_sha256.as_str())
            || state.stream_index != stream_index
        {
            return Err("video checkpoint source or stream mismatch".into());
        }
        Ok(tracker)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VideoTrackObservationRow {
    pub observation_id: String,
    pub face_id: String,
    pub time: VideoTime,
    pub exemplar: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VideoTrackSnapshot {
    pub track_id: String,
    pub media_key: String,
    pub media_fingerprint: String,
    pub revision: u64,
    pub stream_index: u32,
    pub playback_origin: VideoTime,
    pub start: VideoTime,
    pub end: VideoTime,
    pub observation_count: usize,
    pub exemplar_count: usize,
    pub timestamps: Vec<VideoTime>,
    pub face_ids: Vec<String>,
    pub closed: bool,
    pub observations: Vec<VideoTrackObservationRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonAppearanceCursor {
    pub person_id: String,
    pub person_revision: u64,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub after_assignment_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersonVideoAppearance {
    pub media_key: String,
    pub media_fingerprint: String,
    pub source_path: Option<String>,
    pub track_id: String,
    pub track_revision: u64,
    pub observation_id: String,
    pub timestamp: VideoTime,
    pub playback_origin: VideoTime,
    pub seek_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersonAppearancePage {
    pub rows: Vec<PersonVideoAppearance>,
    pub next_cursor: Option<PersonAppearanceCursor>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoExemplarEmbedding {
    pub observation_id: String,
    pub frame_sha256: String,
    pub time: VideoTime,
    pub stream_index: u32,
    pub model_generation: String,
    pub vector: Vec<f32>,
    pub bounds: [f32; 4],
    pub landmarks: [[f32; 2]; 5],
    pub source_width: u32,
    pub source_height: u32,
}

impl MatchStore {
    // Checkpoints are regenerable. Recover the identity seed from surviving
    // observations after import/rebuild rather than deriving it from a moved path.
    fn video_source_namespace_unlocked(
        &self,
        media_key: &str,
        fingerprint: &str,
        stream_index: u32,
    ) -> Result<Option<String>, String> {
        let mut recovered = None;
        let mut after = String::new();
        loop {
            let db = self.database();
            let key = media_key.to_string();
            let fingerprint = fingerprint.to_string();
            let cursor = after.clone();
            let rows: Vec<StoredVideoObservation> = surreal_store::run(async move {
                let mut response = db.query("SELECT * OMIT id FROM match_video_observation WITH INDEX match_video_observation_media WHERE media_key=$key AND media_fingerprint=$fingerprint AND observation_id>$after ORDER BY observation_id ASC LIMIT 128;")
                    .bind(("key", key)).bind(("fingerprint", fingerprint)).bind(("after", cursor))
                    .await.map_err(|e| e.to_string())?.check().map_err(|e| e.to_string())?;
                response.take(0).map_err(|e| e.to_string())
            })?;
            let page_len = rows.len();
            for row in rows {
                after = row.observation_id.clone();
                let observation = row.observation()?;
                if observation.stream_index != stream_index {
                    continue;
                }
                if recovered
                    .as_ref()
                    .is_some_and(|namespace| namespace != &observation.identity_namespace)
                {
                    return Err("video stream has inconsistent identity namespaces".into());
                }
                recovered = Some(observation.identity_namespace);
            }
            if page_len < 128 {
                break;
            }
        }
        if let Some(namespace) = recovered {
            return Ok(namespace);
        }
        Ok(Some(format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    "video-source-namespace-v1",
                    media_key,
                    uuid::Uuid::new_v4().to_string()
                ))
                .map_err(|e| e.to_string())?
            )
        )))
    }

    pub(super) fn video_density_family_unlocked(&self, track_id: &str) -> Result<String, String> {
        let rows = self.video_rows_unlocked("track_id", track_id)?;
        let snapshot = video_track_snapshot(rows.clone())?;
        let first_id = &snapshot.observations[0].observation_id;
        let first = rows
            .iter()
            .find(|row| &row.observation_id == first_id)
            .ok_or("video density anchor missing")?
            .observation()?;
        // Physical copies share density, but distinct temporal tracks and split
        // partitions retain their own earliest-member content anchor.
        Ok(format!(
            "video-track-content:{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    canonical_media_sha256(&snapshot.media_fingerprint)
                        .ok_or("invalid video density fingerprint")?,
                    first.stream_index,
                    &first.policy_sha256,
                    &first.shot_anchor,
                    first.playback_origin,
                    first.time,
                    first.detection.source_index,
                ))
                .map_err(|e| e.to_string())?
            )
        ))
    }
    /// Page assigned observations, not density votes. Empty filtered pages can
    /// still carry a continuation; no unbounded search for a nonempty page.
    pub fn person_video_appearances_page(
        &self,
        person_id: &str,
        cursor: Option<&PersonAppearanceCursor>,
        limit: usize,
    ) -> Result<PersonAppearancePage, String> {
        validate_text("PersonId", person_id)?;
        if !(1..=64).contains(&limit) {
            return Err("appearance page limit must be 1..64".into());
        }
        let _guard = self.database_read_guard("appearance snapshot lock poisoned")?;
        let person = self.require_unlocked::<Person>(PERSON_TABLE, person_id, "Person")?;
        let execution = self.execution_state_unlocked()?;
        if cursor.is_some_and(|c| {
            c.person_id != person_id
                || c.person_revision != person.revision
                || c.identity_revision != execution.identity_revision
                || c.catalog_revision != execution.catalog_revision
                || c.after_assignment_id.len() > 4096
                || c.after_assignment_id.chars().any(char::is_control)
        }) {
            return Err(
                "appearance cursor is stale or belongs to another Person; restart pagination"
                    .into(),
            );
        }
        let after = cursor
            .map_or("", |c| c.after_assignment_id.as_str())
            .to_string();
        let db = self.database();
        let key = person_id.to_string();
        let assignments: Vec<Assignment> = surreal_store::run(async move {
            let mut response=db.query("SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id=$person AND assignment_id>$after ORDER BY assignment_id ASC LIMIT 129;").bind(("person",key)).bind(("after",after)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
            response.take(0).map_err(|e| e.to_string())
        })?;
        let active = self.active_model_generation_unlocked()?;
        let calibration = self.current_active_calibration_unlocked(active.as_deref())?;
        let mut rows = Vec::new();
        let mut scanned = 0;
        let mut last = String::new();
        let mut revisions = BTreeMap::new();
        for assignment in assignments.iter().take(128) {
            scanned += 1;
            last = assignment.assignment_id.clone();
            let Some(face) =
                self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &assignment.face_id)?
            else {
                continue;
            };
            if assignment.assignment_id != assignment.face_id
                || assignment.person_id != person_id
                || assignment.media_key != face.media_key
                || assignment.face_revision != face.face_revision
                || assignment.person_revision != person.revision
            {
                continue;
            }
            let Some(asset) = self.canonical_job_asset_for_media_unlocked(&face.media_key)? else {
                continue;
            };
            if asset
                .source_path
                .as_ref()
                .is_some_and(|path| path.len() > 32 * 1024)
            {
                return Err("appearance source path exceeds response bound".into());
            }
            if canonical_media_sha256(&asset.media_fingerprint).is_none()
                || canonical_media_sha256(&asset.media_fingerprint)
                    != canonical_media_sha256(&face.media_fingerprint)
            {
                continue;
            }
            let manual = assignment.state == AssignmentState::OperatorConfirmed.as_str()
                && assignment.locked;
            if !manual {
                let embedding = assignment
                    .model_generation
                    .as_ref()
                    .map(|g| {
                        self.get_one_unlocked::<FaceEmbedding>(
                            EMBEDDING_TABLE,
                            &embedding_id(&face.face_id, g),
                        )
                    })
                    .transpose()?
                    .flatten();
                let provenance = embedding.as_ref().map(FaceEmbeddingProvenance::from);
                if !self.strict_assignment_provenance_is_current_unlocked(
                    assignment,
                    &face,
                    &person,
                    active.as_deref(),
                    calibration.as_ref(),
                    Some(&asset),
                    provenance.as_ref(),
                ) {
                    continue;
                }
            }
            let db = self.database();
            let face_id = face.face_id.clone();
            let observed: Vec<StoredVideoObservation> = surreal_store::run(async move {
                let mut response=db.query("SELECT * OMIT id FROM match_video_observation WITH INDEX match_video_observation_face WHERE face_id=$face LIMIT 2;").bind(("face",face_id)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                response.take(0).map_err(|e| e.to_string())
            })?;
            let [stored] = observed.as_slice() else {
                continue;
            };
            let observation = stored.observation()?;
            if stored.media_key != face.media_key
                || canonical_media_sha256(&stored.media_fingerprint)
                    != canonical_media_sha256(&face.media_fingerprint)
                || observation.detection.bounds.as_slice() != face.bounds_normalized.as_slice()
            {
                continue;
            }
            let revision = if let Some(revision) = revisions.get(&stored.track_id) {
                *revision
            } else {
                let snapshot = self.video_track_snapshot_unlocked(&stored.track_id)?;
                revisions.insert(stored.track_id.clone(), snapshot.revision);
                snapshot.revision
            };
            rows.push(PersonVideoAppearance {
                media_key: stored.media_key.clone(),
                media_fingerprint: stored.media_fingerprint.clone(),
                source_path: asset.source_path,
                track_id: stored.track_id.clone(),
                track_revision: revision,
                observation_id: stored.observation_id.clone(),
                timestamp: observation.time,
                playback_origin: observation.playback_origin,
                seek_ms: observation
                    .time
                    .playback_milliseconds(observation.playback_origin)?,
            });
            if rows.len() == limit {
                break;
            }
        }
        let has_more = scanned < assignments.len();
        let next_cursor = has_more.then(|| PersonAppearanceCursor {
            person_id: person_id.into(),
            person_revision: person.revision,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            after_assignment_id: last,
        });
        Ok(PersonAppearancePage {
            rows,
            next_cursor,
            has_more,
        })
    }
    pub(super) fn rekey_video_rows_unlocked(
        &self,
        old_key: &str,
        new_key: &str,
        fingerprint: &str,
    ) -> Result<(Vec<(String, String, Value)>, Vec<(String, String)>), String> {
        let mut upserts = Vec::new();
        let mut deletes = Vec::new();
        let mut namespaces = BTreeMap::<u32, Option<String>>::new();
        for mut row in self
            .list_unlocked::<StoredVideoObservation>(VIDEO_OBSERVATION_TABLE)?
            .into_iter()
            .filter(|row| row.media_key == old_key)
        {
            if canonical_media_sha256(&row.media_fingerprint) != Some(fingerprint) {
                return Err("video media move fingerprint mismatch".into());
            }
            let observation = row.observation()?;
            let face: FaceObservation =
                self.require_unlocked(FACE_TABLE, &row.face_id, "video Face")?;
            if face.media_key != old_key
                || canonical_media_sha256(&face.media_fingerprint) != Some(fingerprint)
                || observation.detection.bounds.as_slice() != face.bounds_normalized.as_slice()
            {
                return Err("video media move Face evidence mismatch".into());
            }
            if namespaces
                .get(&observation.stream_index)
                .is_some_and(|namespace| namespace != &observation.identity_namespace)
            {
                return Err("video stream has inconsistent identity namespaces".into());
            }
            namespaces.insert(observation.stream_index, observation.identity_namespace);
            row.media_key = new_key.into();
            upserts.push((
                VIDEO_OBSERVATION_TABLE.into(),
                row.observation_id.clone(),
                serde_json::to_value(row).map_err(|e| e.to_string())?,
            ));
        }
        for mut row in self
            .list_unlocked::<StoredVideoCheckpoint>(VIDEO_CHECKPOINT_TABLE)?
            .into_iter()
            .filter(|row| row.media_key == old_key)
        {
            if canonical_media_sha256(&row.media_fingerprint) != Some(fingerprint) {
                return Err("video checkpoint move fingerprint mismatch".into());
            }
            let stream_index = row
                .checkpoint_id
                .strip_prefix(&format!("{old_key}:"))
                .and_then(|stream| stream.parse::<u32>().ok())
                .ok_or("video checkpoint record identity mismatch")?;
            let checkpoint = row.tracker_for(old_key, stream_index)?.checkpoint();
            if namespaces
                .get(&checkpoint.stream_index)
                .is_some_and(|namespace| namespace != &checkpoint.identity_namespace)
            {
                return Err("video checkpoint identity namespace differs from observations".into());
            }
            namespaces.insert(checkpoint.stream_index, checkpoint.identity_namespace);
            let new_id = format!("{new_key}:{}", checkpoint.stream_index);
            if self
                .get_one_unlocked::<StoredVideoCheckpoint>(VIDEO_CHECKPOINT_TABLE, &new_id)?
                .is_some()
            {
                return Err("destination video checkpoint exists".into());
            }
            deletes.push((VIDEO_CHECKPOINT_TABLE.into(), row.checkpoint_id));
            row.checkpoint_id = new_id.clone();
            row.media_key = new_key.into();
            upserts.push((
                VIDEO_CHECKPOINT_TABLE.into(),
                new_id,
                serde_json::to_value(row).map_err(|e| e.to_string())?,
            ));
        }
        Ok((upserts, deletes))
    }
    pub fn publish_video_exemplar(
        &self,
        fence: &RevisionFence,
        permit: &MatchStagePermit,
        expected_epoch: u64,
        input: &VideoExemplarEmbedding,
    ) -> Result<FaceEmbedding, String> {
        let mut stage_use = permit.consume_for_any_stage(&self.session_id, fence)?;
        if permit.stage != JobStage::Detect {
            return Err("video exemplar requires Detect publication permit".into());
        }
        validate_vector(&input.vector)?;
        let _guard = self.mutation_write_guard("video exemplar publication")?;
        self.require_video_publication_epoch(expected_epoch)?;
        let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        let row: StoredVideoObservation = self.require_unlocked(
            VIDEO_OBSERVATION_TABLE,
            &input.observation_id,
            "video observation",
        )?;
        let observation = row.observation()?;
        if !row.exemplar
            || input.source_width == 0
            || input.source_height == 0
            || row.media_key != asset.media_key
            || row.media_fingerprint != asset.media_fingerprint
            || observation.time != input.time
            || observation.frame_sha256 != input.frame_sha256
            || observation.stream_index != input.stream_index
            || input.model_generation != fence.model_generation
            || observation
                .detection
                .bounds
                .iter()
                .zip(input.bounds)
                .any(|(a, b)| !b.is_finite() || (*a - b).abs() > 0.0001)
        {
            return Err("video exemplar exact frame or generation mismatch".into());
        }
        let mut face: FaceObservation =
            self.require_unlocked(FACE_TABLE, &row.face_id, "video Face")?;
        if face.operator_owned {
            return Err("automatic video exemplar cannot replace operator Face".into());
        }
        let landmarks: Vec<Vec<f32>> = input.landmarks.iter().map(|p| p.to_vec()).collect();
        if face.alignment_valid
            && (face.source_width != Some(input.source_width)
                || face.source_height != Some(input.source_height)
                || face.landmarks_normalized != landmarks)
        {
            return Err("video exemplar regeneration changed canonical geometry".into());
        }
        face.source_width = Some(input.source_width);
        face.source_height = Some(input.source_height);
        face.exif_orientation = Some(1);
        face.landmarks_normalized = landmarks;
        face.alignment_valid = true;
        validate_face(&face)?;
        let embedding = FaceEmbedding {
            embedding_id: embedding_id(&face.face_id, &input.model_generation),
            face_id: face.face_id.clone(),
            vector: input.vector.clone(),
            model_generation: input.model_generation.clone(),
            schema_generation: fence.schema_generation.clone(),
            media_fingerprint: asset.media_fingerprint,
            face_revision: face.face_revision,
            job_id: fence.job_id.clone(),
            active: true,
            created_at: now(),
        };
        let upserts = vec![
            (
                FACE_TABLE.into(),
                face.face_id.clone(),
                serde_json::to_value(face).map_err(|e| e.to_string())?,
            ),
            (
                EMBEDDING_TABLE.into(),
                embedding.embedding_id.clone(),
                serde_json::to_value(&embedding).map_err(|e| e.to_string())?,
            ),
        ];
        permit.authorize_payload(
            serde_json::to_vec(&upserts)
                .map_err(|e| e.to_string())?
                .len(),
        )?;
        self.require_video_publication_epoch(expected_epoch)?;
        self.commit_owned_unlocked(&upserts, &[])?;
        stage_use.success();
        Ok(embedding)
    }
    pub fn pending_video_exemplars(
        &self,
        media_key: &str,
        stream_index: u32,
        model_generation: &str,
    ) -> Result<Vec<StoredVideoObservation>, String> {
        let _guard = self.database_read_guard("pending video lock poisoned")?;
        let mut pending = Vec::new();
        let mut cursor = String::new();
        loop {
            let db = self.database();
            let key = media_key.to_string();
            let after = cursor.clone();
            let generation = model_generation.to_string();
            let rows: Vec<StoredVideoObservation> = surreal_store::run(async move {
                let mut response=db.query(r#"SELECT * OMIT id FROM match_video_observation WITH INDEX match_video_observation_media
                    WHERE media_key=$key AND exemplar=true AND observation_id>$after AND
                    array::len((SELECT VALUE embedding_id FROM match_face_embedding WITH INDEX match_embedding_face_generation
                        WHERE face_id=$parent.face_id AND model_generation=$generation AND active=true
                        AND media_fingerprint=$parent.media_fingerprint
                        AND embedding_id='embedding-' + crypto::sha256('embedding' + $separator + face_id + $separator + model_generation)
                        AND array::len((SELECT VALUE face_id FROM match_face_observation WITH INDEX match_face_id
                            WHERE face_id=$parent.face_id AND media_key=$key AND alignment_valid=true
                            AND media_fingerprint=$parent.media_fingerprint AND face_revision=$parent.face_revision
                            AND schema_generation=$parent.schema_generation LIMIT 1)) > 0 LIMIT 1)) = 0
                    ORDER BY observation_id ASC LIMIT 128;"#).bind(("key",key)).bind(("after",after)).bind(("generation",generation)).bind(("separator","\0".to_string())).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                response.take(0).map_err(|e| e.to_string())
            })?;
            if rows.is_empty() {
                break;
            }
            cursor = rows.last().expect("nonempty page").observation_id.clone();
            for row in rows {
                if row.observation()?.stream_index != stream_index {
                    continue;
                }
                if !self.video_embedding_current_unlocked(&row, model_generation)? {
                    pending.push(row);
                    if pending.len() == 128 {
                        return Ok(pending);
                    }
                }
            }
        }
        Ok(pending)
    }
    fn video_embedding_current_unlocked(
        &self,
        row: &StoredVideoObservation,
        generation: &str,
    ) -> Result<bool, String> {
        let Some(face) = self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &row.face_id)? else {
            return Err("video observation references missing Face".into());
        };
        let embedding = self.get_one_unlocked::<FaceEmbedding>(
            EMBEDDING_TABLE,
            &embedding_id(&row.face_id, generation),
        )?;
        Ok(row.exemplar
            && face.media_key == row.media_key
            && face.alignment_valid
            && embedding.is_some_and(|e| {
                e.active
                    && e.model_generation == generation
                    && e.media_fingerprint == row.media_fingerprint
                    && e.media_fingerprint == face.media_fingerprint
                    && e.face_revision == face.face_revision
                    && e.schema_generation == face.schema_generation
            }))
    }
    /// Cursor is the last returned FaceId. Only current exemplars with valid vectors are emitted.
    pub fn video_exemplar_face_ids_page(
        &self,
        media_key: &str,
        generation: &str,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        if !(1..=256).contains(&limit) {
            return Err("video exemplar page limit must be 1..256".into());
        }
        let _guard = self.database_read_guard("video exemplar page lock poisoned")?;
        let mut cursor = after_id
            .strip_prefix("video-face-")
            .unwrap_or(after_id)
            .to_string();
        let mut result = Vec::new();
        loop {
            let db = self.database();
            let key = media_key.to_string();
            let after = cursor.clone();
            let rows: Vec<StoredVideoObservation> = surreal_store::run(async move {
                let mut response = db.query("SELECT * OMIT id FROM match_video_observation WITH INDEX match_video_observation_media WHERE media_key=$key AND exemplar=true AND observation_id>$after ORDER BY observation_id ASC LIMIT 128;").bind(("key",key)).bind(("after",after)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                response.take(0).map_err(|e| e.to_string())
            })?;
            if rows.is_empty() {
                break;
            }
            cursor = rows.last().expect("nonempty page").observation_id.clone();
            for row in rows {
                if self.video_embedding_current_unlocked(&row, generation)? {
                    result.push(row.face_id);
                    if result.len() == limit {
                        return Ok(result);
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn finalize_video_tracks(
        &self,
        fence: &RevisionFence,
        permit: &MatchStagePermit,
        expected_epoch: u64,
        stream_index: u32,
    ) -> Result<VideoCheckpoint, String> {
        let mut stage_use = permit.consume_for_any_stage(&self.session_id, fence)?;
        if permit.stage != JobStage::Detect {
            return Err("video finalization requires Detect permit".into());
        }
        let _guard = self.mutation_write_guard("video finalization")?;
        self.require_video_publication_epoch(expected_epoch)?;
        let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        let id = format!("{}:{stream_index}", asset.media_key);
        let mut stored: StoredVideoCheckpoint =
            self.require_unlocked(VIDEO_CHECKPOINT_TABLE, &id, "video checkpoint")?;
        if stored.media_key != asset.media_key
            || stored.media_fingerprint != asset.media_fingerprint
        {
            return Err("video finalization fingerprint mismatch".into());
        }
        let mut tracker = stored.tracker_for(&asset.media_key, stream_index)?;
        let mut upserts = Vec::new();
        for track in tracker.finish() {
            let mut row: StoredVideoObservation = self.require_unlocked(
                VIDEO_OBSERVATION_TABLE,
                &track.last.observation_id,
                "video final observation",
            )?;
            row.closed = true;
            row.revision = row
                .revision
                .checked_add(1)
                .ok_or("video revision overflow")?;
            upserts.push((
                VIDEO_OBSERVATION_TABLE.into(),
                row.observation_id.clone(),
                serde_json::to_value(row).map_err(|e| e.to_string())?,
            ));
        }
        let checkpoint = tracker.checkpoint();
        stored.payload = serde_json::to_string(&checkpoint).map_err(|e| e.to_string())?;
        upserts.push((
            VIDEO_CHECKPOINT_TABLE.into(),
            id,
            serde_json::to_value(stored).map_err(|e| e.to_string())?,
        ));
        permit.authorize_payload(
            serde_json::to_vec(&upserts)
                .map_err(|e| e.to_string())?
                .len(),
        )?;
        self.require_video_publication_epoch(expected_epoch)?;
        self.commit_owned_unlocked(&upserts, &[])?;
        stage_use.success();
        Ok(checkpoint)
    }
    pub(super) fn video_rows_unlocked(
        &self,
        field: &str,
        key: &str,
    ) -> Result<Vec<StoredVideoObservation>, String> {
        let index = match field {
            "media_key" => "match_video_observation_media",
            "track_id" => "match_video_observation_track",
            _ => return Err("unsupported video lookup".into()),
        };
        let db = self.database();
        let key = key.to_string();
        let query=format!("SELECT * OMIT id FROM {VIDEO_OBSERVATION_TABLE} WITH INDEX {index} WHERE {field}=$key ORDER BY observation_id ASC LIMIT {};",TRACK_ROWS_LIMIT+1);
        let rows: Vec<StoredVideoObservation> = surreal_store::run(async move {
            let mut response = db
                .query(query)
                .bind(("key", key))
                .await
                .map_err(|e| e.to_string())?
                .check()
                .map_err(|e| e.to_string())?;
            response.take(0).map_err(|e| e.to_string())
        })?;
        if rows.len() > TRACK_ROWS_LIMIT {
            return Err("video inventory exceeds 4096-row atomic scope; split asset sampling into bounded tracks".into());
        }
        Ok(rows)
    }
    pub fn video_media_tracks(&self, media_key: &str) -> Result<Vec<VideoTrackSnapshot>, String> {
        self.video_media_tracks_page(media_key, "", 256)
    }
    pub fn video_track_snapshot(&self, track_id: &str) -> Result<VideoTrackSnapshot, String> {
        validate_text("TrackId", track_id)?;
        if track_id.len() > 4096 {
            return Err("TrackId exceeds bound".into());
        }
        let _guard = self.database_read_guard("video snapshot lock poisoned")?;
        self.video_track_snapshot_unlocked(track_id)
    }
    pub fn video_media_tracks_page(
        &self,
        media_key: &str,
        after_track_id: &str,
        limit: usize,
    ) -> Result<Vec<VideoTrackSnapshot>, String> {
        validate_media_key(media_key)?;
        if !(1..=256).contains(&limit) {
            return Err("video track page limit must be 1..256".into());
        }
        let _guard = self.database_read_guard("video snapshot lock poisoned")?;
        #[derive(Deserialize, SurrealValue)]
        struct TrackId {
            track_id: String,
        }
        let db = self.database();
        let key = media_key.to_string();
        let after = after_track_id.to_string();
        let query=format!("SELECT track_id FROM match_video_observation WITH INDEX match_video_observation_media WHERE media_key=$key AND track_id>$after GROUP BY track_id ORDER BY track_id ASC LIMIT {limit};");
        let tracks: Vec<TrackId> = surreal_store::run(async move {
            let mut response = db
                .query(query)
                .bind(("key", key))
                .bind(("after", after))
                .await
                .map_err(|e| e.to_string())?
                .check()
                .map_err(|e| e.to_string())?;
            response.take(0).map_err(|e| e.to_string())
        })?;
        tracks
            .into_iter()
            .map(|t| self.video_track_snapshot_unlocked(&t.track_id))
            .collect()
    }
    pub(super) fn video_track_snapshot_unlocked(
        &self,
        track_id: &str,
    ) -> Result<VideoTrackSnapshot, String> {
        video_track_snapshot(self.video_rows_unlocked("track_id", track_id)?)
    }
    /// Read one exact observation without keeping the database locked during decoding.
    pub(crate) fn video_inspection_observation(
        &self,
        track_id: &str,
        revision: u64,
        time: VideoTime,
    ) -> Result<StoredVideoObservation, String> {
        validate_text("TrackId", track_id)?;
        time.validate()?;
        let _guard = self.database_read_guard("video inspection lock poisoned")?;
        let rows = self.video_rows_unlocked("track_id", track_id)?;
        let track = video_track_snapshot(rows.clone())?;
        if track.revision != revision {
            return Err("inspection track revision changed".into());
        }
        rows.into_iter()
            .find(|row| row.observation().is_ok_and(|o| o.time == time))
            .ok_or_else(|| "exact inspection observation is absent".into())
    }
    pub fn video_checkpoint(
        &self,
        media_key: &str,
        stream_index: u32,
    ) -> Result<Option<VideoCheckpoint>, String> {
        validate_media_key(media_key)?;
        let id = format!("{media_key}:{stream_index}");
        let _guard = self.database_read_guard("video checkpoint lock poisoned")?;
        self.get_one_unlocked::<StoredVideoCheckpoint>(VIDEO_CHECKPOINT_TABLE, &id)?
            .map(|row| Ok(row.tracker_for(media_key, stream_index)?.checkpoint()))
            .transpose()
    }

    pub fn commit_video_frame(
        &self,
        fence: &RevisionFence,
        permit: &MatchStagePermit,
        expected_epoch: u64,
        policy: &VideoPolicy,
        frame: VideoFrame,
    ) -> Result<VideoCheckpoint, String> {
        let mut stage_use = permit.consume_for_any_stage(&self.session_id, fence)?;
        if permit.stage != JobStage::Detect {
            return Err("video sample requires Detect publication permit".into());
        }
        let _guard = self.mutation_write_guard("video sample publication")?;
        self.require_video_publication_epoch(expected_epoch)?;
        let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        let checkpoint_id = format!("{}:{}", asset.media_key, frame.stream_index);
        let prior =
            self.get_one_unlocked::<StoredVideoCheckpoint>(VIDEO_CHECKPOINT_TABLE, &checkpoint_id)?;
        let mut tracker = if let Some(prior) = prior {
            if prior.media_key != asset.media_key
                || prior.media_fingerprint != asset.media_fingerprint
                || prior.policy_json != serde_json::to_string(policy).map_err(|e| e.to_string())?
            {
                return Err("video checkpoint media or policy changed".into());
            }
            prior.tracker_for(&asset.media_key, frame.stream_index)?
        } else {
            let media_hash = canonical_media_sha256(&asset.media_fingerprint)
                .ok_or("invalid video media hash")?
                .to_string();
            match self.video_source_namespace_unlocked(
                &asset.media_key,
                &asset.media_fingerprint,
                frame.stream_index,
            )? {
                Some(namespace) => VideoTracker::new_for_source(
                    media_hash,
                    frame.stream_index,
                    policy.clone(),
                    namespace,
                )?,
                None => VideoTracker::new(media_hash, frame.stream_index, policy.clone())?,
            }
        };
        let prior_state = tracker.checkpoint();
        if canonical_media_sha256(&asset.media_fingerprint)
            != Some(prior_state.media_sha256.as_str())
        {
            return Err("video checkpoint source mismatch".into());
        }
        let update = tracker.ingest(frame)?;
        let checkpoint = tracker.checkpoint();
        let mut rows = Vec::new();
        let mut prior_ids = BTreeSet::new();
        for track in &prior_state.active {
            prior_ids.extend(track.exemplars.iter().map(|e| e.observation_id.clone()));
        }
        for track in &update.closed_tracks {
            prior_ids.insert(track.last.observation_id.clone());
        }
        for id in prior_ids {
            rows.push(self.require_unlocked::<StoredVideoObservation>(
                VIDEO_OBSERVATION_TABLE,
                &id,
                "video prior exemplar",
            )?);
        }
        let mut upserts: Vec<(String, String, Value)> = Vec::new();
        let mut replayed = BTreeSet::new();
        let new_ids: BTreeSet<_> = update
            .observations
            .iter()
            .map(|o| o.observation_id.clone())
            .collect();
        for (ordinal, observation) in update.observations.into_iter().enumerate() {
            let face_id = format!("video-face-{}", observation.observation_id);
            if let Some(face) = self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &face_id)? {
                let row = self.require_unlocked::<StoredVideoObservation>(
                    VIDEO_OBSERVATION_TABLE,
                    &observation.observation_id,
                    "restored video observation",
                )?;
                let mut prior_observation = row.observation()?;
                // A deliberate split changes membership, never the frame evidence or FaceId.
                prior_observation.track_id = observation.track_id.clone();
                if prior_observation != observation
                    || row.media_key != asset.media_key
                    || row.media_fingerprint != asset.media_fingerprint
                    || face.media_fingerprint != asset.media_fingerprint
                    || face.media_key != asset.media_key
                {
                    return Err("restored video observation evidence changed".into());
                }
                replayed.insert(row.observation_id.clone());
                rows.push(row);
                continue;
            }
            let timestamp = now();
            let face = FaceObservation {
                face_id: face_id.clone(),
                media_key: asset.media_key.clone(),
                media_fingerprint: asset.media_fingerprint.clone(),
                source_index: (checkpoint.sample_count - 1) * 128 + ordinal as u32,
                source_width: None,
                source_height: None,
                exif_orientation: None,
                bounds_normalized: observation.detection.bounds.to_vec(),
                landmarks_normalized: Vec::new(),
                alignment_valid: false,
                quality: observation.detection.quality,
                pose_bucket: observation.detection.pose_bucket.clone(),
                operator_owned: false,
                schema_generation: fence.schema_generation.clone(),
                face_revision: 1,
                created_at: timestamp.clone(),
                updated_at: timestamp,
            };
            upserts.push((
                FACE_TABLE.into(),
                face_id.clone(),
                serde_json::to_value(face).map_err(|e| e.to_string())?,
            ));
            rows.push(StoredVideoObservation {
                observation_id: observation.observation_id.clone(),
                face_id,
                track_id: observation.track_id.clone(),
                media_key: asset.media_key.clone(),
                media_fingerprint: asset.media_fingerprint.clone(),
                revision: 1,
                closed: false,
                exemplar: false,
                payload: serde_json::to_string(&observation).map_err(|e| e.to_string())?,
            });
        }
        let mut preserved_tracks = BTreeSet::new();
        for track_id in rows
            .iter()
            .map(|r| r.track_id.clone())
            .collect::<BTreeSet<_>>()
        {
            let db = self.database();
            let id = track_id.clone();
            let closed: Vec<StoredVideoObservation> = surreal_store::run(async move {
                let mut response=db.query("SELECT * OMIT id FROM match_video_observation WITH INDEX match_video_observation_track WHERE track_id=$track_id AND closed=true LIMIT 1;").bind(("track_id",id)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                response.take(0).map_err(|e| e.to_string())
            })?;
            if !closed.is_empty() {
                preserved_tracks.insert(track_id);
            }
        }
        let tracks = checkpoint.active.iter().chain(update.closed_tracks.iter());
        for track in tracks {
            let exemplars: BTreeSet<_> = track
                .exemplars
                .iter()
                .map(|e| e.observation_id.as_str())
                .collect();
            let closed = update
                .closed_tracks
                .iter()
                .any(|t| t.track_id == track.track_id);
            for row in rows.iter_mut().filter(|r| r.track_id == track.track_id) {
                if replayed.contains(&row.observation_id)
                    || preserved_tracks.contains(&row.track_id)
                {
                    continue;
                }
                let before = row.clone();
                row.exemplar = exemplars.contains(row.observation_id.as_str());
                row.closed |= closed && row.observation_id == track.last.observation_id;
                row.revision = row.revision.max(u64::from(checkpoint.sample_count));
                if before.exemplar == row.exemplar
                    && before.closed == row.closed
                    && !new_ids.contains(&row.observation_id)
                {
                    continue;
                }
                if before.exemplar && !row.exemplar {
                    let id = embedding_id(&row.face_id, &fence.model_generation);
                    if let Some(mut embedding) =
                        self.get_one_unlocked::<FaceEmbedding>(EMBEDDING_TABLE, &id)?
                    {
                        embedding.active = false;
                        upserts.push((
                            EMBEDDING_TABLE.into(),
                            id,
                            serde_json::to_value(embedding).map_err(|e| e.to_string())?,
                        ));
                    }
                }
                upserts.push((
                    VIDEO_OBSERVATION_TABLE.into(),
                    row.observation_id.clone(),
                    serde_json::to_value(&*row).map_err(|e| e.to_string())?,
                ));
            }
        }
        let row = StoredVideoCheckpoint {
            checkpoint_id: checkpoint_id.clone(),
            media_key: asset.media_key,
            media_fingerprint: asset.media_fingerprint,
            policy_json: serde_json::to_string(policy).map_err(|e| e.to_string())?,
            payload: serde_json::to_string(&checkpoint).map_err(|e| e.to_string())?,
        };
        upserts.push((
            VIDEO_CHECKPOINT_TABLE.into(),
            checkpoint_id,
            serde_json::to_value(row).map_err(|e| e.to_string())?,
        ));
        permit.authorize_payload(
            serde_json::to_vec(&upserts)
                .map_err(|e| e.to_string())?
                .len(),
        )?;
        self.require_video_publication_epoch(expected_epoch)?;
        self.commit_owned_unlocked(&upserts, &[])?;
        stage_use.success();
        Ok(checkpoint)
    }
    fn require_video_publication_epoch(&self, epoch: u64) -> Result<(), String> {
        if self.external_admission_epoch() != epoch
            || self.external_holds.blocked()
            || DesiredMode::parse(&self.execution_state_unlocked()?.desired_mode)?
                != DesiredMode::Running
            || !self.holds()?.is_empty()
        {
            return Err("video publication admission changed".into());
        }
        Ok(())
    }
}

pub(super) fn video_track_snapshot(
    rows: Vec<StoredVideoObservation>,
) -> Result<VideoTrackSnapshot, String> {
    if rows.len() > 2048 || rows.iter().filter(|r| r.exemplar).count() > 16 {
        return Err("video track exceeds hard observation or exemplar bounds".into());
    }
    let first = rows.first().ok_or("video track not found")?;
    let track_id = first.track_id.clone();
    let playback_origin = first.observation()?.playback_origin;
    let evidence_origin = first.observation()?;
    let media_key = first.media_key.clone();
    let media_fingerprint = first.media_fingerprint.clone();
    let mut observations = Vec::with_capacity(rows.len());
    let mut revision = 0;
    let mut closed = false;
    let mut stream_index = None;
    for row in rows {
        if row.track_id != track_id
            || row.media_key != media_key
            || row.media_fingerprint != media_fingerprint
        {
            return Err("inconsistent video track graph".into());
        }
        let observation = row.observation()?;
        if observation.playback_origin != playback_origin {
            return Err("video track crosses playback origins".into());
        }
        if observation.policy_sha256 != evidence_origin.policy_sha256
            || observation.identity_namespace != evidence_origin.identity_namespace
            || observation.shot_anchor != evidence_origin.shot_anchor
        {
            return Err("video track crosses sampling policy or shot provenance".into());
        }
        if stream_index.is_some_and(|stream| stream != observation.stream_index) {
            return Err("video track crosses streams".into());
        }
        stream_index = Some(observation.stream_index);
        revision = revision.max(row.revision);
        closed |= row.closed;
        observations.push(VideoTrackObservationRow {
            observation_id: row.observation_id,
            face_id: row.face_id,
            time: observation.time,
            exemplar: row.exemplar,
        });
    }
    observations.sort_by(|a, b| {
        let left =
            i128::from(a.time.pts) * i128::from(a.time.numerator) * i128::from(b.time.denominator);
        let right =
            i128::from(b.time.pts) * i128::from(b.time.numerator) * i128::from(a.time.denominator);
        left.cmp(&right)
            .then_with(|| a.observation_id.cmp(&b.observation_id))
    });
    if observations.windows(2).any(|pair| {
        i128::from(pair[0].time.pts)
            * i128::from(pair[0].time.numerator)
            * i128::from(pair[1].time.denominator)
            == i128::from(pair[1].time.pts)
                * i128::from(pair[1].time.numerator)
                * i128::from(pair[0].time.denominator)
    }) {
        return Err("video track has duplicate frame timestamps".into());
    }
    Ok(VideoTrackSnapshot {
        track_id,
        media_key,
        media_fingerprint,
        revision,
        stream_index: stream_index.ok_or("video stream missing")?,
        playback_origin,
        start: observations[0].time,
        end: observations[observations.len() - 1].time,
        observation_count: observations.len(),
        exemplar_count: observations.iter().filter(|r| r.exemplar).count(),
        timestamps: observations.iter().map(|r| r.time).collect(),
        face_ids: observations.iter().map(|r| r.face_id.clone()).collect(),
        closed,
        observations,
    })
}

#[cfg(test)]
mod source_identity_tests;
#[cfg(test)]
mod appearance_tests {
    use super::*;
    #[test]
    fn wp086_person_appearance_pages_filter_exact_assignments_and_fence_mutations() {
        let root = std::env::temp_dir().join(format!(
            "facial-appearances-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Person A", Vec::new()).unwrap();
        let other = store.create_person("Person B", Vec::new()).unwrap();
        store.register_model_generation("fixture", true).unwrap();
        store.activate_model_generation("fixture").unwrap();
        store.set_desired_mode(DesiredMode::Running).unwrap();
        let configured = store.configure_index_root(&root, Vec::new()).unwrap();
        let job = store
            .start_index_job(&configured.root_id, "fixture")
            .unwrap();
        let media = "media/appearance.mkv";
        let fingerprint = "a".repeat(64);
        let mut asset = store
            .enqueue_asset(&job.job_id, media, &fingerprint)
            .unwrap();
        asset.source_path = Some(root.join("appearance.mkv").to_string_lossy().into());
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        let mut tracker =
            VideoTracker::new(fingerprint.clone(), 0, VideoPolicy::default()).unwrap();
        let mut expected = Vec::new();
        let mut assignment_template = None;
        for index in 0..4 {
            let time = VideoTime {
                pts: index * 1000,
                numerator: 1,
                denominator: 1000,
            };
            let frame = VideoFrame {
                stream_index: 0,
                playback_origin: VideoTime::default(),
                time,
                frame_sha256: format!("{index:064x}"),
                scene_score: 0.0,
                detections: vec![crate::match_video::VideoDetection {
                    source_index: 0,
                    bounds: [0.1, 0.1, 0.3, 0.3],
                    quality: 0.8,
                    pose_bucket: "front".into(),
                    detector_generation: "fixture".into(),
                }],
            };
            let observation = tracker.ingest(frame).unwrap().observations.remove(0);
            let face_id = format!("video-face-{}", observation.observation_id);
            let face = FaceObservation {
                face_id: face_id.clone(),
                media_key: media.into(),
                media_fingerprint: fingerprint.clone(),
                source_index: index as u32,
                source_width: None,
                source_height: None,
                exif_orientation: None,
                bounds_normalized: observation.detection.bounds.to_vec(),
                landmarks_normalized: Vec::new(),
                alignment_valid: false,
                quality: 0.8,
                pose_bucket: "front".into(),
                operator_owned: true,
                schema_generation: MATCH_SCHEMA_GENERATION.into(),
                face_revision: 1,
                created_at: now(),
                updated_at: now(),
            };
            store.upsert_json(FACE_TABLE, &face_id, &face).unwrap();
            let video = StoredVideoObservation {
                observation_id: observation.observation_id.clone(),
                face_id: face_id.clone(),
                track_id: observation.track_id.clone(),
                media_key: media.into(),
                media_fingerprint: fingerprint.clone(),
                revision: 3,
                closed: true,
                exemplar: index == 0,
                payload: serde_json::to_string(&observation).unwrap(),
            };
            store
                .upsert_json(VIDEO_OBSERVATION_TABLE, &video.observation_id, &video)
                .unwrap();
            let owner = if index == 3 { &other } else { &person };
            let assignment = Assignment {
                assignment_id: face_id.clone(),
                face_id: face_id.clone(),
                person_id: owner.person_id.clone(),
                media_key: media.into(),
                look_id: None,
                placement: "catalog".into(),
                state: "operator_confirmed".into(),
                provenance: "operator".into(),
                locked: true,
                model_generation: None,
                calibration_generation: None,
                envelope_hash: None,
                face_revision: 1,
                person_revision: owner.revision,
                operation_id: "fixture".into(),
                created_at: now(),
                updated_at: now(),
            };
            store
                .upsert_json(ASSIGNMENT_TABLE, &face_id, &assignment)
                .unwrap();
            if index != 3 {
                expected.push((face_id, time, video.track_id));
                assignment_template = Some(assignment);
            }
        }
        // A full bounded page of stale references must advance, not scan forever.
        let mut pending = Vec::new();
        for index in 0..128 {
            let mut row = assignment_template.clone().unwrap();
            row.assignment_id = format!("a-missing-{index:03}");
            row.face_id = row.assignment_id.clone();
            pending.push((
                ASSIGNMENT_TABLE.into(),
                row.assignment_id.clone(),
                serde_json::to_value(row).unwrap(),
            ));
        }
        store.commit_owned_unlocked(&pending, &[]).unwrap();
        let empty = store
            .person_video_appearances_page(&person.person_id, None, 1)
            .unwrap();
        assert!(empty.rows.is_empty() && empty.has_more && empty.next_cursor.is_some());
        let mut cursor = empty.next_cursor.clone();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        let mut observed = Vec::new();
        for _ in 0..4 {
            let page = store
                .person_video_appearances_page(&person.person_id, cursor.as_ref(), 1)
                .unwrap();
            for row in &page.rows {
                assert_eq!(row.track_revision, 3);
                assert_eq!(row.source_path, asset.source_path);
                assert_eq!(
                    row.seek_ms,
                    row.timestamp
                        .playback_milliseconds(row.playback_origin)
                        .unwrap()
                );
                observed.push((
                    format!("video-face-{}", row.observation_id),
                    row.timestamp,
                    row.track_id.clone(),
                ));
            }
            cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(observed, expected);
        let stable_cursor = empty.next_cursor.as_ref();
        let execution = store.execution_state_unlocked().unwrap();
        for identity_changed in [true, false] {
            let mut changed = execution.clone();
            if identity_changed {
                changed.identity_revision += 1;
            } else {
                changed.catalog_revision += 1;
            }
            store
                .upsert_json(EXECUTION_TABLE, "global", &changed)
                .unwrap();
            assert!(store
                .person_video_appearances_page(&person.person_id, stable_cursor, 1)
                .is_err());
            store
                .upsert_json(EXECUTION_TABLE, "global", &execution)
                .unwrap();
        }
        let mut changed_asset = asset.clone();
        changed_asset.media_fingerprint = "b".repeat(64);
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &changed_asset)
            .unwrap();
        let changed_page = store
            .person_video_appearances_page(&person.person_id, stable_cursor, 64)
            .unwrap();
        assert!(changed_page.rows.is_empty() && !changed_page.has_more);
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        assert_eq!(
            store
                .person_video_appearances_page(&person.person_id, stable_cursor, 64)
                .unwrap()
                .rows
                .len(),
            expected.len()
        );
        let cursor = empty.next_cursor; // captured revision fence survives only unchanged catalog
                                        // Synthetic storage-boundary proof only: reuse the existing test-only
                                        // integrity-bound calibration builder, not a real activation verdict.
        let automatic_face_id = &expected[0].0;
        let manual: Assignment = store
            .require_unlocked(ASSIGNMENT_TABLE, automatic_face_id, "fixture assignment")
            .unwrap();
        let face: FaceObservation = store
            .require_unlocked(FACE_TABLE, automatic_face_id, "fixture Face")
            .unwrap();
        super::super::tests::activate_calibration(
            &store,
            "fixture",
            "appearance-calibration",
            &"e".repeat(64),
        );
        let embedding = super::super::tests::embedding(&face, "fixture", &job);
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        let mut automatic = manual.clone();
        automatic.state = AssignmentState::CommittedStrictAutomatic.as_str().into();
        automatic.locked = false;
        automatic.provenance = "strict".into();
        automatic.model_generation = Some("fixture".into());
        automatic.calibration_generation = Some("appearance-calibration".into());
        automatic.envelope_hash = Some("e".repeat(64));
        automatic.created_at = now();
        automatic.updated_at = automatic.created_at.clone();
        store
            .upsert_json(ASSIGNMENT_TABLE, automatic_face_id, &automatic)
            .unwrap();
        let current = store
            .person_video_appearances_page(&person.person_id, cursor.as_ref(), 64)
            .unwrap();
        assert_eq!(current.rows.len(), expected.len());
        assert!(current
            .rows
            .iter()
            .any(|row| format!("video-face-{}", row.observation_id) == *automatic_face_id));
        let mut stale_embedding = embedding.clone();
        stale_embedding.active = false;
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &stale_embedding)
            .unwrap();
        let stale = store
            .person_video_appearances_page(&person.person_id, cursor.as_ref(), 64)
            .unwrap();
        assert_eq!(stale.rows.len(), expected.len() - 1);
        assert!(stale
            .rows
            .iter()
            .all(|row| format!("video-face-{}", row.observation_id) != *automatic_face_id));
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        let activation: CalibrationActivation = store
            .require_unlocked(
                CALIBRATION_TABLE,
                "appearance-calibration",
                "fixture activation",
            )
            .unwrap();
        let mut corrupted = activation.clone();
        corrupted.automatic_threshold = 0.91; // Preserve the old digest: verifier must reject it.
        store
            .upsert_json(CALIBRATION_TABLE, "appearance-calibration", &corrupted)
            .unwrap();
        let stale = store
            .person_video_appearances_page(&person.person_id, cursor.as_ref(), 64)
            .unwrap();
        assert_eq!(stale.rows.len(), expected.len() - 1);
        assert!(stale
            .rows
            .iter()
            .all(|row| format!("video-face-{}", row.observation_id) != *automatic_face_id));
        store
            .upsert_json(CALIBRATION_TABLE, "appearance-calibration", &activation)
            .unwrap();
        assert_eq!(
            store
                .person_video_appearances_page(&person.person_id, cursor.as_ref(), 64)
                .unwrap()
                .rows
                .len(),
            expected.len()
        );
        store
            .upsert_json(ASSIGNMENT_TABLE, automatic_face_id, &manual)
            .unwrap();
        let mut changed = person.clone();
        changed.revision += 1;
        store
            .upsert_json(PERSON_TABLE, &person.person_id, &changed)
            .unwrap();
        assert!(store
            .person_video_appearances_page(&person.person_id, cursor.as_ref(), 1)
            .is_err());
        assert!(store
            .person_video_appearances_page(&other.person_id, cursor.as_ref(), 1)
            .is_err());
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
