//! Job loop — the scheduler as the daemon job loop (D-N051 F8, pas 5,
//! D4d, D7).
//!
//! Upstream (`agent0ai/agent-zero`, gel `ba9b67d`):
//! `plugins/_scheduler` is a cron inside the engine
//! (`crontab==1.0.1` in requirements.txt, scheduled / adhoc / planned
//! tasks via `SchedulerTool`; the engine job loop starts in
//! `initialize.initialize_job_loop` -> `helpers/job_loop.run_loop`).
//!
//! Native rule (D7): the scheduler BECOMES the job loop of the
//! single daemon. The `crontab` dependency is abandoned — the job
//! loop IS the scheduler, not a cron. Proof (H-P5-6): a scheduled
//! task executes and proves itself by receipt (the execution is
//! recorded in the ledger). No cron is ever resurrected under another
//! name (F8 D8, H-F8-5).
//!
//! The refusal is structural: `TaskSpec` denies unknown fields, so a
//! task definition carrying `cron` fails to deserialize. There is no
//! cron field anywhere in this module.
//!
//! This module is the native mirror of the adapter's `job_loop.py`:
//! same contract, same function names, Rust types.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::BoxError;

/// Task kind — scheduled / adhoc / planned (upstream `SchedulerTool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Scheduled,
    Adhoc,
    Planned,
}

/// A task definition as read from JSON. `deny_unknown_fields` is the
/// cron refusal: a spec carrying `cron` fails to deserialize (D7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub kind: TaskKind,
    pub name: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub run_at: Option<u64>,
    #[serde(default = "default_handler")]
    pub handler: String,
}

fn default_handler() -> String {
    "echo".to_owned()
}

/// One task, live in the loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub kind: TaskKind,
    pub name: String,
    pub payload: serde_json::Value,
    pub run_at: Option<u64>,
    pub handler: String,
    pub created_at: u64,
    pub status: String, // pending | done | deleted
}

/// One execution receipt, written to the ledger (H-P5-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub receipt_id: String,
    pub task_id: String,
    pub name: String,
    pub kind: TaskKind,
    pub handler: String,
    pub executed_at: u64,
    pub result: String,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One job handler: (payload) -> result text.
pub type JobHandler = Arc<dyn Fn(&serde_json::Value) -> String + Send + Sync>;

fn new_id() -> String {
    // ULID-style uniqueness without a dependency: epoch millis + pid + counter.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{:x}-{}-{}", now_secs(), std::process::id(), n)
}

/// The daemon job loop. Tasks execute; executions prove by receipt.
pub struct JobLoop {
    ledger_path: PathBuf,
    handlers: HashMap<String, JobHandler>,
    tasks: HashMap<String, Task>,
}

impl JobLoop {
    pub fn new(ledger_path: impl AsRef<Path>) -> Self {
        let ledger_path = ledger_path.as_ref().to_path_buf();
        if let Some(parent) = ledger_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut handlers: HashMap<String, JobHandler> = HashMap::new();
        handlers.insert(
            "echo".to_owned(),
            Arc::new(|payload| serde_json::to_string(payload).unwrap_or_default()),
        );
        Self {
            ledger_path,
            handlers,
            tasks: HashMap::new(),
        }
    }

    pub fn register_handler(&mut self, name: &str, handler: JobHandler) {
        self.handlers.insert(name.to_owned(), handler);
    }

    /// Create a task from a spec. A spec carrying `cron` never
    /// reaches this function: `TaskSpec` denies unknown fields.
    pub fn create_task(&mut self, spec: TaskSpec) -> Result<Task, BoxError> {
        if !self.handlers.contains_key(&spec.handler) {
            return Err(format!("unknown handler '{}'", spec.handler).into());
        }
        let task = Task {
            task_id: new_id(),
            kind: spec.kind,
            name: spec.name,
            payload: spec.payload,
            run_at: spec.run_at,
            handler: spec.handler,
            created_at: now_secs(),
            status: "pending".to_owned(),
        };
        self.tasks.insert(task.task_id.clone(), task.clone());
        Ok(task)
    }

    pub fn list_tasks(&self, kind: Option<TaskKind>) -> Vec<Task> {
        let mut tasks: Vec<Task> = self
            .tasks
            .values()
            .filter(|t| t.status != "deleted")
            .filter(|t| kind.is_none_or(|k| t.kind == k))
            .cloned()
            .collect();
        tasks.sort_by_key(|t| t.created_at);
        tasks
    }

    pub fn get_task(&self, task_id: &str) -> Option<&Task> {
        self.tasks.get(task_id)
    }

    pub fn delete_task(&mut self, task_id: &str) -> bool {
        match self.tasks.get_mut(task_id) {
            Some(task) => {
                task.status = "deleted".to_owned();
                true
            }
            None => false,
        }
    }

    /// Execute one task and write its receipt to the ledger (H-P5-6).
    pub fn run_task(&mut self, task_id: &str, now: u64) -> Result<Receipt, BoxError> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("unknown task '{task_id}'"))?;
        if task.status == "deleted" {
            return Err(format!("task '{task_id}' is deleted").into());
        }
        let handler = self
            .handlers
            .get(&task.handler)
            .ok_or_else(|| format!("unknown handler '{}'", task.handler))?;
        let result = handler(&task.payload);
        task.status = "done".to_owned();
        let receipt = Receipt {
            receipt_id: new_id(),
            task_id: task.task_id.clone(),
            name: task.name.clone(),
            kind: task.kind,
            handler: task.handler.clone(),
            executed_at: now,
            result,
        };
        let line = serde_json::to_string(&receipt)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.ledger_path)?;
        use std::io::Write;
        writeln!(file, "{line}")?;
        Ok(receipt)
    }

    /// Execute every pending task whose `run_at` has passed (adhoc: now).
    pub fn run_due(&mut self, now: u64) -> Result<Vec<Receipt>, BoxError> {
        let due: Vec<String> = self
            .list_tasks(None)
            .into_iter()
            .filter(|t| t.status == "pending")
            .filter(|t| t.run_at.is_none_or(|at| at <= now))
            .map(|t| t.task_id)
            .collect();
        let mut receipts = Vec::new();
        for task_id in due {
            receipts.push(self.run_task(&task_id, now)?);
        }
        Ok(receipts)
    }

    pub fn read_receipts(&self) -> Result<Vec<Receipt>, BoxError> {
        let text = match std::fs::read_to_string(&self.ledger_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut receipts = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                receipts.push(serde_json::from_str::<Receipt>(line)?);
            }
        }
        Ok(receipts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_loop() -> JobLoop {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("riginfinity-job-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        JobLoop::new(dir.join("ledger.jsonl"))
    }

    fn spec(kind: TaskKind, name: &str, run_at: Option<u64>) -> TaskSpec {
        TaskSpec {
            kind,
            name: name.to_owned(),
            payload: serde_json::json!({"check": "presence"}),
            run_at,
            handler: "echo".to_owned(),
        }
    }

    #[test]
    fn scheduled_task_runs_with_receipt() {
        let mut job = tmp_loop();
        let task = job
            .create_task(spec(TaskKind::Scheduled, "nightly check", Some(1000)))
            .expect("create task");
        assert_eq!(task.status, "pending");
        let receipts = job.run_due(1000).expect("run due tasks");
        assert_eq!(receipts.len(), 1);
        let receipt = &receipts[0];
        assert_eq!(receipt.task_id, task.task_id);
        assert_eq!(receipt.name, "nightly check");
        assert_eq!(receipt.kind, TaskKind::Scheduled);
        assert_eq!(receipt.executed_at, 1000);
        assert!(receipt.result.contains("presence"));
        assert_eq!(
            job.read_receipts().expect("read receipts")[0].receipt_id,
            receipt.receipt_id
        );
        assert_eq!(
            job.get_task(&task.task_id).expect("task exists").status,
            "done"
        );
    }

    #[test]
    fn adhoc_task_runs_now_and_future_task_waits() {
        let mut job = tmp_loop();
        let now_task = job
            .create_task(spec(TaskKind::Adhoc, "now", None))
            .expect("create task");
        job.create_task(spec(TaskKind::Planned, "later", Some(5000)))
            .expect("create task");
        let receipts = job.run_due(100).expect("run due tasks");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].task_id, now_task.task_id);
    }

    #[test]
    fn cron_is_refused_by_the_spec() {
        // A task definition carrying `cron` fails to deserialize:
        // there is no cron field anywhere in this module (D7).
        let raw = r#"{"kind": "scheduled", "name": "nope", "cron": "*/30 * * * *"}"#;
        let err = serde_json::from_str::<TaskSpec>(raw).expect_err("cron spec must fail");
        assert!(err.to_string().contains("unknown field `cron`"));
    }

    #[test]
    fn unknown_handler_is_refused() {
        let mut job = tmp_loop();
        let mut bad = spec(TaskKind::Adhoc, "x", None);
        bad.handler = "nope".to_owned();
        assert!(job.create_task(bad).is_err());
    }

    #[test]
    fn custom_handler_executes() {
        let mut job = tmp_loop();
        job.register_handler(
            "double",
            Arc::new(|payload| {
                payload["n"]
                    .as_u64()
                    .map(|n| (n * 2).to_string())
                    .unwrap_or_default()
            }),
        );
        let mut s = spec(TaskKind::Adhoc, "d", None);
        s.payload = serde_json::json!({"n": 21});
        s.handler = "double".to_owned();
        let task = job.create_task(s).expect("create task");
        let receipt = job.run_task(&task.task_id, 7).expect("run task");
        assert_eq!(receipt.result, "42");
    }

    #[test]
    fn delete_and_list() {
        let mut job = tmp_loop();
        let keep = job
            .create_task(spec(TaskKind::Adhoc, "keep", None))
            .expect("create task");
        let drop = job
            .create_task(spec(TaskKind::Adhoc, "drop", None))
            .expect("create task");
        assert!(job.delete_task(&drop.task_id));
        assert_eq!(
            job.list_tasks(None)
                .iter()
                .map(|t| t.task_id.clone())
                .collect::<Vec<_>>(),
            vec![keep.task_id]
        );
        assert!(job.list_tasks(Some(TaskKind::Scheduled)).is_empty());
    }
}
