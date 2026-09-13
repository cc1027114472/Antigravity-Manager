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
