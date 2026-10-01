use super::*;

/// Query terms are bounded independently from the Media inventory. Normal
/// searches without `person:` terms never call this projection.
pub const PERSON_SEARCH_TERM_LIMIT: usize = 64;
/// Matches the existing million-row Media search contract while preventing a
/// corrupt identity graph from forcing unbounded allocation.
pub const PERSON_SEARCH_MEDIA_LIMIT: usize = 1_000_000;
const PERSON_SEARCH_PAGE: usize = 512;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonMembershipProjection {
    pub identity_revision: u64,
    pub catalog_revision: u64,
    /// Stable Person ID -> canonical media keys. Every vector is sorted and
    /// unique, so consumers can use binary search without building another
    /// unbounded map.
    pub media_keys_by_person: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonGalleryInventoryRow {
    pub media_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonGalleryInventory {
    pub person: Person,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub total_media: usize,
    pub rows: Vec<PersonGalleryInventoryRow>,
    pub unresolved_media_keys: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum PersonGalleryInventoryOutcome {
    Present(PersonGalleryInventory),
    Missing {
        person_id: String,
        identity_revision: u64,
        catalog_revision: u64,
    },
}

impl MatchStore {
    fn person_media_keys_unlocked(&self, person_id: &str) -> Result<Vec<String>, String> {
        let db = self.database();
        let person_id = person_id.to_string();
        let rows: Vec<String> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT VALUE media_key FROM match_assignment WITH INDEX match_assignment_person \
                     WHERE person_id = $person_id GROUP BY media_key ORDER BY media_key ASC \
                     LIMIT $limit;",
                )
                .bind(("person_id", person_id))
                .bind(("limit", PERSON_SEARCH_MEDIA_LIMIT as u64 + 1))
                .await
                .map_err(|error| format!("query Match Person search membership: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match Person search membership: {error}"))
        })?;
        if rows.len() > PERSON_SEARCH_MEDIA_LIMIT {
            return Err(format!(
                "Match Person search inventory exceeds {PERSON_SEARCH_MEDIA_LIMIT} media rows"
            ));
        }
        Ok(rows)
    }

    /// Exact stable-ID membership for the Person terms in one Media query.
    /// Missing IDs fail closed; a display name is never promoted to an ID.
    pub fn person_membership_projection(
        &self,
        person_ids: &[String],
    ) -> Result<PersonMembershipProjection, String> {
        let mut person_ids = person_ids
            .iter()
            .cloned()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        person_ids.sort();
        person_ids.dedup();
        if person_ids.len() > PERSON_SEARCH_TERM_LIMIT {
            return Err(format!(
                "person search exceeds {PERSON_SEARCH_TERM_LIMIT} stable Person IDs"
            ));
        }
        let _guard = self.database_read_guard("Match Person search snapshot lock is poisoned")?;
        let execution = self.execution_state_unlocked()?;
        let mut media_keys_by_person = BTreeMap::new();
        let mut total_memberships = 0usize;
        for person_id in person_ids {
            let _: Person = self.require_unlocked(PERSON_TABLE, &person_id, "Person")?;
            let media_keys = self.person_media_keys_unlocked(&person_id)?;
            total_memberships = total_memberships.saturating_add(media_keys.len());
            if total_memberships > PERSON_SEARCH_MEDIA_LIMIT {
                return Err(format!(
                    "person search projection exceeds {PERSON_SEARCH_MEDIA_LIMIT} memberships"
                ));
            }
            media_keys_by_person.insert(person_id.clone(), media_keys);
        }
        Ok(PersonMembershipProjection {
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            media_keys_by_person,
        })
    }

    /// The same revision-bound Person/alias catalog used by Viewer editing,
    /// exposed without asking the Media UI to guess the current revision.
    pub fn person_search_autocomplete(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Person>, String> {
        let catalog_revision = {
            let _guard =
                self.database_read_guard("Match autocomplete revision lock is poisoned")?;
            self.execution_state_unlocked()?.catalog_revision
        };
        self.correction_autocomplete(query, catalog_revision, limit)
    }

    /// Full canonical Match-backed inventory for a persisted Person tab. The
    /// database read is bounded and runs only on a background service path;
    /// render virtualization remains independent of this materialization.
    pub fn person_gallery_inventory(
        &self,
        person_id: &str,
    ) -> Result<PersonGalleryInventory, String> {
        match self.person_gallery_inventory_outcome(person_id)? {
            PersonGalleryInventoryOutcome::Present(inventory) => Ok(inventory),
            PersonGalleryInventoryOutcome::Missing { .. } => {
                Err(format!("Person {person_id} does not exist"))
            }
        }
    }

    pub fn person_gallery_inventory_outcome(
        &self,
        person_id: &str,
    ) -> Result<PersonGalleryInventoryOutcome, String> {
        let _guard = self.database_read_guard("Match gallery inventory lock is poisoned")?;
        let execution = self.execution_state_unlocked()?;
        let Some(person) = self.get_one_unlocked::<Person>(PERSON_TABLE, person_id)? else {
            return Ok(PersonGalleryInventoryOutcome::Missing {
                person_id: person_id.to_string(),
                identity_revision: execution.identity_revision,
                catalog_revision: execution.catalog_revision,
            });
        };
        let media_keys = self.person_media_keys_unlocked(person_id)?;
        let mut resolved = BTreeMap::<String, String>::new();
        for chunk in media_keys.chunks(PERSON_SEARCH_PAGE) {
            let page_keys = chunk.to_vec();
            let db = self.database();
            let values: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "(SELECT id, media_key FROM match_job_asset WITH INDEX match_job_asset_media \
                         WHERE media_key IN $page_keys GROUP BY media_key ORDER BY media_key ASC LIMIT 512) \
                         .map(|$group| { SELECT media_key, source_path, updated_at, asset_id FROM ONLY \
                         $group.id ORDER BY updated_at DESC, asset_id ASC LIMIT 1 });",
                    )
                    .bind(("page_keys", page_keys))
                    .await
                    .map_err(|error| format!("query Match gallery inventory paths: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Match gallery inventory paths: {error}"))
            })?;
            for (key, path) in values.into_iter().filter_map(grouped_media_source_path) {
                resolved.entry(key).or_insert(path);
            }
        }
        let rows = media_keys
            .iter()
            .map(|media_key| PersonGalleryInventoryRow {
                media_key: media_key.clone(),
                source_path: resolved.get(media_key).cloned(),
            })
            .collect::<Vec<_>>();
        let unresolved_media_keys = rows
            .iter()
            .filter(|row| row.source_path.is_none())
            .map(|row| row.media_key.clone())
            .collect();
        Ok(PersonGalleryInventoryOutcome::Present(
            PersonGalleryInventory {
                person,
                identity_revision: execution.identity_revision,
                catalog_revision: execution.catalog_revision,
                total_media: rows.len(),
                rows,
                unresolved_media_keys,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        media_search::{
            active_person_token, parse_query, rank, rank_indexed, IndexedMediaRow, IndexedRowMeta,
            LatestSearchRequests, MediaSearchIndex, RankMode, RowMeta, SearchIndexGeneration,
        },
        media_tabs::MediaTabsState,
    };
    use std::path::{Path, PathBuf};

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial-wp085-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn close(root: &Path, store: MatchStore) {
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stable_id_search_and_full_gallery_cover_alias_rename_row_513_and_unresolved_paths() {
        let root = workspace("person-search-gallery");
        let store = MatchStore::open(&root).unwrap();
        let first = store
            .create_person("Duplicate Name", vec!["Shared Alias".to_string()])
            .unwrap();
        let second = store
            .create_person("Duplicate Name", vec!["Shared Alias".to_string()])
            .unwrap();

        let matches = store.person_search_autocomplete("shared", 8).unwrap();
        assert_eq!(matches.len(), 2);
        assert!(matches
            .iter()
            .any(|person| person.person_id == first.person_id));
        assert!(matches
            .iter()
            .any(|person| person.person_id == second.person_id));
        assert!(store
            .person_membership_projection(&["Duplicate Name".to_string()])
            .unwrap_err()
            .contains("does not exist"));

        let renamed = store
            .update_person(
                &first.person_id,
                first.revision,
                "Renamed Person",
                vec!["Renamed Alias".to_string()],
            )
            .unwrap();
        let renamed_matches = store.person_search_autocomplete("renamed", 8).unwrap();
        assert_eq!(renamed_matches.len(), 1);
        assert_eq!(renamed_matches[0].person_id, first.person_id);
        assert_eq!(
            renamed_matches[0].catalog_revision,
            renamed.catalog_revision
        );
        let shared_matches = store.person_search_autocomplete("shared", 8).unwrap();
        assert_eq!(shared_matches.len(), 1);
        assert_eq!(shared_matches[0].person_id, second.person_id);

        let timestamp = now();
        let encoded = (0..513)
            .map(|index| {
                let assignment_id = format!("assignment-{index:04}");
                let assignment = Assignment {
                    assignment_id: assignment_id.clone(),
                    face_id: format!("face-{index:04}"),
                    person_id: first.person_id.clone(),
                    media_key: format!("media-{index:04}"),
                    look_id: None,
                    placement: "trusted".to_string(),
                    state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                    provenance: "wp085-search-test".to_string(),
                    locked: true,
                    model_generation: None,
                    calibration_generation: None,
                    envelope_hash: None,
                    face_revision: 1,
                    person_revision: renamed.revision,
                    operation_id: "wp085-search-test".to_string(),
                    created_at: timestamp.clone(),
                    updated_at: timestamp.clone(),
                };
                (assignment_id, serde_json::to_value(assignment).unwrap())
            })
            .collect::<Vec<_>>();
        let upserts = encoded
            .iter()
            .map(|(id, value)| (ASSIGNMENT_TABLE, id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        store.transactional_upserts_deletes(&upserts, &[]).unwrap();
        store
            .upsert_json(
                JOB_ASSET_TABLE,
                "asset-0512",
                &JobAsset {
                    asset_id: "asset-0512".to_string(),
                    job_id: "job-search-test".to_string(),
                    media_key: "media-0512".to_string(),
                    source_path: Some("resolved/row-0513.jpg".to_string()),
                    media_fingerprint: "fingerprint-0512".to_string(),
                    next_stage: "complete".to_string(),
                    completed_stages: Vec::new(),
                    failure_code: None,
                    failure_message: None,
                    skipped_code: None,
                    skipped_message: None,
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "model-search-test".to_string(),
                    identity_revision: 0,
                    catalog_revision: renamed.catalog_revision,
                    updated_at: timestamp,
                },
            )
            .unwrap();

        let projection = store
            .person_membership_projection(std::slice::from_ref(&first.person_id))
            .unwrap();
        let keys = projection
            .media_keys_by_person
            .get(&first.person_id)
            .unwrap();
        assert_eq!(keys.len(), 513);
        assert_eq!(keys[512], "media-0512");
        assert_eq!(projection.catalog_revision, renamed.catalog_revision);

        let gallery = store.person_gallery_inventory(&first.person_id).unwrap();
        assert_eq!(gallery.total_media, 513);
        assert_eq!(gallery.rows.len(), 513);
        assert_eq!(gallery.rows[512].media_key, "media-0512");
        assert_eq!(
            gallery.rows[512].source_path.as_deref(),
            Some("resolved/row-0513.jpg")
        );
        assert_eq!(gallery.unresolved_media_keys.len(), 512);
        assert!(!gallery
            .unresolved_media_keys
            .iter()
            .any(|media_key| media_key == "media-0512"));

        close(&root, store);
    }

    #[test]
    fn person_membership_projection_preserves_opaque_stable_id_bytes() {
        let root = workspace("person-search-opaque-id");
        let store = MatchStore::open(&root).unwrap();
        let mut person = store.create_person("Opaque", Vec::new()).unwrap();
        person.person_id = " Imported ID_AbC ".to_string();
        store
            .upsert_json(PERSON_TABLE, &person.person_id, &person)
            .unwrap();

        let projection = store
            .person_membership_projection(std::slice::from_ref(&person.person_id))
            .unwrap();
        assert!(projection
            .media_keys_by_person
            .contains_key(&person.person_id));
        assert!(store
            .person_membership_projection(&[person.person_id.trim().to_string()])
            .unwrap_err()
            .contains("does not exist"));

        close(&root, store);
    }

    #[test]
    fn wp085_person_search_acceptance_integrates_catalog_tokens_tabs_and_match_membership() {
        let root = workspace("person-search-acceptance");
        let store = MatchStore::open(&root).unwrap();
        let included = store
            .create_person("Alex Included", vec!["Camera Alias".to_string()])
            .unwrap();
        let excluded = store
            .create_person("Alex Excluded", vec!["Blocked Alias".to_string()])
            .unwrap();

        let included_completion = store.person_search_autocomplete("camera", 8).unwrap();
        assert_eq!(included_completion.len(), 1);
        assert_eq!(included_completion[0].person_id, included.person_id);
        let mut persisted_query = active_person_token("person:camera")
            .unwrap()
            .replace_with_person_id("person:camera", &included_completion[0].person_id)
            .unwrap();

        let excluded_completion = store.person_search_autocomplete("blocked", 8).unwrap();
        assert_eq!(excluded_completion.len(), 1);
        assert_eq!(excluded_completion[0].person_id, excluded.person_id);
        persisted_query.push_str(" !person:blocked");
        persisted_query = active_person_token(&persisted_query)
            .unwrap()
            .replace_with_person_id(&persisted_query, &excluded_completion[0].person_id)
            .unwrap();
        let parsed = parse_query(&persisted_query);
        assert_eq!(parsed.person_ids, vec![included.person_id.clone()]);
        assert_eq!(parsed.excluded.person_ids, vec![excluded.person_id.clone()]);

        let timestamp = now();
        let assignment_specs = [
            ("included-keep", &included, "media-keep"),
            ("included-shared", &included, "media-excluded"),
            ("excluded-shared", &excluded, "media-excluded"),
            ("excluded-only", &excluded, "media-other"),
        ];
        let encoded_assignments = assignment_specs
            .iter()
            .map(|(id, person, media_key)| {
                let assignment = Assignment {
                    assignment_id: format!("assignment-{id}"),
                    face_id: format!("face-{id}"),
                    person_id: person.person_id.clone(),
                    media_key: (*media_key).to_string(),
                    look_id: None,
                    placement: "unsorted".to_string(),
                    state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                    provenance: "wp085-person-search-acceptance".to_string(),
                    locked: true,
                    model_generation: None,
                    calibration_generation: None,
                    envelope_hash: None,
                    face_revision: 1,
                    person_revision: person.revision,
                    operation_id: format!("operation-{id}"),
                    created_at: timestamp.clone(),
                    updated_at: timestamp.clone(),
                };
                (
                    assignment.assignment_id.clone(),
                    serde_json::to_value(assignment).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let assignment_upserts = encoded_assignments
            .iter()
            .map(|(id, value)| (ASSIGNMENT_TABLE, id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        store
            .transactional_upserts_deletes(&assignment_upserts, &[])
            .unwrap();

        let mut tabs = MediaTabsState::default();
        tabs.open_folder_in_new_tab("library".to_string()).unwrap();
        tabs.active_mut().viewport.search_query = persisted_query.clone();
        let persisted_tabs = tabs.encode().unwrap();

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened_store = MatchStore::open(&root).unwrap();
        let reopened_tabs = MediaTabsState::decode(&persisted_tabs).unwrap();
        assert_eq!(
            reopened_tabs.active().viewport.search_query,
            persisted_query
        );
        let reopened_completion = reopened_store
            .person_search_autocomplete("camera", 8)
            .unwrap();
        assert_eq!(reopened_completion.len(), 1);
        assert_eq!(reopened_completion[0].person_id, included.person_id);

        let query = parse_query(&reopened_tabs.active().viewport.search_query);
        let projection = reopened_store
            .person_membership_projection(&[included.person_id.clone(), excluded.person_id.clone()])
            .unwrap();
        let media_keys = ["media-keep", "media-excluded", "media-other"];
        let memberships = media_keys
            .iter()
            .map(|media_key| {
                projection
                    .media_keys_by_person
                    .iter()
                    .filter_map(|(person_id, keys)| {
                        keys.binary_search_by(|key| key.as_str().cmp(media_key))
                            .is_ok()
                            .then(|| person_id.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let rows = media_keys
            .iter()
            .map(|media_key| {
                (
                    format!("{media_key}.jpg"),
                    format!("library/{media_key}.jpg"),
                )
            })
            .collect::<Vec<_>>();
        let reference_meta = memberships
            .iter()
            .map(|person_ids| RowMeta {
                person_ids: Some(person_ids),
                ..RowMeta::default()
            })
            .collect::<Vec<_>>();
        let reference_hits = rank(&rows, &reference_meta, &query, RankMode::Name, 0);

        let indexed_rows = rows
            .iter()
            .zip(&memberships)
            .enumerate()
            .map(|(index, ((name, path), person_ids))| {
                IndexedMediaRow::new(
                    index,
                    name.clone(),
                    path.clone(),
                    IndexedRowMeta::default().with_person_ids(person_ids.clone()),
                )
            })
            .collect::<Vec<_>>();
        let index = MediaSearchIndex::new(SearchIndexGeneration(85), indexed_rows);
        let coordinator = LatestSearchRequests::default();
        let (request, cancellation) =
            coordinator.begin(index.generation(), query, RankMode::Name, 0);
        let indexed_result = rank_indexed(&index, &request, &cancellation);

        assert!(indexed_result.is_complete());
        assert_eq!(indexed_result.hits, reference_hits);
        assert_eq!(reference_hits.len(), 1);
        assert_eq!(reference_hits[0].index, 0);

        close(&root, reopened_store);
    }
}
