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

/// Build standard lightweight probe payload targeting gemini-2.5-flash.
pub fn build_probe_payload(project_id: &str) -> serde_json::Value {
    let session_id = format!(
        "probe_{}_{}",
        chrono::Utc::now().timestamp_millis(),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let base_request = serde_json::json!({
        "model": "gemini-2.5-flash",
        "contents": [{
            "role": "user",
            "parts": [{
                "text": "ping"
            }]
        }],
        "generationConfig": {
            "maxOutputTokens": 1,
            "temperature": 0
        },
        "session_id": session_id
    });
    crate::proxy::mappers::gemini::wrapper::wrap_request(
        &base_request,
        project_id,
        "gemini-2.5-flash",
        None,
        Some(&session_id),
        None,
    )
}

/// Scheduler for tracking and driving 429 auto-recovery tasks.
#[derive(Clone)]
pub struct AutoRecoveryScheduler {
    pub tasks: Arc<DashMap<String, RecoveryTask>>,
    pub data_dir: PathBuf,
    pub token_manager: Arc<tokio::sync::RwLock<Option<Arc<crate::proxy::TokenManager>>>>,
    pub upstream: Arc<tokio::sync::RwLock<Option<Arc<crate::proxy::upstream::client::UpstreamClient>>>>,
}

impl std::fmt::Debug for AutoRecoveryScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoRecoveryScheduler")
            .field("tasks", &self.tasks)
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

impl AutoRecoveryScheduler {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
            data_dir,
            token_manager: Arc::new(tokio::sync::RwLock::new(None)),
            upstream: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Set shared runtime dependencies (TokenManager and UpstreamClient).
    pub async fn set_dependencies(
        &self,
        token_manager: Arc<crate::proxy::TokenManager>,
        upstream: Arc<crate::proxy::upstream::client::UpstreamClient>,
    ) {
        let mut tm = self.token_manager.write().await;
        *tm = Some(token_manager);
        let mut up = self.upstream.write().await;
        *up = Some(upstream);
    }

    /// Build standard lightweight probe payload targeting gemini-2.5-flash.
    pub fn build_probe_payload(project_id: &str) -> serde_json::Value {
        build_probe_payload(project_id)
    }

    /// Probe an account to determine if its 429/quota block has cleared.
    pub async fn probe_account(&self, account_id: &str, email: &str) -> Result<bool, String> {
        let (token_mgr, upstream) = {
            let tm_guard = self.token_manager.read().await;
            let up_guard = self.upstream.read().await;
            match (tm_guard.as_ref(), up_guard.as_ref()) {
                (Some(tm), Some(up)) => (Arc::clone(tm), Arc::clone(up)),
                _ => return Err("Dependencies not initialized".to_string()),
            }
        };

        // 1. Get token & project_id via token_mgr.get_token_by_email(email)
        let (access_token, mut project_id, _, _, _) = match token_mgr.get_token_by_email(email).await {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(
                    "[AutoRecovery] Failed to get token for account {} ({}): {}",
                    account_id,
                    email,
                    e
                );
                return Ok(false);
            }
        };

        if project_id.is_empty() {
            project_id = "bamboo-precept-lgxtn".to_string();
        }

        // 2. Build lightweight probe payload
        let probe_payload = Self::build_probe_payload(&project_id);

        // 3. Send via upstream.call_v1_internal with 5-second timeout
        let call_res = tokio::time::timeout(
            Duration::from_secs(5),
            upstream.call_v1_internal("generateContent", &access_token, probe_payload, None, Some(account_id)),
        )
        .await;

        match call_res {
            Ok(Ok(upstream_res)) => {
                let status = upstream_res.response.status();
                if status.is_success() {
                    tracing::info!(
                        "[AutoRecovery] Probe succeeded for account {} ({}) with status {}",
                        account_id,
                        email,
                        status
                    );
                    Ok(true)
                } else {
                    tracing::info!(
                        "[AutoRecovery] Probe non-success for account {} ({}): status {}",
                        account_id,
                        email,
                        status
                    );
                    Ok(false)
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "[AutoRecovery] Probe call error for account {} ({}): {}",
                    account_id,
                    email,
                    e
                );
                Ok(false)
            }
            Err(_) => {
                tracing::warn!(
                    "[AutoRecovery] Probe timed out after 5s for account {} ({})",
                    account_id,
                    email
                );
                Ok(false)
            }
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
