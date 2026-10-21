# Pending CU Micro-Accumulator & Min-Burn Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Introduce `pending_cu` micro-accumulator for fractional Compute Unit tracking, deduct integer quota percentages on floor threshold (`>= 1%`), retain remainder CU, and abolish the forced `min_burn = 1%` penalty for tiny requests.

**Architecture:**
Split quota calculation into exact delta CU calculation and window-specific accumulation. Track `pending_cu_5h` and `pending_cu_weekly` per account per model billing group in memory (`ProxyToken`) and persist to disk (`EstimatedModelQuota`). Micro-requests accumulate CU without prematurely docking 1%; when accumulated CU reaches 1% capacity floor, deduct the integer points and preserve the fractional remainder.

**Tech Stack:** Rust, Tokio, DashMap, Serde JSON, Cargo test.

---

### Task 1: Model & Config Layer (`src-tauri/src/models/config.rs`)

**Files:**
- Modify: `src-tauri/src/models/config.rs:165-168, 303-375`
- Test: `src-tauri/src/modules/quota_ledger.rs:515-625`

- [x] **Step 1: Write failing tests for delta CU and accumulation without min-burn penalty**

In `src-tauri/src/modules/quota_ledger.rs`, add unit tests:
- `test_compute_delta_cu_pro_and_flash`: Verify exact CU computation for various token sizes.
- `test_accumulate_and_burn_micro_requests`: Verify 200-token requests accumulate fractional CU without deducting integer percentage until `pending_cu >= 5.0` (1% of 500 CU sprint), then burn 1% and retain fractional remainder.
- `test_min_burn_pct_default_is_zero`: Verify default min burn does not penalize 0 CU.

```rust
#[test]
fn test_accumulate_and_burn_micro_requests() {
    let cfg = QuotaLedgerConfig::default();
    // 200 input tokens on Claude (multiplier 3.0, w_in 1.0, conservative 1.25)
    // weighted = 200, delta_cu = (200/1000) * 3.0 * 1.25 = 0.75 CU.
    // Sprint capacity = 500 CU -> 1% = 5.0 CU.
    // 1st request: 0.75 CU -> burn 0%, remainder 0.75 CU.
    let (burn_5h, rem_5h, burn_w, rem_w) = cfg.accumulate_and_burn_dual(
        0.0, 0.0,
        "claude-3-7-sonnet",
        Some(200), Some(0), None
    );
    assert_eq!(burn_5h, 0);
    assert!((rem_5h - 0.75).abs() < 1e-6);
    assert_eq!(burn_w, 0);

    // 6 more identical requests (total 7 * 0.75 = 5.25 CU):
    // 5.25 / 5.0 = 1.05 -> burn 1%, remainder 0.25 CU.
    let (burn_5h_final, rem_5h_final, _, _) = cfg.accumulate_and_burn_dual(
        rem_5h * 6.0, 0.0,
        "claude-3-7-sonnet",
        Some(200), Some(0), None
    );
    assert_eq!(burn_5h_final, 1);
    assert!((rem_5h_final - 0.25).abs() < 1e-6);
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --bin antigravity-manager -- test_accumulate_and_burn_micro_requests`
Expected: FAIL with "no method named `accumulate_and_burn_dual` found"

- [x] **Step 3: Implement `compute_delta_cu`, `accumulate_and_burn_dual`, and update `default_min_burn_pct`**

In `src-tauri/src/models/config.rs`:
1. Change `default_min_burn_pct() -> u32` to return `0`.
2. Add `compute_delta_cu`:
```rust
    pub fn compute_delta_cu(
        &self,
        model: &str,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        cached_tokens: Option<u32>,
    ) -> f64 {
        let input = input_tokens.unwrap_or(0) as f64;
        let output = output_tokens.unwrap_or(0) as f64;
        let cache = cached_tokens.unwrap_or(0) as f64;
        let has_usage = input_tokens.is_some() || output_tokens.is_some() || cached_tokens.is_some();
        let any_tokens = input > 0.0 || output > 0.0 || cache > 0.0;

        if !has_usage || !any_tokens {
            return 0.0;
        }

        let billable_in = if self.cache_is_subset_of_input {
            (input - cache).max(0.0)
        } else {
            input
        };

        let tokens_per_cu = if self.tokens_per_cu > 0.0 {
            self.tokens_per_cu
        } else {
            1000.0
        };
        let m = self.resolve_model_multiplier(model);
        let weighted =
            billable_in * self.w_in + cache * self.w_cache + output * self.w_out;
        let delta_cu = (weighted / tokens_per_cu) * m;
        if delta_cu <= 0.0 {
            return 0.0;
        }
        let conservative = if self.conservative_multiplier >= 1.0 {
            self.conservative_multiplier
        } else {
            1.25
        };
        delta_cu * conservative
    }

    pub fn accumulate_and_burn_5h(&self, pending_cu: f64, delta_cu: f64) -> (i32, f64) {
        let sprint = if self.sprint_capacity_cu > 0.0 {
            self.sprint_capacity_cu
        } else {
            500.0
        };
        let cu_per_pct = sprint / 100.0;
        let total_cu = pending_cu + delta_cu;
        if cu_per_pct <= 0.0 {
            return (0, 0.0);
        }
        let burn_pct = (total_cu / cu_per_pct).floor() as i32;
        let remainder = total_cu - (burn_pct as f64 * cu_per_pct);
        (burn_pct, remainder)
    }

    pub fn accumulate_and_burn_weekly(&self, pending_cu: f64, delta_cu: f64) -> (i32, f64) {
        let marathon = if self.marathon_capacity_cu > 0.0 {
            self.marathon_capacity_cu
        } else {
            self.sprint_capacity_cu.max(500.0) * 3.0
        };
        let cu_per_pct = marathon / 100.0;
        let total_cu = pending_cu + delta_cu;
        if cu_per_pct <= 0.0 {
            return (0, 0.0);
        }
        let burn_pct = (total_cu / cu_per_pct).floor() as i32;
        let remainder = total_cu - (burn_pct as f64 * cu_per_pct);
        (burn_pct, remainder)
    }

    pub fn accumulate_and_burn_dual(
        &self,
        pending_cu_5h: f64,
        pending_cu_weekly: f64,
        model: &str,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        cached_tokens: Option<u32>,
    ) -> (i32, f64, i32, f64) {
        let delta_cu = self.compute_delta_cu(model, input_tokens, output_tokens, cached_tokens);
        let (burn_5h, rem_5h) = self.accumulate_and_burn_5h(pending_cu_5h, delta_cu);
        let (burn_weekly, rem_weekly) = self.accumulate_and_burn_weekly(pending_cu_weekly, delta_cu);
        (burn_5h, rem_5h, burn_weekly, rem_weekly)
    }
```
3. Update `compute_burn_pct`: abolish forced `min_burn = 1` floor; use `self.min_burn_pct` without clamping to 1.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin antigravity-manager -- test_accumulate_and_burn_micro_requests`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/models/config.rs src-tauri/src/modules/quota_ledger.rs
git commit -m "feat(ledger): add compute_delta_cu and accumulate_and_burn_dual with 0% min-burn floor"
```

---

### Task 2: Persistence Layer for Pending CU (`src-tauri/src/models/quota.rs` & `account.rs`)

**Files:**
- Modify: `src-tauri/src/models/quota.rs:107-135, 159-180`
- Modify: `src-tauri/src/modules/account.rs:1685-1740`
- Test: `src-tauri/src/modules/account.rs` unit tests

- [x] **Step 1: Write failing test for persisting and loading pending CU**

Add a test in `src-tauri/src/modules/account.rs` or `quota_ledger.rs` verifying that `persist_estimated_quota_burn` stores `pending_cu_5h` and `pending_cu_weekly`, and loading the account recovers them intact.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --bin antigravity-manager -- test_persist_pending_cu`
Expected: FAIL

- [x] **Step 3: Update `EstimatedModelQuota` and `persist_estimated_quota_burn`**

1. In `src-tauri/src/models/quota.rs`:
Add fields to `EstimatedModelQuota`:
```rust
    /// Fractional CU accumulated in 5h window pending integer percentage burn
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_cu_5h: Option<f64>,
    /// Fractional CU accumulated in 7d weekly window pending integer percentage burn
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_cu_weekly: Option<f64>,
```
In `from_online` / `from_online_dual`: initialize `pending_cu_5h: None, pending_cu_weekly: None` (authoritative reset on calibration).

2. In `src-tauri/src/modules/account.rs`:
Update `persist_estimated_quota_burn`:
```rust
pub fn persist_estimated_quota_burn(
    account_id: &str,
    std_model_id: &str,
    new_pct_5h: i32,
    new_pct_weekly: i32,
    pending_cu_5h: f64,
    pending_cu_weekly: f64,
    protect_model: bool,
) -> Result<(), String> {
...
    entry.pending_cu_5h = if pending_cu_5h > 0.0 { Some(pending_cu_5h) } else { None };
    entry.pending_cu_weekly = if pending_cu_weekly > 0.0 { Some(pending_cu_weekly) } else { None };
...
}
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin antigravity-manager -- test_persist_pending_cu`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/models/quota.rs src-tauri/src/modules/account.rs
git commit -m "feat(quota): persist pending_cu in EstimatedModelQuota"
```

---

### Task 3: TokenManager Runtime Micro-Accumulation (`src-tauri/src/proxy/token_manager.rs`)

**Files:**
- Modify: `src-tauri/src/proxy/token_manager.rs:30-53, 720-835, 995-1120`
- Test: `src-tauri/src/proxy/token_manager.rs` and proxy tests

- [x] **Step 1: Write failing test in `src-tauri/src/proxy/token_manager.rs`**

Add test `test_burn_account_quota_micro_requests_accumulate`:
- Set up test `TokenManager` with a `ProxyToken` holding 100% quota.
- Call `burn_account_quota_on_success` with 200 tokens (e.g. Claude).
- Assert `remaining_quota` remains 100% (not docked to 99%).
- Assert `pending_cu_5h` has accumulated 0.75 CU.
- Call it again repeatedly until total CU >= 5.0 CU.
- Assert quota drops to 99% and remainder CU is preserved.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --bin antigravity-manager -- test_burn_account_quota_micro_requests_accumulate`
Expected: FAIL

- [x] **Step 3: Integrate pending CU in `ProxyToken` and `burn_account_quota_on_success`**

1. In `ProxyToken`:
```rust
    pub pending_cu_5h: HashMap<String, f64>,
    pub pending_cu_weekly: HashMap<String, f64>,
```
Update all `ProxyToken` constructor sites to initialize `pending_cu_5h: HashMap::new()` and `pending_cu_weekly: HashMap::new()`.
In `load_token_from_account`, deserialize `pending_cu_5h` and `pending_cu_weekly` from `estimated_quotas`.

2. In `TokenManager::burn_account_quota_on_success`:
```rust
        let (burn_5h, rem_5h, burn_weekly, rem_weekly) = {
            let entry = self.tokens.get(&account_id)?;
            let p5h = entry.pending_cu_5h.get(&std_id).copied().unwrap_or(0.0);
            let pw = entry.pending_cu_weekly.get(&std_id).copied().unwrap_or(0.0);
            app_config.quota_ledger.accumulate_and_burn_dual(
                p5h,
                pw,
                model_raw,
                input_tokens,
                output_tokens,
                cached_tokens,
            )
        };

        // Always update in-memory pending CU
        if let Some(mut entry) = self.tokens.get_mut(&account_id) {
            entry.pending_cu_5h.insert(std_id.clone(), rem_5h);
            entry.pending_cu_weekly.insert(std_id.clone(), rem_weekly);
        }

        // Only deduct and protect when at least one window has integer burn
        if burn_5h <= 0 && burn_weekly <= 0 {
            tracing::debug!(
                "[QuotaLedger] micro-request account={} model={} pending_5h={:.3} pending_weekly={:.3} (no pct burn)",
                account_id, std_id, rem_5h, rem_weekly
            );
            return;
        }
```
Pass `rem_5h` and `rem_weekly` to `persist_estimated_quota_burn`.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin antigravity-manager -- test_burn_account_quota_micro_requests_accumulate`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/proxy/token_manager.rs
git commit -m "feat(token_manager): integrate pending_cu micro-accumulator and abolish min-burn 1%"
```

---

### Task 4: Full Suite Regression & Integration Verification

**Files:**
- Modify: `src-tauri/src/modules/quota_ledger.rs` (update old assertions that expected 1% for 10 tokens)
- Test: All proxy and quota ledger tests

- [x] **Step 1: Update legacy test expectations**

In `src-tauri/src/modules/quota_ledger.rs`, update tests where 10 tokens previously expected `1%` because of `min_burn = 1`:
- `burn_without_usage_uses_min`: when `min_burn_pct` is 0, burns 0%. When `min_burn_pct` is configured to 1, burns 1%.
- `burn_tiny_nonzero_ceils_to_min`: rename to `burn_tiny_nonzero_accumulates` or update to reflect 0% burn with accumulation.

- [x] **Step 2: Run full test suite**

Run: `cargo test --bin antigravity-manager`
Expected: All tests PASS.

- [x] **Step 3: Commit**

```bash
git add src-tauri/src/modules/quota_ledger.rs
git commit -m "test(ledger): update quota ledger unit tests for micro-accumulator semantics"
```
