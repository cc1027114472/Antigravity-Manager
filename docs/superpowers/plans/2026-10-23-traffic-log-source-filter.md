# 流量日志本地与远端来源区分与筛选实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在流量日志中增加「本地返回」与「远端返回」的来源标识与独立筛选功能，让管理员能够直观区分上游真实响应与本地保护/限流拦截。

**Architecture:** 
1. 后端：上游通信与本地拦截层通过内部响应头 `X-Response-Source: remote|local` 传递来源标记；`middleware/monitor.rs` 捕获并持久化到 SQLite `request_logs` 的 `response_source` 列；数据库查询接口支持按来源进行 SQL 级精准过滤。
2. 前端：`ProxyMonitor.tsx` 扩展状态与类型定义，在顶部工具栏提供「全部来源 / 远端上游 / 本地拦截」下拉筛选，并在表格状态列显示直观徽章，详情面板显示来源说明。

**Tech Stack:** Rust (Axum, Tauri v2, SQLite/rusqlite), TypeScript, React, TailwindCSS, daisyUI.

---

### Task 1: 数据模型与数据库存储扩展

**Files:**
- Modify: `src-tauri/src/proxy/monitor.rs`
- Modify: `src-tauri/src/modules/proxy_db.rs`

- [ ] **Step 1: 修改 `ProxyRequestLog` 增加 `response_source` 字段**
在 `src-tauri/src/proxy/monitor.rs` 的 `ProxyRequestLog` 结构体中添加：
```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_source: Option<String>, // "remote" | "local"
```
并在 `ProxyMonitor::log_request` 构造摘要 `log_summary` 时保留 `response_source: log.response_source.clone()`。

- [ ] **Step 2: 修改 `proxy_db.rs` 增加数据库列与索引**
在 `init_db()` 中添加：
```rust
    let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN response_source TEXT", []);
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_response_source ON request_logs (response_source)",
        [],
    );
```

- [ ] **Step 3: 修改 `save_log`、`get_logs_summary`、`get_log_detail` 读写 `response_source` 列**
在 `save_log` SQL `INSERT INTO request_logs` 添加 `response_source`；在读取各 SQL 查询处映射 `row.get("response_source").ok()` 到 `log.response_source`。

- [ ] **Step 4: 修改 `get_proxy_logs_filtered` 和 `get_proxy_logs_count_filtered` 支持来源过滤**
支持可选参数 `source_filter: Option<&str>`（如 `"all"`, `"remote"`, `"local"`）。
在 SQL 构建逻辑中，若 `source_filter == Some("remote")` 则追加 `AND response_source = 'remote'`；
若 `source_filter == Some("local")` 则追加 `AND (response_source = 'local' OR (response_source IS NULL AND status >= 400))`（兼容历史数据）。

- [ ] **Step 5: 运行 cargo check 验证 Task 1 编译无误**
Run: `cargo check --lib` in `src-tauri`
Expected: 编译通过无错误。

---

### Task 2: 后端响应来源标记与中间件捕获

**Files:**
- Modify: `src-tauri/src/proxy/account_inflight.rs`
- Modify: `src-tauri/src/proxy/handlers/gemini.rs`
- Modify: `src-tauri/src/proxy/handlers/openai.rs`
- Modify: `src-tauri/src/proxy/handlers/claude.rs`
- Modify: `src-tauri/src/proxy/middleware/monitor.rs`

- [ ] **Step 1: 在 `account_inflight::account_headers` 及响应构造处注入 `X-Response-Source`**
在 `src-tauri/src/proxy/account_inflight.rs` 的 `account_headers` 中添加标头：
```rust
headers.insert("X-Response-Source", HeaderValue::from_static("remote"));
```
并在流式响应的 `builder` 中添加 `.header("X-Response-Source", "remote")`。

- [ ] **Step 2: 在本地拦截与保护返回处注入 `X-Response-Source: local`**
在本地配额超限（Quota exceeded）、无可用账号（All accounts exhausted / cooling down）、中间件拦截处，显式添加 `.header("X-Response-Source", "local")`。

- [ ] **Step 3: 在 `middleware/monitor.rs` 提取来源并记录**
在 `middleware/monitor.rs` 解析响应时：
```rust
    let response_source = response
        .headers()
        .get("X-Response-Source")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            // 兜底降级判定
            if status < 400 && response.headers().contains_key("X-Account-Email") {
                Some("remote".to_string())
            } else {
                Some("local".to_string())
            }
        });
```
赋给 `ProxyRequestLog` 的 `response_source`。

- [ ] **Step 4: 运行 cargo check 验证 Task 2 编译无误**
Run: `cargo check --lib` in `src-tauri`
Expected: 编译通过无错误。

---

### Task 3: 后端 API 与 Tauri 命令支持

**Files:**
- Modify: `src-tauri/src/commands/proxy.rs`
- Modify: `src-tauri/src/modules/http_api.rs`

- [ ] **Step 1: 修改 Tauri 命令 `get_proxy_logs_filtered` 与 `get_proxy_logs_count_filtered`**
增加可选参数 `source_filter: Option<String>`，传递给 `proxy_db` 的对应函数。

- [ ] **Step 2: 修改 HTTP API 路由与查询参数解析**
在 `http_api.rs` 的日志查询接口（如 `GET /api/proxy/logs`）中，解析 query 参数 `source` 并传递给查询函数。

- [ ] **Step 3: 运行 cargo check 验证 Task 3 编译无误**
Run: `cargo check --lib` in `src-tauri`
Expected: 编译通过无错误。

---

### Task 4: 前端界面展示与筛选交互

**Files:**
- Modify: `src/components/proxy/ProxyMonitor.tsx`

- [ ] **Step 1: 扩展前端 `ProxyRequestLog` 类型定义**
在 `ProxyMonitor.tsx` 中添加：
```typescript
export interface ProxyRequestLog {
  // ... 现有字段
  response_source?: 'local' | 'remote';
}
```

- [ ] **Step 2: 增加 `sourceFilter` 状态与下拉菜单**
新增 `sourceFilter` 状态（`'all' | 'remote' | 'local'`），并在搜索框与账号筛选器旁添加 Select 下拉框：
- 🌐 全部来源
- ☁️ 远端上游
- 🏠 本地拦截
选择变更时重置 `currentPage = 1` 并触发 `loadData(1, filter, accountFilter, newSourceFilter)`。

- [ ] **Step 3: 修改 `loadData` 传递 `source_filter` 参数**
在调用 `invoke('get_proxy_logs_filtered', { ... })` 和 `get_proxy_logs_count_filtered` 时带上 `sourceFilter: sourceFilter === 'all' ? undefined : sourceFilter`。

- [ ] **Step 4: 在表格状态列增加来源徽章**
在 `LogTable` 的状态码/协议旁或紧贴状态 Badge 处渲染来源小徽章：
```tsx
{log.response_source === 'local' ? (
  <span className="badge badge-xs badge-warning gap-1 font-medium text-[10px]" title="本地代理策略或保护机制直接返回">
    本地
  </span>
) : (
  <span className="badge badge-xs badge-info gap-1 font-medium text-[10px]" title="已发送至远端上游接口返回">
    远端
  </span>
)}
```

- [ ] **Step 5: 在日志详情面板增加响应来源项**
在选中日志的详情弹窗中，增加「响应来源」信息条目，显示：
- `远端上游 (Remote Upstream)` 或 `本地拦截 (Local Intercept)`。

---

### Task 5: 多语言词条补充与端到端编译验证

**Files:**
- Modify: `src/locales/zh.json`
- Modify: `src/locales/en.json`
- (同步其他语言文件，避免缺失报错)

- [ ] **Step 1: 在各语言文件中新增来源相关词条**
如：
```json
"proxy_monitor": {
  "source_all": "全部来源",
  "source_remote": "远端上游",
  "source_local": "本地拦截",
  "source_remote_desc": "请求已实际发送至服务商上游接口",
  "source_local_desc": "本地代理策略或保护机制直接返回",
  "response_source": "响应来源"
}
```

- [ ] **Step 2: 运行前端 TypeScript 类型检查与构建**
Run: `npx tsc --noEmit` & `npm run build`
Expected: 前端无类型错误，Vite 构建顺利产出至 `dist/`。

- [ ] **Step 3: 运行 Rust 后端编译检查**
Run: `cargo check --lib`
Expected: 后端无错误。
