//! Supervised worker compute admission and durable quarantine (WP-086).
use super::*;

pub(super) const WP086_WORKER_SCHEMA_SQL: &str = r#"
DEFINE TABLE OVERWRITE match_worker_quarantine SCHEMAFULL;
DEFINE FIELD OVERWRITE worker_id ON match_worker_quarantine TYPE string;
DEFINE FIELD OVERWRITE job_id ON match_worker_quarantine TYPE string;
DEFINE FIELD OVERWRITE model_generation ON match_worker_quarantine TYPE string;
DEFINE FIELD OVERWRITE code ON match_worker_quarantine TYPE string;
DEFINE FIELD OVERWRITE message ON match_worker_quarantine TYPE string;
DEFINE FIELD OVERWRITE confirmed_dead ON match_worker_quarantine TYPE bool;
DEFINE FIELD OVERWRITE created_at ON match_worker_quarantine TYPE string;
DEFINE INDEX OVERWRITE match_worker_quarantine_id ON match_worker_quarantine FIELDS worker_id UNIQUE;
DEFINE INDEX OVERWRITE match_worker_quarantine_job ON match_worker_quarantine FIELDS job_id, worker_id;
DEFINE INDEX OVERWRITE match_worker_quarantine_generation ON match_worker_quarantine FIELDS model_generation, confirmed_dead;
"#;
const QUARANTINE: &str = "match_worker_quarantine";

#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue)]
pub struct WorkerQuarantine {
    pub worker_id: String,
    pub job_id: String,
    pub model_generation: String,
    pub code: String,
    pub message: String,
    pub confirmed_dead: bool,
    pub created_at: String,
}

pub struct MatchComputePermit {
    io: Option<IoPermit>,
    resources: Option<MatchResourceLease>,
    epoch: u64,
}
impl MatchComputePermit {
    pub(crate) fn cpu_units(&self) -> u64 {
        self.resources
            .as_ref()
            .map_or(0, |lease| lease.request.cpu_inference)
    }
    pub fn admission_epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn finish_after_worker(
        mut self,
        outcome: PermitOutcome,
        worker: &crate::match_worker::IsolatedMatchWorker,
    ) {
        if let Some(io) = self.io.take() {
            io.finish(outcome);
        }
        if let Some(resources) = self.resources.take() {
            worker.finish_compute_resources(resources);
        }
    }

    pub fn finish(mut self, outcome: PermitOutcome) {
        if let Some(io) = self.io.take() {
            io.finish(outcome);
        }
        self.resources.take();
    }
}

impl MatchStore {
    pub fn external_admission_epoch(&self) -> u64 {
        self.external_holds.snapshot()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn acquire_worker_compute(
        &self,
        coordinator: &MediaIoCoordinator,
        root: RootIdentity,
        job_id: &str,
        fence: Option<&RevisionFence>,
        request: ResourceRequest,
        snapshot: &mut Option<MatchResourceLease>,
    ) -> Result<MatchComputePermit, String> {
        if request.worker_memory_bytes != 0
            || request.surreal_writes != 0
            || request.vector_index_builds != 0
            || request.cpu_inference == 0
            || request.admitted_items == 0
            || request.queued_items == 0
            || request.queued_bytes == 0
        {
            return Err(
                "worker compute requires bounded inference accounting and zero writer/index leases"
                    .into(),
            );
        }
        let check = || -> Result<(), String> {
            let _database_unit = self.begin_database_unit()?;
            let _guard = self.database_read_guard("worker admission lock poisoned")?;
            let job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
            let lifecycle = job.lifecycle()?;
            let blocking_holds = self
                .transient_holds
                .lock()
                .map_err(|_| "worker hold lock poisoned")?
                .iter()
                .any(|reason| *reason != HoldReason::ResourcePressure);
            if self.external_holds.blocked()
                || !lifecycle.is_runnable()
                || lifecycle.is_terminal_or_failed()
                || blocking_holds
                || DesiredMode::parse(&self.execution_state_unlocked()?.desired_mode)?
                    != DesiredMode::Running
            {
                return Err("worker admission paused, held, or terminal".into());
            }
            self.require_worker_exit_before_retry_unlocked(job_id)?;
            if let Some(fence) = fence {
                if fence.job_id != job_id {
                    return Err("worker job fence mismatch".into());
                }
                self.require_valid_asset_fence_unlocked(fence, false)?;
            }
            Ok(())
        };
        let epoch = self.external_admission_epoch();
        check()?;
        let io = coordinator
            .enqueue(root, WorkClass::Background)
            .wait()
            .map_err(|e| e.to_string())?;
        check()?;
        if epoch != self.external_admission_epoch() {
            io.finish(PermitOutcome::Cancelled);
            return Err("worker admission changed while queued".into());
        }
        let resources = if let Some(lease) = snapshot.as_mut() {
            self.governor.try_replace(lease, request)?;
            snapshot.take().ok_or("worker snapshot lease disappeared")?
        } else {
            self.governor.try_acquire(request)?
        };
        check()?;
        if epoch != self.external_admission_epoch() {
            io.finish(PermitOutcome::Cancelled);
            return Err("worker admission changed before compute".into());
        }
        Ok(MatchComputePermit {
            io: Some(io),
            resources: Some(resources),
            epoch,
        })
    }

    pub(super) fn require_worker_exit_before_retry_unlocked(
        &self,
        job_id: &str,
    ) -> Result<(), String> {
        let db = self.database();
        let job_id = job_id.to_string();
        let job: IndexJob = self.require_unlocked(JOB_TABLE, &job_id, "IndexJob")?;
        let records: Vec<WorkerQuarantine> = surreal_store::run(async move {
            let mut rows=db.query("SELECT * OMIT id FROM match_worker_quarantine WITH INDEX match_worker_quarantine_generation WHERE model_generation=$generation AND confirmed_dead=false LIMIT 1;").bind(("generation",job.model_generation)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
            rows.take(0).map_err(|e| e.to_string())
        })?;
        if !records.is_empty() {
            return Err("quarantined worker exit is not confirmed; retry blocked".into());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_worker_quarantine(
        &self,
        job_id: &str,
        fence: Option<&RevisionFence>,
        worker_id: &str,
        model_generation: &str,
        code: &str,
        message: &str,
        confirmed_dead: bool,
    ) -> Result<(), String> {
        let _database_unit = self.begin_database_unit()?;
        validate_text("worker ID", worker_id)?;
        validate_text("worker failure", message)?;
        if !matches!(
            code,
            "safe_unit_timeout" | "worker_failed" | "worker_protocol" | "stale_worker_result"
        ) {
            return Err("invalid isolated worker failure code".into());
        }
        let _guard = self.mutation_write_guard("worker quarantine")?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if job.model_generation != model_generation {
            return Err("worker generation mismatch".into());
        }
        if let Some(fence) = fence {
            if fence.job_id != job_id {
                return Err("worker quarantine job mismatch".into());
            }
            let asset: JobAsset = self.require_unlocked(
                JOB_ASSET_TABLE,
                &job_asset_id(job_id, &fence.media_key),
                "job asset",
            )?;
            // A stale result is itself quarantinable. It cannot authorize any
            // asset truth write, but must not erase the worker's death record.
            let _ = validate_asset_fence(&asset, fence);
        }
        // This terminal diagnostic does not admit new inference and remains legal
        // after a playback hold or operator pause lands. Failed fences reject late writes.
        if !matches!(
            job.lifecycle()?,
            JobLifecycle::Cancelled | JobLifecycle::Paused
        ) {
            job.lifecycle = JobLifecycle::Failed.as_str().into();
        }
        job.failure_code = Some(code.into());
        job.failure_message = Some(message.into());
        job.updated_at = now();
        let row = WorkerQuarantine {
            worker_id: worker_id.into(),
            job_id: job_id.into(),
            model_generation: model_generation.into(),
            code: code.into(),
            message: message.into(),
            confirmed_dead,
            created_at: now(),
        };
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    QUARANTINE,
                    worker_id,
                    serde_json::to_value(row).map_err(|e| e.to_string())?,
                ),
                (
                    JOB_TABLE,
                    job_id,
                    serde_json::to_value(job).map_err(|e| e.to_string())?,
                ),
            ],
            &[],
        )
    }
    pub fn acknowledge_worker_exit(&self, worker_id: &str) -> Result<(), String> {
        let _database_unit = self.begin_database_unit()?;
        let _guard = self.mutation_write_guard("worker exit confirmation")?;
        let mut row: WorkerQuarantine =
            self.require_unlocked(QUARANTINE, worker_id, "worker quarantine")?;
        row.confirmed_dead = true;
        self.transactional_upserts_deletes_unlocked(
            &[(
                QUARANTINE,
                worker_id,
                serde_json::to_value(row).map_err(|e| e.to_string())?,
            )],
            &[],
        )
    }
}
