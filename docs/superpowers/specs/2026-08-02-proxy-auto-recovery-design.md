# Proxy 429 自动探测与指数退避自愈规范设计 (Auto-Recovery Probe Design)

## 1. 概述与背景 (Overview & Background)

在 Antigravity Manager 的反代运行实践中，账号在遇到 Google 上游返回 HTTP 429（如 `RESOURCE_EXHAUSTED`、`quota_exhausted`、`RateLimitExceeded`）时，系统为了防止继续向故障账号派发请求，会调用 `disable_proxy_on_429` 将账号持久化置为 `proxy_disabled = true`，并移出内存调度池。

然而，在实际运行中有两大痛点：
1. **假 429 / 误伤**：客户端输错模型名（或发送了不合规参数）、短时间突发并发抖动等，可能引发上游误报 429。现行机制会直接一棒子将账号打入冷宫，必须人工在面板手动重新启用；
2. **缺乏自愈机制**：真正的 429 限流通常具有周期性（如 Google 4~5 小时的 Sprint 周期重置或每天零点重置）。现行机制即使上游配额已经恢复，账号依然处于 `proxy_disabled` 状态，无法自动复活。

本规范设计了一套轻量、可靠的**主动探针与指数退避自愈系统 (Auto-Recovery Probe System)**，在 429 发生时提供 Step 0 即时校验，并对真实 429 账号执行有限梯度的渐进探测与自动复活。

---

## 2. 核心设计原则 (Core Principles)

1. **零额外配额负担**：探针固定采用极轻量的基础模型（`gemini-2.5-flash`，`max_tokens=1`），消耗配额几乎为 0，避免探测过程损耗高阶模型额度；
2. **范围严格聚焦**：仅针对 **HTTP 429（限流/配额相关）** 触发自愈；对于 `invalid_grant`（凭据失效）和显式人工禁用（`manual`），绝不擅自探测；
3. **阶梯退避且有限终止**：提供 Step 0 即时自愈，失败后依次进入 1m $\rightarrow$ 15m $\rightarrow$ 1h $\rightarrow$ 4h 四阶探测，超过 4 阶后自动终止，防止对无配额账号无限轰炸；
4. **单实例无竞争**：通过内存任务队列实现原子去重，并发 429 报错不会导致重复发探针；
5. **抗重启冷启动恢复**：应用重启后，自动扫描处于 429 禁用状态的账号并重新纳管入自愈队列。

---

## 3. 探针规格与 Step 0 即时校验 (Probe Spec & Step 0)

### 3.1 探针请求构造 (Probe Payload)
* **目标模型**：`gemini-2.5-flash`
* **协议层**：使用 Google Cloud Code 底层标准 RPC 通道（与账号常规请求使用相同协议与 Header）；
* **网络出口**：复用该账号已绑定的代理出口（Proxy Pool 绑定或默认出口），保持 IP 环境一致；
* **内容规格**：
  ```json
  {
    "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
    "generationConfig": {
      "maxOutputTokens": 1,
      "temperature": 0
    }
  }
  ```
* **超时时间**：严格设定为 5 秒，快速失败，不阻塞任何业务请求。

### 3.2 拦截点与 Step 0 即时校验流程
* **拦截点**：`TokenManager::mark_rate_limited_async` 检测到 `status == 429` 且排除 Grace Retry 短重试窗口（`retryDelay <= 2s`）。
* **处理逻辑**：
  1. 系统在准备调用 `disable_proxy_on_429` 前，立即异步发起 Step 0 探测；
  2. **若 Step 0 成功 (HTTP 200)**：
     - 判定为假 429（输入模型错误或参数不匹配）；
     - 豁免禁用：**不写盘 `proxy_disabled = true`**，清空该账号在 `RateLimitTracker` 中的误报记录；
     - 本次客户端请求正常返回错误或轮换至下一账号，但该账号完好保留在池中；
  3. **若 Step 0 失败 (HTTP 429/5xx)**：
     - 确认为真实限流；
     - 正常执行原有的 `disable_proxy_on_429` 逻辑（写盘 `proxy_disabled = true`，移出内存池，串行池切号）；
     - **同步将账号注册至 `AutoRecoveryScheduler`**，准备后续退避探测。

---

## 4. 调度器架构与退避状态机 (Scheduler & State Machine)

### 4.1 数据结构
```rust
pub struct RecoveryTask {
    pub account_id: String,
    pub email: String,
    pub attempt: u8,               // 1..=4
    pub next_probe_at: std::time::Instant,
    pub initial_reason: String,
}
```

调度器内部持有 `Arc<DashMap<String, RecoveryTask>>`，以 `account_id` 为唯一键，天然支持防重。

### 4.2 退避阶梯时间表 (Ladder Schedule)
当 Step 0 探测失败后，进入如下调度阶段：

| 阶段 | 延迟间隔 | 触发时间说明 | 累计时间 | 业务目标 |
| :--- | :--- | :--- | :--- | :--- |
| **Step 0** | 0s | 触发 429 瞬间 | 0 | 毫秒级自愈假 429 / 输错模型 |
| **Step 1** | 60s | Step 0 失败后 1 分钟 | 1 分钟 | 捕获短时并发高峰回落 |
| **Step 2** | 15m | Step 1 失败后 15 分钟 | 16 分钟 | 捕获短周期 RPM 限流恢复 |
| **Step 3** | 1h | Step 2 失败后 1 小时 | 1 小时 16 分钟 | 捕获小时级配额刷新 |
| **Step 4** | 4h | Step 3 失败后 4 小时 | 5 小时 16 分钟 | 捕获 Google 4~5h Sprint 周期配额重置 |

### 4.3 轮询与终止机制
* 调度器启动后台循环（每 10 秒 tick 一次），检查已到期的任务；
* 到期后发起 `gemini-2.5-flash` 探针请求；
* **探测成功**：触发自愈恢复（见第 5 节），任务移出队列；
* **探测失败**：
  * 若 `attempt < 4`：`attempt += 1`，按阶梯更新 `next_probe_at`；
  * 若 `attempt == 4`：任务从队列中移除，终止后续探测，账号保持 `proxy_disabled = true`，记录告警日志。

### 4.4 启动扫描自检 (Boot Scan)
* 当应用/反代服务启动时，调度器自动遍历本地账号；
* 凡符合以下条件的账号自动重新纳管：
  1. `account.proxy_disabled == true`
  2. `account.proxy_disabled_reason` 包含 `"429"`、`"QuotaExhausted"`、`"RateLimitExceeded"`（排除含有 `"manual"` 或 `"Forbidden"` 的账号）
* 初始入队参数：`attempt = 1`，`next_probe_at = now + 60s`（服务启动 1 分钟后自动初检）。

---

## 5. 自愈恢复与系统联动 (Recovery Action & Synchronization)

一旦任一探针返回 HTTP 200，系统原子执行以下操作：

1. **持久化状态翻转**：
   - 调用 `toggle_proxy_status(account_id, true, None)`；
   - 磁盘 JSON 中 `proxy_disabled` 置为 `false`，清空 `proxy_disabled_reason` 和 `proxy_disabled_at`；
   - 更新账号索引 `accounts_index.json`。
2. **内存池与限流重载**：
   - 清理 `RateLimitTracker` 中的限流锁记录；
   - 调用 `TokenManager::reload_account`，账号重新进入内存调度池；
   - 串行池模式下自动将账号归入可用轮换列表。
3. **前端 UI 实时感知**：
   - 触发 `emit_accounts_refreshed()` 事件广播；
   - 前端无需刷新页面，账号红标自动变绿恢复正常；
   - 输出清晰的日志：`🎉 [AutoRecovery] Account <email> successfully recovered via probe (attempt: <N>), re-enabled for proxy.`
4. **人工操作防冲突**：
   - 若用户在退避期间手动点击了“启用反代”，调度器收到事件后直接注销该任务；
   - 若用户手动执行了“禁用反代”（原因标为手动），调度器绝不干涉，立即注销探测任务。

---

## 6. 测试与验证策略 (Testing & Verification)

1. **单元测试 (`proxy::tests::auto_recovery`)**：
   - 模拟 Step 0 成功场景：验证账号不被置为 `proxy_disabled`；
   - 模拟 Step 0 失败场景：验证账号被置为 `proxy_disabled` 且正确入队；
   - 模拟 Step 1~4 阶梯时间推移：验证定时器正确推进和最大 4 阶后终止。
2. **集成测试 (Mock Upstream)**：
   - 模拟上游返回 429，验证探测请求只发给 `gemini-2.5-flash` 且参数符合极简格式；
   - 验证自愈后账号能够重新承接 OpenAI / Claude 兼容请求。
