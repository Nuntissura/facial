use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub struct DebugEvent {
    pub ts: String,
    pub level: String,
    pub source: String,
    pub message: String,
    pub payload: Value,
}

pub struct DebugBus {
    path: PathBuf,
    max_events: usize,
    events: VecDeque<DebugEvent>,
}

impl DebugBus {
    pub fn new(path: PathBuf, max_events: usize) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Self {
            path,
            max_events,
            events: VecDeque::with_capacity(max_events),
        }
    }

    pub fn emit(
        &mut self,
        level: &str,
        source: &str,
        message: &str,
        payload: Option<Value>,
    ) -> DebugEvent {
        let event = DebugEvent {
            ts: DateTime::<Utc>::from(std::time::SystemTime::now()).to_rfc3339(),
            level: level.to_ascii_uppercase(),
            source: source.to_string(),
            message: message.to_string(),
            payload: payload.unwrap_or_else(|| Value::Object(Default::default())),
        };
        if self.events.len() >= self.max_events {
            let _ = self.events.pop_front();
        }
        self.events.push_back(event.clone());

        if let Ok(serialized) = serde_json::to_string(&event) {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = file.write_all(serialized.as_bytes());
                let _ = file.write_all(b"\n");
            }
        }
        event
    }

    /// Record a model-applied UI action as a structured event via self.emit
    /// (reuses ring cap + jsonl append). level=INFO when applied else WARN.
    pub fn record_applied_action(
        &mut self,
        command_id: &str,
        intent: &str,
        applied: bool,
        message: &str,
        snapshot: serde_json::Value,
    ) -> DebugEvent {
        let level = if applied { "INFO" } else { "WARN" };
        self.emit(
            level,
            "ModelAction",
            &format!("applied={applied} intent={intent} id={command_id} :: {message}"),
            Some(serde_json::json!({
                "command_id": command_id, "intent": intent,
                "applied": applied, "message": message, "state": snapshot,
            })),
        )
    }

    pub fn combined_recent(&mut self, limit: usize) -> Vec<DebugEvent> {
        // Merge in-memory events with file-sourced events so callers see a
        // single coherent timeline regardless of which source an event came
        // from (spec section 7).
        let mut merged = self.events.iter().cloned().collect::<Vec<_>>();
        if let Ok(raw) = fs::read_to_string(&self.path) {
            for line in raw.lines() {
                if let Ok(item) = serde_json::from_str::<DebugEvent>(line) {
                    merged.push(item);
                }
            }
        }

        // De-duplicate identical events that appear in both the in-memory ring
        // buffer and the on-disk log. Events are keyed on their full content so
        // distinct events sharing a timestamp are preserved.
        let mut seen = std::collections::HashSet::new();
        merged.retain(|event| {
            seen.insert((
                event.ts.clone(),
                event.level.clone(),
                event.source.clone(),
                event.message.clone(),
                event.payload.to_string(),
            ))
        });

        // Sort chronologically (oldest -> newest) by timestamp. RFC 3339
        // timestamps sort correctly lexicographically for a fixed offset.
        merged.sort_by(|a, b| a.ts.cmp(&b.ts));

        // Keep only the most recent `limit` events, still in oldest -> newest
        // order.
        if merged.len() > limit {
            let start = merged.len() - limit;
            merged.drain(0..start);
        }
        merged
    }
}
