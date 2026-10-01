use super::*;

pub(in crate::match_store) const MEDIA_CONTEXT_TABLE: &str = "match_media_context";
pub(in crate::match_store) const MEDIA_CONTEXT_SCHEMA: &str = r#"
DEFINE TABLE OVERWRITE match_media_context SCHEMAFULL;
DEFINE FIELD OVERWRITE media_key ON match_media_context TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON match_media_context TYPE string;
DEFINE FIELD OVERWRITE revision ON match_media_context TYPE int;
DEFINE FIELD OVERWRITE capture_unix_millis ON match_media_context TYPE option<int>;
DEFINE FIELD OVERWRITE time_window_millis ON match_media_context TYPE option<int>;
DEFINE FIELD OVERWRITE album_ids ON match_media_context TYPE array<string>;
DEFINE INDEX OVERWRITE match_media_context_key ON match_media_context FIELDS media_key UNIQUE;
"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaContextRequest {
    pub media_key: String,
    pub media_fingerprint: String,
    pub expected_revision: u64,
    #[serde(default)]
    pub capture_unix_millis: Option<i64>,
    #[serde(default)]
    pub time_window_millis: Option<u64>,
    #[serde(default)]
    pub album_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalMediaContext {
    pub media_key: String,
    pub media_fingerprint: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_unix_millis: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_window_millis: Option<u64>,
    pub album_ids: Vec<String>,
}
impl MediaContextRequest {
    pub fn validate(&self) -> Result<(), String> {
        let text = |s: &str, cap| {
            !s.is_empty() && s.trim() == s && s.len() <= cap && !s.chars().any(char::is_control)
        };
        if !text(&self.media_key, 4096)
            || self.media_fingerprint.len() != 64
            || !self
                .media_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.expected_revision >= i64::MAX as u64
            || self.album_ids.len() > 32
            || self.album_ids.iter().any(|s| !text(s, 256))
            || self.album_ids.windows(2).any(|w| w[0] >= w[1])
            || self.time_window_millis.is_some_and(|w| {
                w == 0 || w > i64::MAX as u64 || self.capture_unix_millis.is_none()
            })
        {
            return Err("invalid canonical media context bounds or ordering".into());
        }
        Ok(())
    }
}
impl CanonicalMediaContext {
    pub fn validate(&self) -> Result<(), String> {
        if self.revision == 0 {
            return Err("media context revision must be positive".into());
        }
        MediaContextRequest {
            media_key: self.media_key.clone(),
            media_fingerprint: self.media_fingerprint.clone(),
            expected_revision: self.revision - 1,
            capture_unix_millis: self.capture_unix_millis,
            time_window_millis: self.time_window_millis,
            album_ids: self.album_ids.clone(),
        }
        .validate()
    }
}
impl MatchStore {
    pub fn media_context(&self, key: &str) -> Result<Option<CanonicalMediaContext>, String> {
        self.get_one(MEDIA_CONTEXT_TABLE, key)
    }
    pub fn replace_media_context(
        &self,
        request: &MediaContextRequest,
    ) -> Result<CanonicalMediaContext, String> {
        request.validate()?;
        let _guard = self.mutation_write_guard("operator media context replacement")?;
        let asset = self
            .canonical_job_asset_for_media_unlocked(&request.media_key)?
            .ok_or("media context requires canonical indexed media")?;
        if canonical_media_sha256(&asset.media_fingerprint)
            != Some(request.media_fingerprint.as_str())
        {
            return Err("media context fingerprint is stale".into());
        }
        let prior: Option<CanonicalMediaContext> =
            self.get_one_unlocked(MEDIA_CONTEXT_TABLE, &request.media_key)?;
        if prior.as_ref().map_or(0, |r| r.revision) != request.expected_revision {
            return Err("media context revision conflict".into());
        }
        let row = CanonicalMediaContext {
            media_key: request.media_key.clone(),
            media_fingerprint: request.media_fingerprint.clone(),
            revision: request.expected_revision + 1,
            capture_unix_millis: request.capture_unix_millis,
            time_window_millis: request.time_window_millis,
            album_ids: request.album_ids.clone(),
        };
        self.commit_owned_unlocked(
            &[(
                MEDIA_CONTEXT_TABLE.into(),
                row.media_key.clone(),
                serde_json::to_value(&row).map_err(|e| e.to_string())?,
            )],
            &[],
        )?;
        Ok(row)
    }
    pub(super) fn canonical_context_evidence_unlocked(
        &self,
        face: &FaceObservation,
        person: &Person,
    ) -> Result<Vec<ContextEvidence>, String> {
        self.canonical_context_evidence_bounded_unlocked(face, person, &mut 256)
    }
    pub(super) fn canonical_context_evidence_bounded_unlocked(
        &self,
        face: &FaceObservation,
        person: &Person,
        budget: &mut usize,
    ) -> Result<Vec<ContextEvidence>, String> {
        let Some(current) =
            self.get_one_unlocked::<CanonicalMediaContext>(MEDIA_CONTEXT_TABLE, &face.media_key)?
        else {
            return Ok(Vec::new());
        };
        current.validate()?;
        if current.album_ids.is_empty()
            && (current.capture_unix_millis.is_none() || current.time_window_millis.is_none())
        {
            return Ok(Vec::new());
        }
        if Some(current.media_fingerprint.as_str())
            != canonical_media_sha256(&face.media_fingerprint)
            || self
                .canonical_job_asset_for_media_unlocked(&face.media_key)?
                .is_none_or(|a| {
                    canonical_media_sha256(&a.media_fingerprint)
                        != Some(current.media_fingerprint.as_str())
                })
        {
            return Ok(Vec::new());
        }
        let db = self.database();
        let person_id = person.person_id.clone();
        let limit = *budget + 1;
        let mut assignments: Vec<Assignment> = surreal_store::run(async move {
            let mut response=db.query("SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person WHERE person_id=$person LIMIT $limit;").bind(("person",person_id)).bind(("limit",limit)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
            response.take(0).map_err(|e| e.to_string())
        })?;
        if assignments.len() > *budget {
            return Err("canonical context reference bound exceeded; review is incomplete".into());
        }
        *budget -= assignments.len();
        assignments.sort_by(|a, b| a.assignment_id.cmp(&b.assignment_id));
        let mut found = BTreeMap::new();
        for assignment in assignments {
            if assignment.state != "operator_confirmed"
                || !assignment.locked
                || assignment.media_key == face.media_key
                || assignment.person_revision != person.revision
            {
                continue;
            }
            let Some(reference_face) =
                self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &assignment.face_id)?
            else {
                continue;
            };
            if reference_face.face_revision != assignment.face_revision
                || reference_face.media_key != assignment.media_key
            {
                continue;
            }
            let Some(reference) = self.get_one_unlocked::<CanonicalMediaContext>(
                MEDIA_CONTEXT_TABLE,
                &assignment.media_key,
            )?
            else {
                continue;
            };
            reference.validate()?;
            if Some(reference.media_fingerprint.as_str())
                != canonical_media_sha256(&reference_face.media_fingerprint)
                || self
                    .canonical_job_asset_for_media_unlocked(&reference.media_key)?
                    .is_none_or(|a| {
                        canonical_media_sha256(&a.media_fingerprint)
                            != Some(reference.media_fingerprint.as_str())
                    })
            {
                continue;
            }
            let mut sources = Vec::new();
            if let (Some(time), Some(other), Some(window)) = (
                current.capture_unix_millis,
                reference.capture_unix_millis,
                current.time_window_millis,
            ) {
                if time.abs_diff(other) <= window {
                    sources.push((0, ContextSource::Time { unix_millis: time }));
                }
            }
            if let Some(album) = current
                .album_ids
                .iter()
                .find(|a| reference.album_ids.binary_search(a).is_ok())
            {
                sources.push((
                    1,
                    ContextSource::Album {
                        album_id: album.clone(),
                    },
                ));
            }
            for (kind, source) in sources {
                let source_id = format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&(
                            face,
                            person.revision,
                            &current,
                            &reference,
                            &assignment,
                            &source
                        ))
                        .map_err(|e| e.to_string())?
                    )
                );
                found.entry(kind).or_insert(ContextEvidence {
                    version: 1,
                    evidence_id: format!("context-{source_id}"),
                    candidate_person_id: person.person_id.clone(),
                    source_id,
                    source_revision: current.revision,
                    source,
                    score: 1.0,
                });
            }
        }
        Ok(found.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wp086_media_context_requires_explicit_window_and_bounded_canonical_albums() {
        let mut request = MediaContextRequest {
            media_key: "root/media.jpg".into(),
            media_fingerprint: "a".repeat(64),
            expected_revision: 0,
            capture_unix_millis: Some(100),
            time_window_millis: None,
            album_ids: Vec::new(),
        };
        request.validate().unwrap(); // capture alone never invents a proximity window
        request.time_window_millis = Some(0);
        assert!(request.validate().is_err());
        request.time_window_millis = Some(1);
        request.capture_unix_millis = None;
        assert!(request.validate().is_err());
        request.time_window_millis = None;
        request.album_ids = vec!["album".into(), "album".into()];
        assert!(request.validate().is_err());
        request.album_ids = vec!["x".repeat(257)];
        assert!(request.validate().is_err());
        request.album_ids = (0..33).map(|n| format!("album-{n:02}")).collect();
        assert!(request.validate().is_err());
        request.album_ids.clear();
        request.validate().unwrap();
        request.expected_revision = i64::MAX as u64;
        assert!(request.validate().is_err());
    }
}
