# Humanized Nap Display & Models Log Silence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Provide full visibility into runtime humanized micro-nap countdowns on the accounts page while eliminating log noise from high-frequency model listing queries.

**Architecture:** Expose `HumanizedPacer` micro-nap countdown seconds via `list_accounts` dynamic injection into `Account.nap_remaining_seconds`. Display an Indigo `🌙 小憩中 Xm` badge and tooltip on `AccountCard` and `AccountRow`. Silence `GET /v1beta/models` and `GET /v1/models` in `monitor_middleware` (already implemented and verified).

**Tech Stack:** Rust (Axum, DashMap, Tauri commands), React 18, TypeScript, TailwindCSS, Lucide-React, i18next.

---

### Task 1: HumanizedPacer Nap Countdown Query & Unit Test

**Files:**
- Modify: `src-tauri/src/proxy/rate_limit.rs:110-150`
- Test: `src-tauri/src/proxy/rate_limit.rs:880-925`

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/proxy/rate_limit.rs` inside `mod tests`:
```rust
    #[test]
    fn test_humanized_pacer_nap_countdown() {
        let pacer = HumanizedPacer::new();
        assert_eq!(pacer.get_nap_remaining_seconds("non_existent"), None);

        let now = std::time::Instant::now();
        pacer.account_states.insert(
            "test_acc".to_string(),
            AccountPacerState {
                last_request_time: now,
                request_counter: 25,
                current_interval_ms: 2000,
                nap_until: Some(now + Duration::from_secs(120)),
                threshold_jitter: 0,
            },
        );

        assert!(pacer.is_in_nap("test_acc"));
        let remaining = pacer.get_nap_remaining_seconds("test_acc");
        assert!(remaining.is_some());
        let secs = remaining.unwrap();
        assert!(secs >= 118 && secs <= 120);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run:
```powershell
$env:PATH = "C:\Users\Administrator\scoop\apps\mingw-winlibs\current\bin;D:\GOWorks\fanzhongli\Antigravity-Manager\src-tauri\target\debug\deps;$env:PATH"
$env:CC = "gcc"
$env:CXX = "g++"
$env:CMAKE_GENERATOR = "MinGW Makefiles"
$env:CMAKE_MAKE_PROGRAM = "mingw32-make"
$env:BINDGEN_EXTRA_CLANG_ARGS = "--target=x86_64-pc-windows-gnu"
cargo test --lib -- test_humanized_pacer_nap_countdown
```
Expected: FAIL with `no method named get_nap_remaining_seconds found for struct HumanizedPacer`.

- [ ] **Step 3: Implement get_nap_remaining_seconds on HumanizedPacer**

In `src-tauri/src/proxy/rate_limit.rs` inside `impl HumanizedPacer`:
```rust
    /// Returns the remaining seconds of an active micro-nap, or None if not napping.
    pub fn get_nap_remaining_seconds(&self, account_id: &str) -> Option<u64> {
        if let Some(entry) = self.account_states.get(account_id) {
            if let Some(nap_until) = entry.nap_until {
                let now = std::time::Instant::now();
                if now < nap_until {
                    return Some((nap_until - now).as_secs());
                }
            }
        }
        None
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run:
```powershell
$env:PATH = "C:\Users\Administrator\scoop\apps\mingw-winlibs\current\bin;D:\GOWorks\fanzhongli\Antigravity-Manager\src-tauri\target\debug\deps;$env:PATH"
$env:CC = "gcc"
$env:CXX = "g++"
$env:CMAKE_GENERATOR = "MinGW Makefiles"
$env:CMAKE_MAKE_PROGRAM = "mingw32-make"
$env:BINDGEN_EXTRA_CLANG_ARGS = "--target=x86_64-pc-windows-gnu"
cargo test --lib -- test_humanized_pacer_nap_countdown
```
Expected: PASS (`test proxy::rate_limit::tests::test_humanized_pacer_nap_countdown ... ok`).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/proxy/rate_limit.rs
git commit -m "feat(pacer): add get_nap_remaining_seconds query on HumanizedPacer"
```

---

### Task 2: Account Model Runtime Field & list_accounts Injection

**Files:**
- Modify: `src-tauri/src/models/account.rs:90-130`
- Modify: `src-tauri/src/commands/mod.rs:50-65`

- [ ] **Step 1: Add nap_remaining_seconds to Account struct and new()**

In `src-tauri/src/models/account.rs`:
```rust
    /// [UI] Runtime-only: remaining seconds of humanized micro-nap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nap_remaining_seconds: Option<u64>,
```
In `Account::new`:
```rust
            auto_recovery_attempt: None,
            nap_remaining_seconds: None,
```

- [ ] **Step 2: Inject nap_remaining_seconds in list_accounts**

In `src-tauri/src/commands/mod.rs` around line 62 (after `auto_recovery_attempt` injection):
```rust
            // [UI] Inject humanized micro-nap countdown if currently napping
            if let Some(nap_secs) = instance.token_manager.pacer.get_nap_remaining_seconds(&account.id) {
                account.nap_remaining_seconds = Some(nap_secs);
            }
```

- [ ] **Step 3: Verify compilation with cargo check**

Run:
```powershell
$env:PATH = "C:\Users\Administrator\scoop\apps\mingw-winlibs\current\bin;D:\GOWorks\fanzhongli\Antigravity-Manager\src-tauri\target\debug\deps;$env:PATH"
$env:CC = "gcc"
$env:CXX = "g++"
$env:CMAKE_GENERATOR = "MinGW Makefiles"
$env:CMAKE_MAKE_PROGRAM = "mingw32-make"
$env:BINDGEN_EXTRA_CLANG_ARGS = "--target=x86_64-pc-windows-gnu"
cargo check --lib
```
Expected: PASS with 0 errors.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/models/account.rs src-tauri/src/commands/mod.rs
git commit -m "feat(account): inject runtime nap_remaining_seconds in list_accounts"
```

---

### Task 3: Frontend TypeScript Definitions & i18n Locales

**Files:**
- Modify: `src/types/account.ts:25-30`
- Modify: `src/locales/zh.json:150-165`
- Modify: `src/locales/en.json:150-165`

- [ ] **Step 1: Add nap_remaining_seconds to Account interface**

In `src/types/account.ts`:
```typescript
    /** Runtime-only: current auto-recovery attempt (1–4). Absent when not in queue. */
    auto_recovery_attempt?: number;
    /** Runtime-only: remaining seconds of humanized micro-nap. Absent when not in nap. */
    nap_remaining_seconds?: number;
```

- [ ] **Step 2: Add translation keys in zh.json and en.json**

In `src/locales/zh.json` under `"accounts"`:
```json
    "humanized_napping": "小憩中 {{mins}}m",
    "humanized_napping_tooltip": "拟人化防风控微小憩中，剩余约 {{mins}} 分钟。期间流量优先走其他账号；若无其他可用账号，系统会自动唤醒响应。",
```

In `src/locales/en.json` under `"accounts"`:
```json
    "humanized_napping": "Napping {{mins}}m",
    "humanized_napping_tooltip": "Humanized micro-nap for anti-risk pacing, approx {{mins}}m remaining. Traffic prefers other rested accounts; will wake up automatically if all accounts are busy.",
```

- [ ] **Step 3: Commit**

```bash
git add src/types/account.ts src/locales/zh.json src/locales/en.json
git commit -m "feat(i18n): add humanized napping labels and typescript fields"
```

---

### Task 4: Frontend UI Badges in AccountCard and AccountRow

**Files:**
- Modify: `src/components/accounts/AccountCard.tsx`
- Modify: `src/components/accounts/AccountRow.tsx`

- [ ] **Step 1: Import Moon in AccountCard and AccountRow**

In `src/components/accounts/AccountCard.tsx`:
Add `Moon` to the `lucide-react` import.

In `src/components/accounts/AccountRow.tsx`:
Add `Moon` to the `lucide-react` import.

- [ ] **Step 2: Render badge in AccountCard.tsx**

In `AccountCard.tsx` in the badges section:
```tsx
{account.nap_remaining_seconds != null && account.nap_remaining_seconds > 0 && !isDisabled && !account.proxy_disabled && (
    <span
        className="px-1.5 py-0.5 rounded-md bg-indigo-100 dark:bg-indigo-900/40 text-indigo-700 dark:text-indigo-300 text-[9px] font-bold flex items-center gap-1 shadow-sm border border-indigo-200/50"
        title={t('accounts.humanized_napping_tooltip', { mins: Math.ceil(account.nap_remaining_seconds / 60) })}
    >
        <Moon className="w-2.5 h-2.5" />
        {t('accounts.humanized_napping', { mins: Math.ceil(account.nap_remaining_seconds / 60) })}
    </span>
)}
```

- [ ] **Step 3: Render badge in AccountRow.tsx**

In `AccountRow.tsx` in the status badge section:
```tsx
{account.nap_remaining_seconds != null && account.nap_remaining_seconds > 0 && !isDisabled && !account.proxy_disabled && (
    <span
        className="px-2 py-0.5 rounded-md bg-indigo-100 dark:bg-indigo-900/50 text-indigo-700 dark:text-indigo-300 text-[10px] font-bold flex items-center gap-1 shadow-sm border border-indigo-200/50"
        title={t('accounts.humanized_napping_tooltip', { mins: Math.ceil(account.nap_remaining_seconds / 60) })}
    >
        <Moon className="w-2.5 h-2.5" />
        <span>{t('accounts.humanized_napping', { mins: Math.ceil(account.nap_remaining_seconds / 60) })}</span>
    </span>
)}
```

- [ ] **Step 4: Check frontend TypeScript compilation**

Run:
```bash
pnpm run build
```
Expected: Build succeeds with 0 errors.

- [ ] **Step 5: Commit**

```bash
git add src/components/accounts/AccountCard.tsx src/components/accounts/AccountRow.tsx
git commit -m "feat(ui): display humanized micro-nap badge in AccountCard and AccountRow"
```

---

### Task 5: End-to-End Verification

- [ ] **Step 1: Run Rust unit tests**

```powershell
$env:PATH = "C:\Users\Administrator\scoop\apps\mingw-winlibs\current\bin;D:\GOWorks\fanzhongli\Antigravity-Manager\src-tauri\target\debug\deps;$env:PATH"
$env:CC = "gcc"
$env:CXX = "g++"
$env:CMAKE_GENERATOR = "MinGW Makefiles"
$env:CMAKE_MAKE_PROGRAM = "mingw32-make"
$env:BINDGEN_EXTRA_CLANG_ARGS = "--target=x86_64-pc-windows-gnu"
cargo test --lib -- test_humanized_pacer_nap_countdown test_is_metadata_probe
```
Expected: Both tests pass.

- [ ] **Step 2: Verify git status is clean**
Verify no untracked temp files were added.
