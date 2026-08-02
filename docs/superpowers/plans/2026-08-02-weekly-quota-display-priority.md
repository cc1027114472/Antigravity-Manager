# Weekly Quota Display Priority Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When official weekly quota is exhausted (0%), list/card display shows weekly % and reset_time; otherwise keep preferring 5h.

**Architecture:** Adjust `officialBillingPercentages` in `quotaDisplay.ts` so weekly overrides 5h only when weekly remaining maps to ≤0%. All list/card callers already use this helper via `resolveBillingPct`.

**Tech Stack:** TypeScript, existing `npx tsx` standalone tests.

**Spec:** `docs/superpowers/specs/2026-08-02-weekly-quota-display-priority-design.md`

---

### Task 1: Failing tests

**Files:**
- Modify: `src/utils/__tests__/quotaDisplay.test.ts`

- [x] **Step 1: Add two tests** — weekly exhausted prefers weekly; weekly healthy prefers 5h
- [x] **Step 2: Run** `npx tsx src/utils/__tests__/quotaDisplay.test.ts`

### Task 2: Implement selection

**Files:**
- Modify: `src/utils/quotaDisplay.ts` (`officialBillingPercentages`)

- [x] **Step 1: Collect weekly + 5h per billing group; if weekly pct ≤ 0 use weekly else prefer 5h**
- [x] **Step 2: Re-run tests — all pass**

### Task 3: Done

- [x] **Step 1: Do not commit** unless user asks
