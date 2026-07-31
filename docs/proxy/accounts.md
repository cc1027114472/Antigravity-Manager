# Proxy account pool & auto-disable behavior

## What we wanted
- Keep the proxy “always-on” even when some Google OAuth accounts become invalid.
- Avoid repeatedly attempting to refresh a revoked `refresh_token` (noise + wasted requests).
- Make failures actionable by surfacing account state clearly in the UI.

## What we got
### 1) Disabled accounts are skipped by the proxy pool
Account files can be marked as disabled on disk (`accounts/<id>.json`):
- `disabled: true`
- `disabled_at: <unix_ts>`
- `disabled_reason: <string>`

The proxy token pool loader skips such accounts:
- `TokenManager::load_single_account(...)` in [`src-tauri/src/proxy/token_manager.rs`](../../src-tauri/src/proxy/token_manager.rs)

### 2) Automatic disable on OAuth `invalid_grant`
If an account refresh fails with `invalid_grant` during token refresh, the proxy marks it disabled and removes it from the in-memory pool:
- Refresh/disable logic: `TokenManager::get_token(...)` in [`src-tauri/src/proxy/token_manager.rs`](../../src-tauri/src/proxy/token_manager.rs)
- Persist disable flags to disk: `TokenManager::disable_account(...)` in [`src-tauri/src/proxy/token_manager.rs`](../../src-tauri/src/proxy/token_manager.rs)

This prevents endless rotation attempts against a dead account.

### 3) Batch quota refresh concurrency & egress
`refresh_all_quotas_logic` in [`src-tauri/src/modules/account.rs`](../../src-tauri/src/modules/account.rs):
- Skips accounts marked `quota.is_forbidden` (not all `disabled` flags).
- Global concurrency max **5**.
- **Same egress key** (`proxy:{id}` from AI-identical resolve, else `upstream` / `direct`) runs **at most 1** refresh at a time, so multiple accounts sharing one IP do not hit Google in parallel.
- Per-account **resolve-once + pin**: the batch task resolves egress once via `resolve_and_pin_egress`, so nested token refresh / quota / project calls reuse that same proxy (avoids RoundRobin advancing mid-task).

### 3b) Unified account egress (= AI traffic)
Quota fetch, OAuth `refresh_access_token` (with `account_id`), `project_resolver::fetch_project_id`, calibration ticker (via batch refresh), and AI chat all share the same picker:

1. Bound proxy for that account  
2. Else unbound pool selection (`select_proxy_from_pool`)  
3. Else global upstream proxy  
4. Else direct  

See: `get_proxy_for_account` / `get_effective_standard_client` / `resolve_and_pin_egress` in [`src-tauri/src/proxy/proxy_pool.rs`](../../src-tauri/src/proxy/proxy_pool.rs).

**Sticky same IP across separate requests** still prefers **one account ↔ one proxy binding**. Unbound + RoundRobin/Random may pick different pool nodes on different tasks; within one batch task the pin keeps them aligned.

### 3c) Local quota ledger (estimate + calibrate)
Selection and `quota_protection` prefer **local estimated remaining %** per standard model id (`Account.estimated_quotas`), not the last online snapshot alone.

| Role | Source |
|------|--------|
| Real-time intercept / sort | Local ledger (`estimated_quotas` → in-memory `ProxyToken.model_quotas`) |
| Calibration | Online `fetch_quota` via `update_account_quota` (overwrites estimates) |
| Hard backstop | Google 429 (non-grace): immediate rotate; set `proxy_disabled`, remove from pool, advance serial cursor; rate-limit lock without blocking on realtime quota fetch |

Burn on successful proxy responses (`ProxyMonitor::log_request` → `TokenManager::burn_estimated_quota`):

- `burn = max(min_burn_pct, ceil(tokens / tokens_per_percent))` (no usage → `min_burn_pct` only)
- Config: `AppConfig.quota_ledger` (`enabled`, `min_burn_pct` default 1, `tokens_per_percent` default 20000)
- Threshold compare uses `<=` so reserve at exactly the threshold is protected
- **Threshold-cross calibrate:** when quota protection is on and a monitored billing group first crosses from above the threshold to `<= threshold`, the proxy fetches official quota once (10‑minute cooldown per account+group), overwrites the local ledger via `update_account_quota`, then decides protection from the calibrated %. Calibrate failure falls back to protecting on the local estimate.

**Serial pool vs ledger:** serial advance still keys off `protected_models` / rate limits (AI selection). The ledger only makes protection trip sooner between online refreshes. Both share the same account↔proxy binding table for egress; they are not the same feature.

Headless calibration: Rust ticker `modules/quota_calibration.rs` follows `auto_refresh` + `refresh_interval` (does not depend on the UI `BackgroundTaskRunner`).

When `quota_ledger.enabled=false`, protection/selection fall back to the online `quota` snapshot only.

**UI display:** bars and ranks use local ledger via `getDisplayQuotaModels` ([`src/utils/quotaDisplay.ts`](../../src/utils/quotaDisplay.ts)); official % is tooltip/校验 only. After online refresh, calibration overwrites local estimates so UI aligns with Google until the next burns. Toolbar: **同步本地** re-reads disk `estimated_quotas` via silent `fetchAccounts` (no Google/OAuth); **刷新所有** runs online calibration. UI does not auto-poll or auto-refresh after burns — click 同步本地 to update %.

### 4) UI surfaces disabled state and blocks actions
The accounts UI reads `disabled` fields and shows a “Disabled” badge and tooltip, and disables “switch / refresh” controls:
- Account type includes `disabled*` fields: [`src/types/account.ts`](../../src/types/account.ts)
- Card view: [`src/components/accounts/AccountCard.tsx`](../../src/components/accounts/AccountCard.tsx)
- Table row view: [`src/components/accounts/AccountRow.tsx`](../../src/components/accounts/AccountRow.tsx)
- Filters: “Available” excludes disabled accounts: [`src/pages/Accounts.tsx`](../../src/pages/Accounts.tsx)

Translations:
- [`src/locales/en.json`](../../src/locales/en.json)
- [`src/locales/zh.json`](../../src/locales/zh.json)

### 5) API errors avoid leaking user emails
Token refresh failures returned to API clients no longer include account emails:
- Error message construction: `TokenManager::get_token(...)` in [`src-tauri/src/proxy/token_manager.rs`](../../src-tauri/src/proxy/token_manager.rs)
- Proxy error mapping: `handle_messages(...)` in [`src-tauri/src/proxy/handlers/claude.rs`](../../src-tauri/src/proxy/handlers/claude.rs)

## Operational guidance
- If an account becomes disabled due to `invalid_grant`, it usually means the `refresh_token` was revoked or expired.
- Re-authorize the account (or update the stored token) to restore it.

## Validation
1) Ensure at least one account file has `disabled: true`.
2) Start the proxy and verify:
   - The disabled account is not selected for requests.
   - Batch quota refresh logs show “Skipping … (Disabled)”.
   - The UI shows the Disabled badge and blocks actions.
