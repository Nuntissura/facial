use super::*;
mod media;
#[cfg(test)]
mod tests;
use crate::match_context::{
    rank_context_review, rank_for_review, ContextConfig, ContextEvidence, ContextReviewCandidate,
    ContextSource, VisualReviewCandidate,
};
pub use media::*;

pub(super) const CONTEXT_TABLE: &str = "match_review_context";
pub(super) const WP086_CONTEXT_SCHEMA_SQL: &str = r#"
DEFINE TABLE OVERWRITE match_review_context SCHEMAFULL;
DEFINE FIELD OVERWRITE context_id ON match_review_context TYPE string;
DEFINE FIELD OVERWRITE face_id ON match_review_context TYPE string;
DEFINE FIELD OVERWRITE person_id ON match_review_context TYPE string;
DEFINE FIELD OVERWRITE payload ON match_review_context TYPE string;
DEFINE INDEX OVERWRITE match_review_context_id ON match_review_context FIELDS context_id UNIQUE;
DEFINE INDEX OVERWRITE match_review_context_face ON match_review_context FIELDS face_id, person_id;
"#;
#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoredReviewContext {
    pub context_id: String,
    pub face_id: String,
    pub person_id: String,
    pub payload: String,
}
impl MatchStore {
    pub fn set_context_evidence(
        &self,
        face_id: &str,
        person_id: &str,
        evidence: Vec<ContextEvidence>,
    ) -> Result<(), String> {
        let candidate = ContextReviewCandidate {
            person_id: person_id.into(),
            visual_similarity: 0.0,
            context: evidence,
        };
        candidate.validate()?;
        let _guard = self.mutation_write_guard("review context update")?;
        let face = self.require_unlocked::<FaceObservation>(FACE_TABLE, face_id, "Face")?;
        let person = self.require_unlocked::<Person>(PERSON_TABLE, person_id, "Person")?;
        let canonical = self.canonical_context_evidence_unlocked(&face, &person)?;
        for item in &candidate.context {
            if matches!(
                item.source,
                ContextSource::Time { .. } | ContextSource::Album { .. }
            ) {
                if !canonical.contains(item) {
                    return Err("time/album evidence lacks current canonical source".into());
                }
            } else if item.source_revision != person.revision
                || item.source_id != context_source_id(&face, &person, &item.source)?
            {
                return Err(
                    "context evidence does not bind current canonical source revision".into(),
                );
            }
        }
        let context_id = format!("{:x}", Sha256::digest(format!("{face_id}\0{person_id}")));
        let row = StoredReviewContext {
            context_id: context_id.clone(),
            face_id: face_id.into(),
            person_id: person_id.into(),
            payload: serde_json::to_string(&candidate.context).map_err(|e| e.to_string())?,
        };
        self.transactional_upserts_deletes_unlocked(
            &[(
                CONTEXT_TABLE,
                &context_id,
                serde_json::to_value(row).map_err(|e| e.to_string())?,
            )],
            &[],
        )
    }
    pub fn context_review(
        &self,
        face_id: &str,
        enabled: bool,
    ) -> Result<Vec<ContextReviewCandidate>, String> {
        let _guard = self.mutation_write_guard("context evidence refresh")?;
        let face = self.require_unlocked::<FaceObservation>(FACE_TABLE, face_id, "Face")?;
        let asset = self.canonical_job_asset_for_media_unlocked(&face.media_key)?;
        let relative_source = if let Some(asset) = &asset {
            let job: IndexJob =
                self.require_unlocked(JOB_TABLE, &asset.job_id, "context source job")?;
            let root = self.get_one_unlocked::<MatchIndexRoot>(ROOT_CONFIG_TABLE, &job.root_key)?;
            match (root, asset.source_path.as_deref()) {
                (Some(root), Some(source)) => Path::new(source)
                    .strip_prefix(Path::new(&root.path))
                    .ok()
                    .filter(|p| {
                        p.components()
                            .all(|c| matches!(c, std::path::Component::Normal(_)))
                    })
                    .map(|p| p.to_string_lossy().replace('\\', "/")),
                _ => None,
            }
        } else {
            None
        };
        let active = self.active_model_generation_unlocked()?;
        let db = self.database();
        let key = face_id.to_string();
        let (suggestions, contexts): (Vec<Suggestion>, Vec<StoredReviewContext>) =
            surreal_store::run(async move {
                let mut response=db.query("SELECT * OMIT id FROM match_suggestion WITH INDEX match_suggestion_face_person WHERE face_id=$face_id LIMIT 257; SELECT * OMIT id FROM match_review_context WITH INDEX match_review_context_face WHERE face_id=$face_id LIMIT 257;").bind(("face_id",key)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                Ok((
                    response.take(0).map_err(|e| e.to_string())?,
                    response.take(1).map_err(|e| e.to_string())?,
                ))
            })?;
        if suggestions.len() > 256 || contexts.len() > 256 {
            return Err("context review bound exceeded".into());
        }
        let mut candidates = BTreeMap::<String, ContextReviewCandidate>::new();
        let mut people = BTreeMap::new();
        for suggestion in suggestions {
            let person = self.require_unlocked::<Person>(
                PERSON_TABLE,
                &suggestion.candidate_person_id,
                "context candidate Person",
            )?;
            let embedding = self
                .get_one_unlocked::<FaceEmbedding>(
                    EMBEDDING_TABLE,
                    &embedding_id(face_id, &suggestion.model_generation),
                )?
                .as_ref()
                .map(FaceEmbeddingProvenance::from);
            if !self.suggestion_provenance_is_current_unlocked(
                &suggestion,
                &face,
                &person,
                active.as_deref(),
                asset.as_ref(),
                embedding.as_ref(),
            ) {
                continue;
            }
            people.insert(person.person_id.clone(), person);
            let candidate = candidates
                .entry(suggestion.candidate_person_id.clone())
                .or_insert(ContextReviewCandidate {
                    person_id: suggestion.candidate_person_id,
                    visual_similarity: suggestion.similarity,
                    context: Vec::new(),
                });
            candidate.visual_similarity = candidate.visual_similarity.max(suggestion.similarity);
        }
        // Persisted hints are projections; regenerate from canonical sources.
        let mut reference_budget = 256usize;
        let mut revisions = BTreeMap::new();
        let mut upserts = Vec::new();
        for candidate in candidates.values_mut() {
            let person = &people[&candidate.person_id];
            candidate
                .context
                .extend(self.canonical_context_evidence_bounded_unlocked(
                    &face,
                    person,
                    &mut reference_budget,
                )?);
            if let Some(path) = relative_source.as_deref() {
                let path = path.replace('\\', "/");
                let (folder, filename) = path.rsplit_once('/').unwrap_or(("", &path));
                let names = std::iter::once(&person.name).chain(person.aliases.iter());
                for (text, source) in [
                    (
                        folder,
                        ContextSource::Folder {
                            path: folder.into(),
                        },
                    ),
                    (
                        filename,
                        ContextSource::Filename {
                            name: filename.into(),
                        },
                    ),
                ] {
                    if text.is_empty() || text.len() > 1024 {
                        continue;
                    }
                    let normalized = context_words(text);
                    if names.clone().any(|name| {
                        let name = context_words(name);
                        !name.trim().is_empty() && normalized.contains(&name)
                    }) {
                        candidate
                            .context
                            .push(context_evidence(&face, person, source)?);
                    }
                }
            }
            let db = self.database();
            let person_id = person.person_id.clone();
            let media_key = face.media_key.clone();
            let excluded = face.face_id.clone();
            let cooccurs: Vec<Assignment> = surreal_store::run(async move {
                let mut response=db.query("SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person WHERE person_id=$person_id AND media_key=$media_key AND face_id!=$excluded AND state='operator_confirmed' AND locked=true LIMIT 1;").bind(("person_id",person_id)).bind(("media_key",media_key)).bind(("excluded",excluded)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                response.take(0).map_err(|e| e.to_string())
            })?;
            if let Some(assignment) = cooccurs.first() {
                if let Some(other) =
                    self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &assignment.face_id)?
                {
                    if assignment.face_revision == other.face_revision
                        && assignment.person_revision == person.revision
                        && other.media_key == face.media_key
                        && other.media_fingerprint == face.media_fingerprint
                    {
                        candidate.context.push(context_evidence(
                            &face,
                            person,
                            ContextSource::Cooccurrence {
                                person_id: person.person_id.clone(),
                            },
                        )?);
                    }
                }
            }
            candidate.validate()?;
            for evidence in &candidate.context {
                revisions.insert(evidence.source_id.clone(), evidence.source_revision);
            }
            let context_id = format!(
                "{:x}",
                Sha256::digest(format!("{face_id}\0{}", person.person_id))
            );
            let row = StoredReviewContext {
                context_id: context_id.clone(),
                face_id: face_id.into(),
                person_id: person.person_id.clone(),
                payload: serde_json::to_string(&candidate.context).map_err(|e| e.to_string())?,
            };
            upserts.push((
                CONTEXT_TABLE.to_string(),
                context_id,
                serde_json::to_value(row).map_err(|e| e.to_string())?,
            ));
        }
        let visual = candidates
            .values()
            .map(|c| VisualReviewCandidate {
                person_id: c.person_id.clone(),
                visual_score: c.visual_similarity,
            })
            .collect::<Vec<_>>();
        let evidence = candidates
            .values()
            .flat_map(|c| c.context.clone())
            .collect::<Vec<_>>();
        rank_for_review(&ContextConfig::default(), &visual, &evidence, &revisions)?;
        self.commit_owned_unlocked(&upserts, &[])?;
        rank_context_review(candidates.into_values().collect(), enabled)
    }
}
fn context_words(value: &str) -> String {
    format!(
        " {} ",
        value
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    )
}
fn context_source_id(
    face: &FaceObservation,
    person: &Person,
    source: &ContextSource,
) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &face.face_id,
                face.face_revision,
                &face.media_fingerprint,
                &person.person_id,
                person.revision,
                source
            ))
            .map_err(|e| e.to_string())?
        )
    ))
}
fn context_evidence(
    face: &FaceObservation,
    person: &Person,
    source: ContextSource,
) -> Result<ContextEvidence, String> {
    let source_id = context_source_id(face, person, &source)?;
    Ok(ContextEvidence {
        version: 1,
        evidence_id: format!("context-{source_id}"),
        candidate_person_id: person.person_id.clone(),
        source_id,
        source_revision: person.revision,
        source,
        score: 1.0,
    })
}
