# Humanized Nap Display & Models Listing Log Silence Design

**Date:** 2026-10-22  
**Status:** Approved (design); pending implementation  
**Scope:** 
1. Expose `HumanizedPacer` micro-nap countdown (`nap_remaining_seconds`) through `list_accounts` to frontend card and table views with an Indigo moon badge (`🌙 小憩中 Xm`) and explanatory tooltip.
2. Silence high-frequency model listing queries (`GET /v1beta/models` and `GET /v1/models`) with HTTP status < 400 in `monitor_middleware`, eliminating log flooding while preserving genuine generation and error logs.

---

## 1. Problem Statement

### 1.1 Micro-Nap Invisibility
`HumanizedPacer` dynamically places accounts into a 2~5 minute micro-nap (after every 20~35 requests) to avoid multi-account homogenization and upstream anti-automation detection. While in a micro-nap, the account scheduler softly avoids the napping account in favor of other rested accounts, yet guarantees fallback to available accounts if all are napping or only one account exists.
However, the UI currently provides zero visibility into this state: an account undergoing a micro-nap looks identical to an active account, leaving users puzzled why traffic is temporarily not distributed to it.

### 1.2 Model Listing Log Flooding
External AI tools and IDE plugins (e.g., Cherry Studio, NextChat, LobeChat, Cursor, Cline) frequently poll model list endpoints (`GET /v1beta/models` and `GET /v1/models`) upon startup, focus, or periodic refresh. Because these requests produce 0 upstream token consumption and are answered purely from memory, recording them in `proxy_logs.db` fills the request log table with repetitive listing noise, diluting actual prompt/generation logs.

---

## 2. Goals & Non-Goals

### 2.1 Goals
- Expose runtime micro-nap state per account: calculate `nap_remaining_seconds` in `HumanizedPacer` without persisting to disk.
- Return `nap_remaining_seconds` through `list_accounts` command.
- Render a concise, clean UI badge (`🌙 小憩中 Xm`) with a descriptive tooltip in both `AccountCard` and `AccountRow` when `nap_remaining_seconds > 0` and the account is not otherwise disabled or rate-limited.
- In `monitor_middleware`, silently pass through successful (`status < 400`) `GET` requests to `/v1beta/models` and `/v1/models` without persisting to `proxy_logs.db` or pushing to the in-memory monitor ring buffer.
- Keep error responses (`status >= 400`) on model listing routes logged to assist debugging.

### 2.2 Non-Goals
- Do not make micro-nap a hard blocker: scheduler routing logic remains soft-bypass with fallback.
- Do not persist nap states to SQLite or disk across restarts (transient in-memory state).
- Do not silence `POST` generation requests or any other endpoint.

---

## 3. Technical Architecture & Changes

### 3.1 Backend: Pacer & Account Model
1. **`src-tauri/src/proxy/rate_limit.rs` (`HumanizedPacer`)**:
   Add query method:
   ```rust
   pub fn get_nap_remaining_seconds(&self, account_id: &str) -> Option<u64> {
       if let Some(entry) = self.account_states.get(account_id) {
           if let Some(nap) = entry.nap_until {
               let now = std::time::Instant::now();
               if now < nap {
                   return Some((nap - now).as_secs());
               }
           }
       }
       None
   }
   ```

2. **`src-tauri/src/models/account.rs` (`Account`)**:
   Add runtime-injected field:
   ```rust
   /// [UI] Runtime-only: remaining seconds of humanized micro-nap.
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub nap_remaining_seconds: Option<u64>,
   ```

3. **`src-tauri/src/commands/mod.rs` (`list_accounts`)**:
   Inject the countdown into each account:
   ```rust
   if let Some(nap_secs) = instance.token_manager.pacer.get_nap_remaining_seconds(&account.id) {
       account.nap_remaining_seconds = Some(nap_secs);
   }
   ```

### 3.2 Backend: Monitor Middleware Silence
**`src-tauri/src/proxy/middleware/monitor.rs`**:
Define a helper function:
```rust
fn is_pure_model_listing_success(method: &Method, uri: &str, status: u16) -> bool {
    if method != Method::GET || status >= 400 {
        return false;
    }
    let path = uri.split('?').next().unwrap_or(uri);
    path == "/v1beta/models" || path == "/v1/models"
}
```
In `monitor_middleware`:
If `is_pure_model_listing_success(&method, &uri, status)` is `true`, immediately return `response` without wrapping in stream interception or calling `monitor.log_request`.

### 3.3 Frontend: Types & Display
1. **`src/types/account.ts`**:
   Add `nap_remaining_seconds?: number;` to interface `Account`.

2. **`src/components/accounts/AccountCard.tsx` & `src/components/accounts/AccountRow.tsx`**:
   When `account.nap_remaining_seconds != null && account.nap_remaining_seconds > 0 && !isDisabled && !account.proxy_disabled`:
   Render:
   - Badge: Indigo background (`bg-indigo-100 text-indigo-700 dark:bg-indigo-900/40 dark:text-indigo-300 border-indigo-200/50`)
   - Icon: `<Moon className="w-2.5 h-2.5" />`
   - Label: `小憩中 ${Math.ceil(account.nap_remaining_seconds / 60)}m` (or i18n equivalent)
   - Tooltip: `"拟人化防风控微小憩中，剩余约 X 分钟。期间流量优先走其他账号；若无其他可用账号，系统会自动唤醒响应。"`

---

## 4. Verification & Testing Strategy

1. **Unit Tests**:
   - Verify `HumanizedPacer::get_nap_remaining_seconds` returns `None` for un-napped accounts and `Some(s)` for actively napping accounts.
   - Verify `is_pure_model_listing_success` correctly filters `GET /v1beta/models` and `GET /v1/models` when status is 200, but returns `false` for POST or error statuses (e.g. 500).
2. **Build Verification**:
   - Run `cargo check --lib` to ensure Rust backend compiles cleanly.
   - Run `pnpm run build` or `pnpm run check` for frontend TypeScript validity.
