# 流量日志本地与远端来源区分与筛选设计方案

**日期**: 2026-10-23  
**状态**: Approved (待实施)  
**作者**: Antigravity  

---

## 1. 背景与目标

在当前的流量日志排查中，错误状态码（如 429 Too Many Requests、403 Forbidden、500 等）来源不明确：
- 一部分是在本地代理阶段由于配额保护（Quota Protection）、小憩熔断（Circuit Breaker / Micro-nap）、并发满载、无可用账号、鉴权失败等直接在本地拦截并返回的；
- 另一部分是请求真正发往远端上游（Google Gemini、OpenAI、Anthropic 等）后由上游服务器返回的。

为了让管理员排查故障时能够一目了然区分是「本地拦截」还是「远端真实响应」，并在高并发场景下按来源精准筛选，特设计本方案。

---

## 2. 总体架构与设计方案

### 2.1 来源判定逻辑 (Source Tracking)
1. **远端返回 (`remote`)**：
   - 凡是经过上游客户端（如 `UpstreamClient` / `rquest`）向上游发起真实 HTTP 请求并收到 Response 的链路，在构造返回 Response 时注入内部响应头 `X-Response-Source: remote`。
2. **本地返回 (`local`)**：
   - 凡是在本地鉴权（Auth 中间件）、配额判定、并发保护、小憩调度、请求校验等阶段提前构造错误或拦截响应退出的，不具有 `X-Response-Source: remote`，或者显式注入 `X-Response-Source: local`。
3. **日志中间件捕获 (`src-tauri/src/proxy/middleware/monitor.rs`)**：
   - 在捕获 Response 时，读取 `response.headers().get("X-Response-Source")`。
   - 若为 `remote` 则标记为 `"remote"`，否则标记为 `"local"`。

---

## 3. 数据层改造

### 3.1 数据结构扩充
- `src-tauri/src/proxy/monitor.rs` 中的 `ProxyRequestLog`：
  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct ProxyRequestLog {
      // ... 现有字段
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub response_source: Option<String>, // "remote" | "local"
  }
  ```

### 3.2 数据库存储与迁移 (`src-tauri/src/modules/proxy_db.rs`)
- 初始化表结构增加字段：
  ```rust
  let _ = conn.execute("ALTER TABLE request_logs ADD COLUMN response_source TEXT", []);
  conn.execute("CREATE INDEX IF NOT EXISTS idx_response_source ON request_logs (response_source)", []);
  ```
- 日志插入语句写入 `response_source`。
- 查询接口扩展：
  - `get_proxy_logs_filtered(filter, errors_only, source_filter, limit, offset)`
  - `get_proxy_logs_count_filtered(filter, errors_only, source_filter)`
  - 支持按 `source_filter` (`"all" | "remote" | "local"`) 过滤。历史空数据向下兼容降级判定。

### 3.3 HTTP API 接口支持 (`src-tauri/src/modules/http_api.rs` & `commands/proxy.rs`)
- 扩展 Tauri 命令与 HTTP API，接收可选参数 `source` 或 `source_filter`。

---

## 4. 前端界面与交互设计

### 4.1 数据模型扩展 (`src/components/proxy/ProxyMonitor.tsx`)
- `ProxyRequestLog` 接口新增 `response_source?: 'local' | 'remote'`。

### 4.2 筛选栏扩展
- 在搜索框与账号筛选下拉框之间，新增「来源」下拉筛选：
  - `全部来源` (All Sources)
  - `远端上游` (Remote Upstream)
  - `本地拦截` (Local Intercept)
- 切换筛选时自动重置至第 1 页并重新拉取。

### 4.3 列表表格视觉设计
- 在「状态码」或「协议」旁显示明确的来源 Badge：
  - 远端：`<span className="badge badge-xs badge-info">远端</span>`（清新蓝色或绿色）
  - 本地：`<span className="badge badge-xs badge-warning">本地</span>`（醒目橙黄色）
- 支持鼠标悬浮提示解释（Tooltip）。

### 4.4 详情弹窗增强
- 在日志详细信息面板中，增加「响应来源」字段：
  - 远端：`远端上游 (已实际发送至服务商上游接口)`
  - 本地：`本地拦截 (未打到上游，由本地代理策略或保护机制直接返回)`

---

## 5. 验收标准
1. 发起正常请求至 Google/上游，流量日志记录 `response_source = "remote"`，前端显示「远端」徽章。
2. 触发本地拦截（如无账号、配额满、Token错误），流量日志记录 `response_source = "local"`，前端显示「本地」徽章。
3. 在流量日志页面切换「全部 / 远端 / 本地」下拉筛选，列表与分页总数能准确过滤并实时刷新。
4. 点击日志项展开详情，能够明确看到该请求的响应来源。
