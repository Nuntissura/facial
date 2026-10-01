//! Explicit bounded unnamed review; clustering never writes identity state.
use super::*;
use crate::identity::{cluster_unnamed_conservative, ClusterCandidate, IdentityVector};
use std::collections::HashSet;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnnamedClusterReviewRequest {
    pub face_ids: Vec<String>,
    pub model_generation: String,
    pub similarity_threshold: f32,
    pub minimum_quality: f32,
    pub minimum_independent_families: usize,
}
impl UnnamedClusterReviewRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=256).contains(&self.face_ids.len())
            || !(1..=256).contains(&self.minimum_independent_families)
            || !self.similarity_threshold.is_finite()
            || !(-1.0..=1.0).contains(&self.similarity_threshold)
            || !self.minimum_quality.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_quality)
        {
            return Err("unnamed review requires an explicit bounded selection and finite caller-supplied policy".into());
        }
        validate_text("unnamed review generation", &self.model_generation)?;
        if self.model_generation.trim() != self.model_generation
            || self.model_generation.chars().any(char::is_control)
        {
            return Err("unnamed review generation must be canonical text".into());
        }
        let mut ids = BTreeSet::new();
        for id in &self.face_ids {
            validate_text("unnamed review FaceId", id)?;
            if id.trim() != id || id.chars().any(char::is_control) || !ids.insert(id) {
                return Err("unnamed review FaceIds must be unique canonical identifiers".into());
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UnnamedClusterReviewRow {
    pub face_id: String,
    pub duplicate_family_id: Option<String>,
    pub cluster_id: Option<String>,
    pub independent_families: usize,
    pub exclusion_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UnnamedClusterReview {
    pub requested_face_ids: Vec<String>,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub rows: Vec<UnnamedClusterReviewRow>,
}
impl MatchStore {
    pub fn review_unnamed_clusters(
        &self,
        request: &UnnamedClusterReviewRequest,
    ) -> Result<UnnamedClusterReview, String> {
        request.validate()?;
        let _guard = self.database_read_guard("unnamed review lock poisoned")?;
        if self.active_model_generation_unlocked()?.as_deref()
            != Some(request.model_generation.as_str())
        {
            return Err("unnamed review requires the current validated model generation".into());
        }
        let execution = self.execution_state_unlocked()?;
        let mut requested = request.face_ids.clone();
        requested.sort();
        let mut rows = Vec::new();
        let mut candidates = Vec::new();
        let mut candidate_rows = Vec::new();
        let mut video_families = BTreeMap::new();
        for id in &requested {
            let mut row = UnnamedClusterReviewRow {
                face_id: id.clone(),
                duplicate_family_id: None,
                cluster_id: None,
                independent_families: 0,
                exclusion_reason: None,
            };
            match self.unnamed_review_candidate_unlocked(
                id,
                &request.model_generation,
                request.minimum_quality,
                &mut video_families,
            )? {
                Ok(candidate) => {
                    row.duplicate_family_id = Some(candidate.duplicate_family_id.clone());
                    candidate_rows.push(rows.len());
                    candidates.push(candidate);
                }
                Err(reason) => row.exclusion_reason = Some(reason.into()),
            }
            rows.push(row);
        }
        // Faces carrying Person cannot-links are excluded at the canonical adapter.
        let membership = cluster_unnamed_conservative(
            &candidates,
            request.similarity_threshold,
            request.minimum_quality,
            request.minimum_independent_families,
            &HashSet::new(),
        )
        .map_err(|e| e.to_string())?;
        let mut groups = BTreeMap::<usize, Vec<usize>>::new();
        for (i, group) in membership.iter().enumerate() {
            if let Some(group) = group {
                groups.entry(*group).or_default().push(i);
            } else {
                rows[candidate_rows[i]].exclusion_reason =
                    Some("insufficient_independent_families".into());
            }
        }
        for members in groups.into_values() {
            let faces = members
                .iter()
                .map(|i| candidates[*i].observation_id.as_str())
                .collect::<Vec<_>>();
            let families = members
                .iter()
                .map(|i| candidates[*i].duplicate_family_id.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            let group_id = format!(
                "review-cluster-{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(
                        &request.model_generation,
                        request.similarity_threshold.to_bits(),
                        request.minimum_quality.to_bits(),
                        request.minimum_independent_families,
                        &faces
                    ))
                    .map_err(|e| e.to_string())?
                )
            );
            for i in members {
                rows[candidate_rows[i]].cluster_id = Some(group_id.clone());
                rows[candidate_rows[i]].independent_families = families;
            }
        }
        Ok(UnnamedClusterReview {
            requested_face_ids: requested,
            model_generation: request.model_generation.clone(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            rows,
        })
    }
    fn unnamed_review_candidate_unlocked(
        &self,
        id: &str,
        generation: &str,
        minimum_quality: f32,
        video_families: &mut BTreeMap<String, String>,
    ) -> Result<Result<ClusterCandidate, &'static str>, String> {
        let Some(face) = self.get_one_unlocked::<FaceObservation>(FACE_TABLE, id)? else {
            return Ok(Err("face_missing"));
        };
        let db = self.database();
        let key = id.to_string();
        let (assigned, constrained, trusted): (Vec<String>, Vec<String>, Vec<String>) =
            surreal_store::run(async move {
                let mut response=db.query("SELECT VALUE face_id FROM match_assignment WITH INDEX match_assignment_face WHERE face_id=$face LIMIT 1; SELECT VALUE face_id FROM match_constraint WITH INDEX match_constraint_face_person WHERE face_id=$face LIMIT 1; SELECT VALUE face_id FROM match_trusted_member WITH INDEX match_trusted_member_face WHERE face_id=$face LIMIT 1;").bind(("face",key)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                Ok((
                    response.take(0).map_err(|e| e.to_string())?,
                    response.take(1).map_err(|e| e.to_string())?,
                    response.take(2).map_err(|e| e.to_string())?,
                ))
            })?;
        if !assigned.is_empty() {
            return Ok(Err("already_assigned"));
        }
        if !constrained.is_empty() {
            return Ok(Err("person_cannot_link"));
        }
        if !trusted.is_empty() {
            return Ok(Err("trusted_identity_evidence"));
        }
        if self
            .get_one_unlocked::<FaceDisposition>(FACE_DISPOSITION_TABLE, id)?
            .is_some()
        {
            return Ok(Err("face_disposition"));
        }
        if !face.alignment_valid || !face.quality.is_finite() || face.quality < minimum_quality {
            return Ok(Err("quality_or_alignment"));
        }
        let Some(asset) = self.canonical_job_asset_for_media_unlocked(&face.media_key)? else {
            return Ok(Err("current_source_missing"));
        };
        let Some(embedding) =
            self.get_one_unlocked::<FaceEmbedding>(EMBEDDING_TABLE, &embedding_id(id, generation))?
        else {
            return Ok(Err("embedding_missing"));
        };
        if !embedding.active
            || embedding.face_id != face.face_id
            || embedding.media_fingerprint != face.media_fingerprint
            || embedding.face_revision != face.face_revision
            || embedding.schema_generation != face.schema_generation
            || embedding.schema_generation != MATCH_SCHEMA_GENERATION
            || embedding.model_generation != generation
            || asset.job_id != embedding.job_id
            || asset.media_fingerprint != face.media_fingerprint
            || asset.model_generation != generation
            || asset.schema_generation != face.schema_generation
        {
            return Ok(Err("stale_embedding_or_source"));
        }
        let family = if let Some(observation_id) = id.strip_prefix("video-face-") {
            let Some(observation) = self.get_one_unlocked::<StoredVideoObservation>(
                video::VIDEO_OBSERVATION_TABLE,
                observation_id,
            )?
            else {
                return Ok(Err("video_observation_missing"));
            };
            observation.observation()?;
            if !observation.exemplar
                || observation.face_id != face.face_id
                || observation.media_key != face.media_key
                || observation.media_fingerprint != face.media_fingerprint
            {
                return Ok(Err("not_current_video_exemplar"));
            }
            if let Some(family) = video_families.get(&observation.track_id) {
                family.clone()
            } else {
                let family = self.video_density_family_unlocked(&observation.track_id)?;
                video_families.insert(observation.track_id, family.clone());
                family
            }
        } else {
            format!(
                "image-content:{}",
                canonical_media_sha256(&face.media_fingerprint)
                    .ok_or("invalid canonical image fingerprint")?
            )
        };
        let vector = match IdentityVector::from_persisted(embedding.vector, generation) {
            Ok(vector) => vector,
            Err(_) => return Ok(Err("invalid_embedding_vector")),
        };
        Ok(Ok(ClusterCandidate {
            observation_id: id.into(),
            duplicate_family_id: family,
            quality: face.quality,
            operator_confirmed: false,
            vector,
        }))
    }
}
