//! Match admission and lock adapter for the shared database owner (WP-086).
use super::*;

pub(super) struct MatchMutationGuard<'a> {
    _lock: std::sync::RwLockWriteGuard<'a, ()>,
    _scope: surreal_store::MatchUnitScope,
}

impl<'a> MatchMutationGuard<'a> {
    pub(super) fn new(
        lock: std::sync::RwLockWriteGuard<'a, ()>,
        scope: surreal_store::MatchUnitScope,
    ) -> Self {
        Self {
            _lock: lock,
            _scope: scope,
        }
    }
}

impl MatchStore {
    #[cfg(test)]
    pub(crate) fn test_delay_next_checkpoint_ack(&self, delay: std::time::Duration) {
        *self.next_checkpoint_ack_delay.lock().unwrap() = Some(delay);
    }

    pub(crate) fn resolve_pending_database_failures(
        &self,
        job_id: Option<&str>,
    ) -> Result<(), String> {
        let mut pending = self
            .pending_database_failures
            .lock()
            .map_err(|_| "Match pending database failures are poisoned".to_string())?;
        if let Some(job_id) = job_id {
            pending.retain(|record| record["job_id"].as_str() != Some(job_id));
        } else {
            pending.clear();
        }
        Ok(())
    }
    pub(crate) fn record_pending_database_failure(
        &self,
        job_id: &str,
        failure_code: &str,
        error: &str,
    ) {
        let mut operation_id = error.split("operation_id=").nth(1).and_then(|suffix| {
            let id = suffix.split(|ch: char| !ch.is_ascii_hexdigit()).next()?;
            (id.len() == 32).then(|| id.to_string())
        });
        if let Ok(mut pending) = self.pending_database_failures.lock() {
            let job_id = job_id.chars().take(128).collect::<String>();
            if operation_id.is_none() {
                operation_id = pending
                    .iter()
                    .find(|record| record["job_id"].as_str() == Some(job_id.as_str()))
                    .and_then(|record| record["operation_id"].as_str())
                    .map(str::to_string);
            }
            pending.retain(|record| record["job_id"].as_str() != Some(job_id.as_str()));
            if pending.len() == 16 {
                pending.remove(0);
                self.pending_database_failures_evicted
                    .fetch_add(1, Ordering::AcqRel);
            }
            pending.push(json!({
                "job_id": job_id,
                "failure_code": redacted_failure_code(failure_code),
                "recording_error_code": database_error_code(error),
                "operation_id": operation_id,
                "recording_acknowledged": false,
                "recorded_at": now(),
            }));
        }
    }

    pub(super) fn pending_database_failure_snapshot(&self) -> Option<Vec<Value>> {
        self.pending_database_failures
            .try_lock()
            .ok()
            .map(|pending| pending.clone())
    }

    pub(super) fn database_recovery_pending(&self) -> bool {
        let owner = self.store.owner_diagnostics();
        owner["phase"].as_str() != Some("ready")
            || owner["unreconciled_operations"]
                .as_u64()
                .is_some_and(|count| count > 0)
            || self
                .transient_holds
                .try_lock()
                .ok()
                .is_some_and(|holds| holds.contains(&HoldReason::DatabaseOwnerQuarantined))
            || self
                .pending_database_failures
                .try_lock()
                .ok()
                .is_some_and(|pending| !pending.is_empty())
    }

    /// Preserve canonical integrity errors; only owner failures become snapshots.
    pub(super) fn database_diagnostic_result(
        &self,
        result: Result<Value, String>,
    ) -> Result<Value, String> {
        result.or_else(|error| {
            if database_unavailability_error(&error) {
                Ok(self.database_recovery_snapshot(&error))
            } else {
                Err(error)
            }
        })
    }

    /// Parent-held diagnostics never query the unavailable canonical store.
    pub(super) fn database_recovery_snapshot(&self, error: &str) -> Value {
        let pending = self.pending_database_failure_snapshot();
        let failure_recording = if pending.as_ref().is_some_and(|records| !records.is_empty()) {
            "pending_recovery"
        } else {
            "unavailable"
        };
        let holds = self.transient_holds.try_lock().ok().map(|holds| {
            let mut reasons = holds
                .iter()
                .map(|reason| reason.as_str().to_string())
                .collect::<BTreeSet<_>>();
            let external = self.external_holds.snapshot();
            if external & 1 != 0 {
                reasons.insert("viewer_playback".into());
            }
            if external & 2 != 0 {
                reasons.insert("immersive_fullscreen".into());
            }
            reasons.into_iter().collect::<Vec<_>>()
        });
        json!({
            "availability": "unavailable",
            "canonical_state": {"availability": "unavailable", "error_code": database_error_code(error)},
            "execution": {
                "desired_mode": null,
                "effective_state": "unavailable",
                "identity_revision": null,
                "catalog_revision": null,
                "database_owner": self.store.owner_diagnostics(),
                "transient_holds": holds,
                "holds": holds,
                "pending_failure_records": pending,
                "pending_failure_records_capacity": 16,
                "pending_failure_records_evicted": self.pending_database_failures_evicted.load(Ordering::Acquire),
                "pending_failure_records_authoritative": false,
                "failure_recording": failure_recording,
            },
            "privacy": {"embeddings_in_status": false, "face_crops_in_status": false,
                "database_handles_in_status": false, "failure_messages": false},
        })
    }

    /// Lazy initialization yields to Media between SQL requests. Its native
    /// queries use the background deadline; explicit operator recovery keeps
    /// the foreground lane. This does not bound filesystem initialization.
    pub(super) fn database(&self) -> surreal_store::EmbeddedDb {
        if self.initializing {
            self.store.match_db()
        } else {
            self.store.db()
        }
    }

    /// One bounded control/read unit. Nested calls keep the enclosing deadline.
    pub(crate) fn begin_database_unit(&self) -> Result<surreal_store::MatchUnitScope, String> {
        let deadline = self
            .store
            .match_unit_deadline()
            .unwrap_or_else(|| std::time::Instant::now() + crate::match_worker::SAFE_UNIT_LIMIT);
        self.store.begin_match_unit(deadline)
    }

    pub(crate) fn reconcile_database_operations(&self) -> Result<(), String> {
        self.store.reconcile_pending_operations()
    }

    pub(super) fn database_read_guard(
        &self,
        label: &str,
    ) -> Result<std::sync::RwLockReadGuard<'_, ()>, String> {
        let Some(deadline) = self.store.match_unit_deadline() else {
            return self
                .store
                .transaction_lock()
                .read()
                .map_err(|_| label.to_string());
        };
        loop {
            require_persist_time_remaining(deadline)?;
            match self.store.transaction_lock().try_read() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(label.to_string()),
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(
                    deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .min(std::time::Duration::from_millis(2)),
                ),
            }
        }
    }
}

fn database_unavailability_error(error: &str) -> bool {
    let markers = [
        "safe_unit_timeout",
        "commit_outcome_unknown",
        "database_owner_exit_pending",
        "database_owner_epoch_changed",
        "database_owner_exited",
        "database_owner_pipe_backpressure",
        "database_owner_queue_limit",
        "database_owner_queue_bytes_limit",
        "database_owner_uncertainty_limit",
        "database owner unavailable",
        "database owner state poisoned",
        "database outcome registry poisoned",
        "database owner startup protocol mismatch",
        "spawn database owner",
    ];
    let is_owner_component = |part: &str| {
        let part = part.trim_start_matches('"');
        markers.iter().any(|marker| {
            part.strip_prefix(marker).is_some_and(|suffix| {
                suffix.is_empty()
                    || suffix
                        .chars()
                        .next()
                        .is_some_and(|ch| matches!(ch, ':' | ';' | ' ' | '"'))
            })
        })
    };
    if is_owner_component(error) {
        return true;
    }
    let Some((wrapper, owner_error)) = error.split_once(": ") else {
        return false;
    };
    // These query-await wrappers are fixed code labels. Decoder and semantic
    // errors may quote arbitrary persisted text and must never be segmented.
    if wrapper != "query recent Match jobs"
        && ![
            "read Match ",
            "query Match ",
            "count Match ",
            "commit Match ",
        ]
        .iter()
        .any(|prefix| wrapper.starts_with(prefix))
    {
        return false;
    }
    owner_error.split("; ").any(is_owner_component)
}

fn database_error_code(error: &str) -> &'static str {
    for code in [
        "commit_outcome_unknown",
        "database_owner_exit_pending",
        "database_owner_epoch_changed",
        "safe_unit_timeout",
        "database_recovery_pending",
    ] {
        if error.contains(code) {
            return code;
        }
    }
    "canonical_state_unavailable"
}
