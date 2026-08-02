# Weekly Quota Display Priority

**Date:** 2026-08-02  
**Status:** Approved (design); pending implementation  
**Scope:** Account list/card official quota snapshot selection between `weekly` and `5h` buckets

## Problem

Google quota has dual windows (`weekly` + `5h`) per billing group. The list UI always preferred the `5h` bucket via `officialBillingPercentages`. When the weekly Individual quota was exhausted (e.g. reset in ~108h) while the 5h snapshot still showed ~100%, the UI looked healthy even though requests returned 429 `RESOURCE_EXHAUSTED`.

## Goal

When the official **weekly** bucket is exhausted, list/card display must show weekly remaining % and weekly `reset_time`. Otherwise keep showing the **5h** bucket (current default).

## Non-goals

- Do not change Account Details dual-bucket cards (both `weekly` and `5h` remain visible).
- Do not change `liveLimit` / red ERR overlay or its tooltip.
- Do not change ledger precedence: `estimated_quotas` still wins over official snapshot for the displayed percentage when present.
- Do not introduce a configurable mid-range threshold (e.g. switch at 20%).

## Decision

**Approach A:** Change only `officialBillingPercentages` in `src/utils/quotaDisplay.ts`.

### Selection rule (per billing group: `gemini` | `claude`)

1. Parse that group’s `weekly` and `5h` buckets from `quota_groups`.
2. If a **weekly** bucket exists and `remaining_fraction <= 0` (display 0%) → use weekly `%` and `reset_time`.
3. Else → prefer `5h`; if no `5h`, fall back to weekly (or any other non-5h bucket already used as fallback).

### Exhaustion threshold

`remaining_fraction <= 0` after the existing `fractionToPct` path (i.e. displayed percentage `<= 0`). No intermediate threshold.

### Call sites covered (no UI rewiring)

Anything that goes through `resolveBillingPct` / `getSplitQuotaDisplays` / `getListQuotaDisplays` / `getBillingGroupDisplays` inherits the new official selection automatically.

## Tests

Add cases in `src/utils/__tests__/quotaDisplay.test.ts`:

1. Weekly `remaining_fraction = 0`, 5h `= 1.0` → list/split shows `0%` and weekly `reset_time`.
2. Weekly `= 0.5`, 5h `= 0.8` → still shows 5h `80%` and 5h `reset_time`.

Existing “prefer official 5h when no ledger” tests remain valid when weekly is absent or not exhausted.

## Implementation notes

- Identify weekly via `window === 'weekly'` or `bucket_id` containing `weekly` (mirror existing 5h detection).
- Keep billing-group mapping (`gemini` / `3p` / display_name heuristics) unchanged.
- Do not commit unless explicitly requested.
