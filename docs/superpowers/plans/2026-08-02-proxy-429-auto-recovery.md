# Proxy 429 自动探测与指数退避自愈实施计划 (Proxy 429 Auto-Recovery Implementation Plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现针对 HTTP 429 的极简探针校验 (Step 0) 与四阶指数退避自动探测自愈调度器 (AutoRecoveryScheduler)，在兼顾零额外配额损耗与抗重启的前提下，自动恢复被误伤或配额已重置的账号。

**Architecture:** 创建独立的 `AutoRecoveryScheduler` 模块，通过单例去重任务队列维护退避状态机（0s $\rightarrow$ 1m $\rightarrow$ 15m $\rightarrow$ 1h $\rightarrow$ 4h）；使用官方轻量通道以 `gemini-2.5-flash` 进行低成本快速探活；成功后原子翻转账号持久化状态并联动内存池与前端广播；并在启动时对历史 429 禁用账号执行冷启动纳管扫描。

**Tech Stack:** Rust (Tokio, DashMap, Axum, rquest), Serde JSON, Tauri Event Bridge.

---

## 文件结构与变动规划 (File Structure Plan)

* **新增文件**:
  * `src-tauri/src/proxy/auto_recovery.rs`: 核心自愈调度器模块，包含 `RecoveryTask`、`AutoRecoveryScheduler`、探针请求构造、四阶退避循环与自愈恢复联动。
  * `src-tauri/src/proxy/tests/auto_recovery_tests.rs`: 针对自愈状态机、阶梯递增、Step 0 拦截与抗重启扫描的专用测试套件。
* **修改文件**:
  * `src-tauri/src/proxy/mod.rs`: 导出 `pub mod auto_recovery;`。
  * `src-tauri/src/proxy/token_manager.rs`:
    * 在 `TokenManager` 中注入/关联 `AutoRecoveryScheduler` 引用；
    * 改造 `disable_proxy_on_429` / `mark_rate_limited_async`，插入 Step 0 即时探测与退避入队逻辑。
  * `src-tauri/src/proxy/server.rs`:
    * 在 `AppState` 中初始化并持有 `AutoRecoveryScheduler`；
    * 启动后台轮询循环并在服务关闭时取消；
    * 在启动阶段调用冷启动扫描纳管。
  * `src-tauri/src/proxy/tests/mod.rs`: 注册 `pub mod auto_recovery_tests;`。

---

## 详细任务步骤 (Tasks)

### Task 1: 创建自愈调度器核心数据结构与退避状态机

**Files:**
- Create: `src-tauri/src/proxy/auto_recovery.rs`
- Modify: `src-tauri/src/proxy/mod.rs`
- Test: `src-tauri/src/proxy/tests/auto_recovery_tests.rs`

- [x] **Step 1: 编写测试用例验证任务去重与退避阶梯计算**

```rust
// src-tauri/src/proxy/tests/auto_recovery_tests.rs
#[cfg(test)]
mod tests {
    use std::time::Duration;
    use crate::proxy::auto_recovery::get_backoff_delay;

    #[test]
    fn test_backoff_ladder_intervals() {
        assert_eq!(get_backoff_delay(1), Duration::from_secs(60));
        assert_eq!(get_backoff_delay(2), Duration::from_secs(15 * 60));
        assert_eq!(get_backoff_delay(3), Duration::from_secs(60 * 60));
        assert_eq!(get_backoff_delay(4), Duration::from_secs(4 * 60 * 60));
        // 超过 4 阶不再延长
        assert_eq!(get_backoff_delay(5), Duration::from_secs(4 * 60 * 60));
    }
}
```

- [x] **Step 2: 运行测试确保编译失败（缺少定义）**

Run: `cargo test --package antigravity-manager --lib proxy::tests::auto_recovery_tests`
Expected: FAIL due to unresolved import `auto_recovery`

- [x] **Step 3: 实现 `auto_recovery.rs` 的核心数据结构**

在 `src-tauri/src/proxy/auto_recovery.rs` 中定义：
* `get_backoff_delay(attempt: u8) -> std::time::Duration`
* `RecoveryTask` 结构体（`account_id`, `email`, `attempt`, `next_probe_at`, `initial_reason`）
* `AutoRecoveryScheduler` 基础实现：
  * `new(...)`
  * `enqueue_task(&self, account_id: &str, email: &str, initial_reason: &str)`
  * `remove_task(&self, account_id: &str)`
  * `get_due_tasks(&self) -> Vec<RecoveryTask>`
* 在 `src-tauri/src/proxy/mod.rs` 中添加 `pub mod auto_recovery;`
* 在 `src-tauri/src/proxy/tests/mod.rs` 中添加 `pub mod auto_recovery_tests;`

- [x] **Step 4: 运行测试验证基础阶梯与队列**

Run: `cargo test --package antigravity-manager --lib proxy::tests::auto_recovery_tests`
Expected: PASS

- [x] **Step 5: 提交代码**

```bash
git add src-tauri/src/proxy/auto_recovery.rs src-tauri/src/proxy/mod.rs src-tauri/src/proxy/tests/auto_recovery_tests.rs src-tauri/src/proxy/tests/mod.rs
git commit -m "feat(proxy): add auto-recovery data structures and backoff ladder"
```

---

### Task 2: 实现轻量探针构建与探测请求逻辑

**Files:**
- Modify: `src-tauri/src/proxy/auto_recovery.rs`
- Test: `src-tauri/src/proxy/tests/auto_recovery_tests.rs`

- [x] **Step 1: 编写探针构建与校验的测试用例**

在 `src-tauri/src/proxy/tests/auto_recovery_tests.rs` 中添加针对探针 payload 结构与超时的验证。

- [x] **Step 2: 实现探针执行方法 `probe_account`**

在 `AutoRecoveryScheduler` 中实现异步方法：
```rust
pub async fn probe_account(
    &self,
    account_id: &str,
    email: &str,
) -> Result<bool, String> {
    // 1. 从 TokenManager 获取当前账号的有效 token 与 project_id
    // 2. 构造轻量标准 gemini-2.5-flash 请求体 (maxOutputTokens: 1, text: "ping")
    // 3. 通过 UpstreamClient::call_v1_internal 发送，设置 5 秒超时，走账号绑定代理出口
    // 4. 收到 200 返回 Ok(true)；收到 429/403/超时 返回 Ok(false) 或 Err
}
```

- [x] **Step 3: 运行测试验证**

Run: `cargo test --package antigravity-manager --lib proxy::tests::auto_recovery_tests`
Expected: PASS

- [x] **Step 4: 提交代码**

```bash
git add src-tauri/src/proxy/auto_recovery.rs src-tauri/src/proxy/tests/auto_recovery_tests.rs
git commit -m "feat(proxy): implement gemini-2.5-flash probe request in auto_recovery"
```

---

### Task 3: 实现自愈复活联动与后台轮询 Loop

**Files:**
- Modify: `src-tauri/src/proxy/auto_recovery.rs`
- Test: `src-tauri/src/proxy/tests/auto_recovery_tests.rs`

- [x] **Step 1: 编写状态翻转与自愈联动的集成测试**

测试自愈成功后账号在文件和内存中的状态：
* `account.proxy_disabled == false`
* `proxy_disabled_reason == None`
* `RateLimitTracker` 中无残留限流记录
* 任务从 `AutoRecoveryScheduler` 中移除。

- [x] **Step 2: 实现 `recover_account` 与轮询 `start_loop`**

在 `AutoRecoveryScheduler` 中实现：
* `recover_account(&self, account_id: &str, email: &str, attempt: u8) -> Result<(), String>`:
  1. 调用 `crate::modules::account::toggle_proxy_status(account_id, true, None)` 写盘；
  2. 调用 `token_manager.clear_rate_limit(account_id)`；
  3. 调用 `token_manager.reload_account(account_id)` 重新加入内存调度池；
  4. 广播前端事件 `crate::modules::log_bridge::emit_accounts_refreshed()`；
  5. 打印醒目的成功自愈日志。
* `start_loop(&self, cancel_token: CancellationToken)`:
  * 每 10 秒唤醒一次；
  * 执行 `get_due_tasks`；
  * 并行探测到期账号；
  * 成功调 `recover_account`，失败按阶梯累加 `attempt`，满 4 次则移除。

- [x] **Step 3: 运行测试验证**

Run: `cargo test --package antigravity-manager --lib proxy::tests::auto_recovery_tests`
Expected: PASS

- [x] **Step 4: 提交代码**

```bash
git add src-tauri/src/proxy/auto_recovery.rs src-tauri/src/proxy/tests/auto_recovery_tests.rs
git commit -m "feat(proxy): implement recovery actions and backoff loop"
```

---

### Task 4: 拦截 429 并接入 Step 0 即时校验与入队

**Files:**
- Modify: `src-tauri/src/proxy/token_manager.rs`
- Modify: `src-tauri/src/proxy/auto_recovery.rs`
- Test: `src-tauri/src/proxy/tests/auto_recovery_tests.rs`

- [x] **Step 1: 编写 Step 0 即时探测成功与失败的测试**

验证：
* Step 0 探测成功时：不落盘禁用，直接清除限流标记；
* Step 0 探测失败时：落盘禁用，并以 Step 1 (1分钟后) 入队调度器。

- [x] **Step 2: 修改 `disable_proxy_on_429` 接入 Step 0 与调度器**

在 `TokenManager` 中：
* 注入 `auto_recovery: Arc<tokio::sync::RwLock<Option<Arc<AutoRecoveryScheduler>>>>`；
* 在 `disable_proxy_on_429` 前，若调度器就绪，调用 `scheduler.probe_step_zero(account_id, email).await`：
  * 若成功：取消本次禁用，记录日志并返回；
  * 若失败：执行写盘 `toggle_proxy_status(account_id, false, ...)`，并调用 `scheduler.enqueue_task(...)`。

- [x] **Step 3: 运行单元测试**

Run: `cargo test --package antigravity-manager --lib proxy::tests::auto_recovery_tests`
Expected: PASS

- [x] **Step 4: 提交代码**

```bash
git add src-tauri/src/proxy/token_manager.rs src-tauri/src/proxy/auto_recovery.rs src-tauri/src/proxy/tests/auto_recovery_tests.rs
git commit -m "feat(proxy): hook Step 0 instant probe and backoff enqueue into disable_proxy_on_429"
```

---

### Task 5: 接入冷启动扫描与应用生命周期

**Files:**
- Modify: `src-tauri/src/proxy/server.rs`
- Modify: `src-tauri/src/proxy/auto_recovery.rs`
- Modify: `src-tauri/src/commands/proxy.rs`

- [x] **Step 1: 实现启动扫描纳管 `scan_and_enqueue_disabled`**

在 `AutoRecoveryScheduler` 中实现：
* 扫描 `data_dir/accounts/*.json`；
* 检查 `proxy_disabled == true` 且 `proxy_disabled_reason` 包含 429 / Quota / RateLimit（且不含 `manual` 与 `Forbidden`）；
* 自动以 `attempt = 1, next_probe_at = now + 60s` 注册进队列；
* 日志记录纳管账号总数。

- [x] **Step 2: 在 `server.rs` 与 `AppState` 中挂载并启动后台任务**

* 在 `AppState` 中新增 `pub auto_recovery: Arc<AutoRecoveryScheduler>`；
* 服务启动时：
  1. 调用 `auto_recovery.scan_and_enqueue_disabled()`；
  2. 调用 `auto_recovery.start_loop(cancel_token)` 启动轮询后台任务；
  3. 将调度器注入 `token_manager`。
* 服务停止时：通过 `cancel_token` 安全取消后台协程。

- [x] **Step 3: 运行全量 proxy 测试**

Run: `cargo test --package antigravity-manager --lib proxy::tests`
Expected: All tests PASS

- [x] **Step 4: 提交代码**

```bash
git add src-tauri/src/proxy/server.rs src-tauri/src/proxy/auto_recovery.rs src-tauri/src/commands/proxy.rs
git commit -m "feat(proxy): bind auto-recovery scheduler to server lifecycle and boot scan"
```

---

### Task 6: 完整端到端验证与构建回归

**Files:**
- Verify: 全局编译与全量测试

- [x] **Step 1: 执行 cargo check 验证无警告与类型错误**

Run: `cargo check`
Expected: Finished dev profile, no errors

- [x] **Step 2: 执行全量自动化测试**

Run: `cargo test --package antigravity-manager --lib`
Expected: All tests PASS

- [x] **Step 3: 提交最终文档与代码**

```bash
git add docs/superpowers/plans/2026-08-02-proxy-429-auto-recovery.md
git commit -m "docs: finalize proxy 429 auto-recovery implementation plan"
```
