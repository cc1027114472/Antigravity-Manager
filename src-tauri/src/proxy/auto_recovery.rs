use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Get backoff delay for an auto-recovery probe attempt.
///
/// Ladder:
/// - Attempt 1: 1 minute (60s)
/// - Attempt 2: 15 minutes (900s)
/// - Attempt 3: 1 hour (3600s)
/// - Attempt 4: 4 hours (14400s)
/// - Attempt >= 5: 4 hours (capped)
pub fn get_backoff_delay(attempt: u8) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(60),
        2 => Duration::from_secs(15 * 60),
        3 => Duration::from_secs(60 * 60),
        4 => Duration::from_secs(4 * 60 * 60),
        _ => Duration::from_secs(4 * 60 * 60),
    }
}

/// Recovery task representing an account undergoing exponential backoff auto-recovery probing.
#[derive(Debug, Clone)]
pub struct RecoveryTask {
    pub account_id: String,
    pub email: String,
    pub attempt: u8,
    pub next_probe_at: Instant,
    pub initial_reason: String,
}

/// Scheduler for tracking and driving 429 auto-recovery tasks.
#[derive(Debug, Clone)]
pub struct AutoRecoveryScheduler {
    pub tasks: Arc<DashMap<String, RecoveryTask>>,
    pub data_dir: PathBuf,
}

impl AutoRecoveryScheduler {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
            data_dir,
        }
    }

    /// Enqueue a recovery task for an account.
    ///
    /// Inserts if not exists (or updates if new reason), initial attempt = 1,
    /// next_probe_at = Instant::now() + get_backoff_delay(1).
    pub fn enqueue_task(&self, account_id: &str, email: &str, initial_reason: &str) {
        match self.tasks.entry(account_id.to_string()) {
            dashmap::Entry::Occupied(mut occ) => {
                let task = occ.get_mut();
                if task.initial_reason != initial_reason {
                    task.initial_reason = initial_reason.to_string();
                }
                if task.email != email {
                    task.email = email.to_string();
                }
            }
            dashmap::Entry::Vacant(vac) => {
                vac.insert(RecoveryTask {
                    account_id: account_id.to_string(),
                    email: email.to_string(),
                    attempt: 1,
                    next_probe_at: Instant::now() + get_backoff_delay(1),
                    initial_reason: initial_reason.to_string(),
                });
            }
        }
    }

    /// Remove a recovery task by account_id.
    pub fn remove_task(&self, account_id: &str) -> Option<RecoveryTask> {
        self.tasks.remove(account_id).map(|(_, task)| task)
    }

    /// Get a cloned recovery task by account_id.
    pub fn get_task(&self, account_id: &str) -> Option<RecoveryTask> {
        self.tasks.get(account_id).map(|r| r.clone())
    }

    /// Return all tasks whose scheduled probe time has arrived (Instant::now() >= next_probe_at).
    pub fn get_due_tasks(&self) -> Vec<RecoveryTask> {
        let now = Instant::now();
        self.tasks
            .iter()
            .filter(|entry| now >= entry.value().next_probe_at)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Advance a task to the next attempt:
    /// Increments attempt; if attempt > 4, removes the task and returns None;
    /// else updates next_probe_at and returns Some(updated_task).
    pub fn advance_task(&self, account_id: &str) -> Option<RecoveryTask> {
        match self.tasks.entry(account_id.to_string()) {
            dashmap::Entry::Occupied(mut occ) => {
                let task = occ.get_mut();
                task.attempt += 1;
                if task.attempt > 4 {
                    occ.remove();
                    None
                } else {
                    task.next_probe_at = Instant::now() + get_backoff_delay(task.attempt);
                    Some(task.clone())
                }
            }
            dashmap::Entry::Vacant(_) => None,
        }
    }

    /// Returns the number of active recovery tasks.
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }
}
