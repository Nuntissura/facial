//! Context only orders already-existing review candidates; it cannot assign identity.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextSource {
    Folder { path: String },
    Filename { name: String },
    Time { unix_millis: i64 },
    Album { album_id: String },
    Cooccurrence { person_id: String },
}
impl ContextSource {
    fn kind(&self) -> usize {
        match self {
            Self::Folder { .. } => 0,
            Self::Filename { .. } => 1,
            Self::Time { .. } => 2,
            Self::Album { .. } => 3,
            Self::Cooccurrence { .. } => 4,
        }
    }
    fn text(&self) -> Option<&str> {
        match self {
            Self::Folder { path } => Some(path),
            Self::Filename { name } => Some(name),
            Self::Album { album_id } => Some(album_id),
            Self::Cooccurrence { person_id } => Some(person_id),
            Self::Time { .. } => None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContextEvidence {
    pub version: u32,
    pub evidence_id: String,
    pub candidate_person_id: String,
    pub source_id: String,
    pub source_revision: u64,
    pub source: ContextSource,
    pub score: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    pub version: u32,
    pub max_candidates: usize,
    pub max_evidence: usize,
    pub max_string_bytes: usize,
    pub max_total_bytes: usize,
    pub source_weights: [f32; 5],
    pub context_weight: f32,
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            version: 1,
            max_candidates: 512,
            max_evidence: 2560,
            max_string_bytes: 1024,
            max_total_bytes: 1024 * 1024,
            source_weights: [0.2; 5],
            context_weight: 0.1,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VisualReviewCandidate {
    pub person_id: String,
    pub visual_score: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RankedReviewCandidate {
    pub person_id: String,
    pub visual_score: f32,
    pub context_score: f32,
    pub review_score: f32,
    pub evidence_ids: Vec<String>,
}
fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
pub fn rank_for_review(
    config: &ContextConfig,
    candidates: &[VisualReviewCandidate],
    evidence: &[ContextEvidence],
    current_source_revisions: &BTreeMap<String, u64>,
) -> Result<Vec<RankedReviewCandidate>, String> {
    if config.version != 1
        || config.max_candidates == 0
        || config.max_candidates > 512
        || config.max_evidence == 0
        || config.max_evidence > 2560
        || config.max_string_bytes == 0
        || config.max_string_bytes > 1024
        || config.max_total_bytes == 0
        || config.max_total_bytes > 1024 * 1024
        || !config.context_weight.is_finite()
        || !(0.0..=1.0).contains(&config.context_weight)
        || config
            .source_weights
            .iter()
            .any(|w| !w.is_finite() || !(0.0..=1.0).contains(w))
        || config.source_weights.iter().sum::<f32>() > 1.000001
        || candidates.len() > config.max_candidates
        || evidence.len() > config.max_evidence
    {
        return Err("invalid context configuration or caps".into());
    }
    let mut ranked = BTreeMap::new();
    for candidate in candidates {
        if !bounded_text(&candidate.person_id, config.max_string_bytes)
            || !candidate.visual_score.is_finite()
            || !(-1.0..=1.0).contains(&candidate.visual_score)
            || ranked
                .insert(
                    candidate.person_id.clone(),
                    RankedReviewCandidate {
                        person_id: candidate.person_id.clone(),
                        visual_score: candidate.visual_score,
                        context_score: 0.0,
                        review_score: candidate.visual_score,
                        evidence_ids: Vec::new(),
                    },
                )
                .is_some()
        {
            return Err("invalid or duplicate visual candidate".into());
        }
    }
    let mut ids = BTreeSet::new();
    let mut votes = BTreeSet::new();
    let mut ordered_evidence = evidence.iter().collect::<Vec<_>>();
    ordered_evidence.sort_by(|a, b| {
        (&a.candidate_person_id, a.source.kind(), &a.evidence_id).cmp(&(
            &b.candidate_person_id,
            b.source.kind(),
            &b.evidence_id,
        ))
    });
    for item in ordered_evidence {
        if item.version != 1
            || item.source_revision == 0
            || !item.score.is_finite()
            || !(0.0..=1.0).contains(&item.score)
            || [
                &item.evidence_id,
                &item.candidate_person_id,
                &item.source_id,
            ]
            .iter()
            .any(|s| !bounded_text(s, config.max_string_bytes))
            || item
                .source
                .text()
                .is_some_and(|s| !bounded_text(s, config.max_string_bytes))
            || current_source_revisions.get(&item.source_id) != Some(&item.source_revision)
            || !ids.insert(&item.evidence_id)
            || !votes.insert((&item.candidate_person_id, item.source.kind()))
        {
            return Err("invalid, duplicated or stale context provenance".into());
        }
        let candidate = ranked
            .get_mut(&item.candidate_person_id)
            .ok_or("context cannot inject a visually ineligible candidate")?;
        candidate.context_score += item.score * config.source_weights[item.source.kind()];
        candidate.evidence_ids.push(item.evidence_id.clone());
    }
    let mut budget = ContextByteBudget(config.max_total_bytes);
    serde_json::to_writer(&mut budget, &(candidates, evidence))
        .map_err(|_| "context byte budget exceeded".to_string())?;
    let mut ranked = ranked.into_values().collect::<Vec<_>>();
    for candidate in &mut ranked {
        candidate.review_score =
            candidate.visual_score + config.context_weight * candidate.context_score;
        candidate.evidence_ids.sort();
    }
    ranked.sort_by(|a, b| {
        b.review_score
            .total_cmp(&a.review_score)
            .then_with(|| a.person_id.cmp(&b.person_id))
    });
    Ok(ranked)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContextReviewCandidate {
    pub person_id: String,
    pub visual_similarity: f32,
    pub context: Vec<ContextEvidence>,
}
impl ContextReviewCandidate {
    pub fn validate(&self) -> Result<(), String> {
        if self.person_id.is_empty()
            || self.person_id.len() > 1024
            || !self.visual_similarity.is_finite()
            || !(-1.0..=1.0).contains(&self.visual_similarity)
            || self.context.len() > 5
        {
            return Err("invalid bounded context review candidate".into());
        }
        let mut sources = BTreeSet::new();
        for evidence in &self.context {
            if !sources.insert(evidence.source.kind())
                || !evidence.score.is_finite()
                || !(0.0..=1.0).contains(&evidence.score)
                || evidence.version != 1
                || evidence.source_revision == 0
                || evidence.candidate_person_id != self.person_id
                || [&evidence.evidence_id, &evidence.source_id]
                    .iter()
                    .any(|v| !bounded_text(v, 1024))
                || evidence
                    .source
                    .text()
                    .is_some_and(|s| !bounded_text(s, 1024))
            {
                return Err("invalid separate context evidence".into());
            }
        }
        Ok(())
    }
    pub fn context_score(&self) -> f32 {
        let mut scores = [0.0; 5];
        for evidence in &self.context {
            scores[evidence.source.kind()] = evidence.score * 0.2;
        }
        scores.into_iter().sum()
    }
}
struct ContextByteBudget(usize);
impl std::io::Write for ContextByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("context byte limit"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Input is an already visually selected review set. Context neither expands it
/// nor changes the independently exposed visual score or any persisted truth.
pub fn rank_context_review(
    mut candidates: Vec<ContextReviewCandidate>,
    enabled: bool,
) -> Result<Vec<ContextReviewCandidate>, String> {
    if candidates.len() > 512 {
        return Err("context review candidate bound exceeded".into());
    }
    let mut ids = BTreeSet::new();
    for candidate in &candidates {
        candidate.validate()?;
        if !ids.insert(&candidate.person_id) {
            return Err("duplicate context review candidate".into());
        }
    }
    candidates.sort_by(|a, b| {
        let score = |candidate: &ContextReviewCandidate| {
            candidate.visual_similarity
                + if enabled {
                    0.1 * candidate.context_score()
                } else {
                    0.0
                }
        };
        score(b)
            .total_cmp(&score(a))
            .then_with(|| b.visual_similarity.total_cmp(&a.visual_similarity))
            .then_with(|| a.person_id.cmp(&b.person_id))
    });
    Ok(candidates)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_requires_current_provenance_and_cannot_inject_candidates() {
        let candidate = VisualReviewCandidate {
            person_id: "p1".into(),
            visual_score: 0.8,
        };
        let evidence = ContextEvidence {
            version: 1,
            evidence_id: "e1".into(),
            candidate_person_id: "p1".into(),
            source_id: "s1".into(),
            source_revision: 2,
            source: ContextSource::Folder {
                path: "photos/p1".into(),
            },
            score: 1.0,
        };
        let revisions = BTreeMap::from([("s1".into(), 2)]);
        let ranked = rank_for_review(
            &ContextConfig::default(),
            &[candidate.clone()],
            &[evidence.clone()],
            &revisions,
        )
        .unwrap();
        assert_eq!(ranked[0].visual_score, 0.8);
        assert_eq!(ranked[0].context_score, 0.2);
        assert_eq!(ranked[0].evidence_ids, vec!["e1"]);
        assert!(rank_for_review(
            &ContextConfig::default(),
            &[candidate.clone()],
            &[evidence.clone()],
            &BTreeMap::from([("s1".into(), 1)])
        )
        .is_err());
        assert!(rank_for_review(
            &ContextConfig::default(),
            &[],
            &[evidence.clone()],
            &revisions
        )
        .is_err());
        let mut duplicate = evidence.clone();
        duplicate.evidence_id = "e2".into();
        assert!(rank_for_review(
            &ContextConfig::default(),
            &[candidate.clone()],
            &[evidence, duplicate],
            &revisions
        )
        .is_err());
        let mut config = ContextConfig::default();
        config.context_weight = f32::NAN;
        assert!(rank_for_review(&config, &[candidate], &[], &revisions).is_err());
    }
    #[test]
    fn context_order_is_permutation_invariant_and_strings_are_bounded() {
        let candidates = vec![VisualReviewCandidate {
            person_id: "p".into(),
            visual_score: 0.8,
        }];
        let sources = [
            ContextSource::Folder {
                path: "folder".into(),
            },
            ContextSource::Filename {
                name: "file".into(),
            },
            ContextSource::Album {
                album_id: "album".into(),
            },
        ];
        let evidence = sources
            .into_iter()
            .zip([0.02, 0.02, 0.01])
            .enumerate()
            .map(|(i, (source, score))| ContextEvidence {
                version: 1,
                evidence_id: format!("e{i}"),
                candidate_person_id: "p".into(),
                source_id: format!("s{i}"),
                source_revision: 1,
                source,
                score,
            })
            .collect::<Vec<_>>();
        let revisions = evidence.iter().map(|e| (e.source_id.clone(), 1)).collect();
        let expected = rank_for_review(
            &ContextConfig::default(),
            &candidates,
            &evidence,
            &revisions,
        )
        .unwrap();
        let mut reversed = evidence.clone();
        reversed.reverse();
        assert_eq!(
            expected,
            rank_for_review(
                &ContextConfig::default(),
                &candidates,
                &reversed,
                &revisions
            )
            .unwrap()
        );
        let mut oversized = evidence;
        oversized[0].source = ContextSource::Folder {
            path: "x".repeat(1025),
        };
        assert!(rank_for_review(
            &ContextConfig::default(),
            &candidates,
            &oversized,
            &revisions
        )
        .is_err());
        let mut config = ContextConfig::default();
        config.max_total_bytes = 32;
        assert!(rank_for_review(&config, &candidates, &reversed, &revisions).is_err());
    }
    #[test]
    fn ablation_changes_only_order_and_keeps_visual_evidence() {
        let a = ContextReviewCandidate {
            person_id: "a".into(),
            visual_similarity: 0.8,
            context: vec![],
        };
        let b = ContextReviewCandidate {
            person_id: "b".into(),
            visual_similarity: 0.79,
            context: vec![ContextEvidence {
                version: 1,
                evidence_id: "e1".into(),
                candidate_person_id: "b".into(),
                source_id: "s1".into(),
                source_revision: 1,
                source: ContextSource::Folder {
                    path: "fixture-folder".into(),
                },
                score: 1.0,
            }],
        };
        assert_eq!(
            rank_context_review(vec![a.clone(), b.clone()], true).unwrap(),
            vec![b.clone(), a.clone()]
        );
        assert_eq!(
            rank_context_review(vec![b.clone(), a.clone()], false).unwrap(),
            vec![a, b]
        );
    }
}
