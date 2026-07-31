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
- **Same egress key** (bound `proxy:{id}`, else `upstream` / `direct`) runs **at most 1** refresh at a time, so multiple accounts sharing one IP do not hit Google in parallel.

### 3b) Quota / token-refresh egress (account ops)
Quota fetch and OAuth `refresh_access_token` (with `account_id`) use **account-ops egress**, not the AI traffic pool picker:

1. Bound proxy for that account  
2. Else global upstream proxy  
3. Else direct  

**Unbound accounts never scrape `select_proxy_from_pool`** (avoids sharing someone else’s / random pool IP for ops).  
AI chat traffic still uses `get_effective_client` / pool selection as before.

Prefer **one account ↔ one proxy binding** so quota refresh and AI share the same egress.

See: `get_effective_standard_client_for_account_ops` / `egress_key_for_account` in [`src-tauri/src/proxy/proxy_pool.rs`](../../src-tauri/src/proxy/proxy_pool.rs).

### 3c) Local quota ledger (estimate + calibrate)
Selection and `quota_protection` prefer **local estimated remaining %** per standard model id (`Account.estimated_quotas`), not the last online snapshot alone.

| Role | Source |
|------|--------|
| Real-time intercept / sort | Local ledger (`estimated_quotas` → in-memory `ProxyToken.model_quotas`) |
| Calibration | Online `fetch_quota` via `update_account_quota` (overwrites estimates) |
| Hard backstop | Google 429 / realtime fetch also calibrates the ledger before lockout |

Burn on successful proxy responses (`ProxyMonitor::log_request` → `TokenManager::burn_estimated_quota`):

- `burn = max(min_burn_pct, ceil(tokens / tokens_per_percent))` (no usage → `min_burn_pct` only)
- Config: `AppConfig.quota_ledger` (`enabled`, `min_burn_pct` default 1, `tokens_per_percent` default 20000)
- Threshold compare uses `<=` so reserve at exactly the threshold is protected

**Serial pool vs ledger:** serial advance still keys off `protected_models` / rate limits (AI selection). The ledger only makes protection trip sooner between online refreshes. Both share the same account↔proxy binding table for egress; they are not the same feature.

Headless calibration: Rust ticker `modules/quota_calibration.rs` follows `auto_refresh` + `refresh_interval` (does not depend on the UI `BackgroundTaskRunner`).

When `quota_ledger.enabled=false`, protection/selection fall back to the online `quota` snapshot only.

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
