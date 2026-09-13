use std::time::{Duration, Instant};
use crate::proxy::auto_recovery::{get_backoff_delay, AutoRecoveryScheduler};

#[test]
fn test_backoff_ladder_intervals() {
    assert_eq!(get_backoff_delay(0), Duration::from_secs(60));
    assert_eq!(get_backoff_delay(1), Duration::from_secs(60));
    assert_eq!(get_backoff_delay(2), Duration::from_secs(15 * 60));
    assert_eq!(get_backoff_delay(3), Duration::from_secs(60 * 60));
    assert_eq!(get_backoff_delay(4), Duration::from_secs(4 * 60 * 60));
    // 超过 4 阶不再延长
    assert_eq!(get_backoff_delay(5), Duration::from_secs(4 * 60 * 60));
    assert_eq!(get_backoff_delay(6), Duration::from_secs(4 * 60 * 60));
    assert_eq!(get_backoff_delay(255), Duration::from_secs(4 * 60 * 60));
}

#[test]
fn test_auto_recovery_task_lifecycle() {
    let temp_dir = std::env::temp_dir().join("test_auto_recovery_lifecycle");
    let scheduler = AutoRecoveryScheduler::new(temp_dir);

    // Initial state
    assert_eq!(scheduler.task_count(), 0);
    assert!(scheduler.get_task("acc_1").is_none());

    // 1. Enqueue task
    scheduler.enqueue_task("acc_1", "acc1@example.com", "HTTP 429 Too Many Requests");
    assert_eq!(scheduler.task_count(), 1);

    let task = scheduler.get_task("acc_1").expect("task should exist");
    assert_eq!(task.account_id, "acc_1");
    assert_eq!(task.email, "acc1@example.com");
    assert_eq!(task.attempt, 1);
    assert_eq!(task.initial_reason, "HTTP 429 Too Many Requests");

    // 2. Deduplication: re-enqueuing the same account updates reason/email without duplicating
    scheduler.enqueue_task("acc_1", "acc1@example.com", "Rate limited by upstream");
    assert_eq!(scheduler.task_count(), 1);
    let task_dup = scheduler.get_task("acc_1").expect("task should exist");
    assert_eq!(task_dup.attempt, 1);
    assert_eq!(task_dup.initial_reason, "Rate limited by upstream");

    // 3. Advance to attempt 2
    let adv1 = scheduler.advance_task("acc_1").expect("should advance to attempt 2");
    assert_eq!(adv1.attempt, 2);
    assert_eq!(scheduler.task_count(), 1);

    // 4. Advance to attempt 3
    let adv2 = scheduler.advance_task("acc_1").expect("should advance to attempt 3");
    assert_eq!(adv2.attempt, 3);
    assert_eq!(scheduler.task_count(), 1);

    // 5. Advance to attempt 4
    let adv3 = scheduler.advance_task("acc_1").expect("should advance to attempt 4");
    assert_eq!(adv3.attempt, 4);
    assert_eq!(scheduler.task_count(), 1);

    // 6. Expiration after 4 attempts: 5th advance removes task and returns None
    let adv4 = scheduler.advance_task("acc_1");
    assert!(adv4.is_none());
    assert_eq!(scheduler.task_count(), 0);
    assert!(scheduler.get_task("acc_1").is_none());

    // Advance non-existent task returns None
    assert!(scheduler.advance_task("acc_1").is_none());

    // 7. Remove task test
    scheduler.enqueue_task("acc_2", "acc2@example.com", "429");
    assert_eq!(scheduler.task_count(), 1);
    let removed = scheduler.remove_task("acc_2").expect("should remove task");
    assert_eq!(removed.account_id, "acc_2");
    assert_eq!(scheduler.task_count(), 0);
    assert!(scheduler.remove_task("acc_2").is_none());
}

#[test]
fn test_get_due_tasks() {
    let temp_dir = std::env::temp_dir().join("test_auto_recovery_due");
    let scheduler = AutoRecoveryScheduler::new(temp_dir);

    // Task 1: future (not due)
    scheduler.enqueue_task("acc_future", "future@example.com", "429");

    // Task 2: past (due)
    scheduler.enqueue_task("acc_due", "due@example.com", "429");
    // Manually backdate next_probe_at to the past
    if let Some(mut task) = scheduler.tasks.get_mut("acc_due") {
        task.next_probe_at = Instant::now() - Duration::from_secs(10);
    }

    let due_tasks = scheduler.get_due_tasks();
    assert_eq!(due_tasks.len(), 1);
    assert_eq!(due_tasks[0].account_id, "acc_due");
}

#[test]
fn test_build_probe_payload() {
    let payload = AutoRecoveryScheduler::build_probe_payload("test-project-123");

    // Verify root fields
    assert_eq!(
        payload.get("project").and_then(|v| v.as_str()),
        Some("test-project-123")
    );
    assert_eq!(
        payload.get("model").and_then(|v| v.as_str()),
        Some("gemini-2.5-flash")
    );

    // Verify inner request fields
    let req = payload.get("request").expect("payload must have request field");

    // Contents structure
    let contents = req
        .get("contents")
        .and_then(|v| v.as_array())
        .expect("request must have contents array");
    assert_eq!(contents.len(), 1);
    assert_eq!(
        contents[0].get("role").and_then(|v| v.as_str()),
        Some("user")
    );
    let parts = contents[0]
        .get("parts")
        .and_then(|v| v.as_array())
        .expect("contents must have parts array");
    assert_eq!(parts[0].get("text").and_then(|v| v.as_str()), Some("ping"));

    // Generation config: maxOutputTokens: 1, temperature: 0
    let gen_config = req
        .get("generationConfig")
        .expect("request must have generationConfig");
    assert_eq!(
        gen_config.get("maxOutputTokens").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert_eq!(
        gen_config.get("temperature").and_then(|v| v.as_i64()),
        Some(0)
    );

    // Session ID starts with "probe_"
    let session_id = req
        .get("session_id")
        .or_else(|| req.get("sessionId"))
        .and_then(|v| v.as_str())
        .expect("request must have session_id or sessionId");
    assert!(
        session_id.starts_with("probe_"),
        "session_id should start with probe_, got: {}",
        session_id
    );
}

#[tokio::test]
async fn test_probe_account_uninitialized_dependencies() {
    let temp_dir = std::env::temp_dir().join("test_auto_recovery_uninit");
    let scheduler = AutoRecoveryScheduler::new(temp_dir);

    // Dependencies are not initialized, should return Err("Dependencies not initialized")
    let result = scheduler.probe_account("acc_test", "test@example.com").await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "Dependencies not initialized");
}

#[tokio::test]
async fn test_recover_account_removes_task() {
    let temp_dir = std::env::temp_dir().join("test_auto_recovery_recover");
    let scheduler = AutoRecoveryScheduler::new(temp_dir);

    scheduler.enqueue_task("acc_test_rec", "rec@example.com", "HTTP 429 Too Many Requests");
    assert_eq!(scheduler.task_count(), 1);
    assert!(scheduler.get_task("acc_test_rec").is_some());

    // Calling recover_account removes the task from the scheduler
    let _ = scheduler.recover_account("acc_test_rec", "rec@example.com", 1).await;

    assert_eq!(scheduler.task_count(), 0);
    assert!(scheduler.get_task("acc_test_rec").is_none());
}

#[tokio::test]
async fn test_start_loop_cancels_cleanly() {
    let temp_dir = std::env::temp_dir().join("test_auto_recovery_loop");
    let scheduler = std::sync::Arc::new(AutoRecoveryScheduler::new(temp_dir));
    let cancel_token = tokio_util::sync::CancellationToken::new();

    let handle = scheduler.start_loop(cancel_token.clone());
    // Cancel immediately
    cancel_token.cancel();

    // Await handle with a timeout to verify it terminates cleanly
    let join_res = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(join_res.is_ok(), "Loop task should terminate after cancellation");
    assert!(join_res.unwrap().is_ok(), "Task join should succeed");
}
