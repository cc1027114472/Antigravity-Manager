/**
 * Standalone tests for quota display merge.
 * Run: npx tsx src/utils/__tests__/quotaDisplay.test.ts
 */
import type { Account } from '../../types/account';
import {
    formatQuotaTooltip,
    getBillingGroupDisplays,
    getDisplayQuotaModels,
    getListQuotaDisplays,
    getSplitQuotaDisplays,
} from '../quotaDisplay';
import { getLiveLimitForModel } from '../liveLimit';

let passed = 0;
let failed = 0;

function test(description: string, fn: () => void): void {
    try {
        fn();
        passed++;
        console.log(`  ok  ${description}`);
    } catch (e) {
        failed++;
        console.error(`  FAIL ${description}`);
        console.error(e);
    }
}

function assertEq<T>(actual: T, expected: T, msg?: string): void {
    if (actual !== expected) {
        throw new Error(`${msg ?? 'assertEq'} expected=${JSON.stringify(expected)} actual=${JSON.stringify(actual)}`);
    }
}

const baseAccount = (over: Partial<Account> = {}): Account => ({
    id: 'a1',
    email: 'a@test.com',
    token: {
        access_token: 'x',
        refresh_token: 'y',
        expires_in: 3600,
        expiry_timestamp: 0,
        token_type: 'Bearer',
    },
    created_at: 0,
    last_used: 0,
    ...over,
});

console.log('quotaDisplay tests');

test('falls back to online when no ledger', () => {
    const account = baseAccount({
        quota: {
            models: [{ name: 'claude-sonnet-4-6', percentage: 55, reset_time: '' }],
            last_updated: 1,
        },
    });
    const models = getDisplayQuotaModels(account);
    assertEq(models[0]?.percentage, 55);
    assertEq(models[0]?.officialPercentage, undefined);
});

test('overlays estimated percentage onto online model', () => {
    const account = baseAccount({
        quota: {
            models: [{ name: 'claude-sonnet-4-6', percentage: 55, reset_time: 't' }],
            last_updated: 1,
        },
        estimated_quotas: {
            claude: {
                model: 'claude',
                percentage: 12,
                lastOnlinePct: 55,
            },
        },
    });
    const models = getDisplayQuotaModels(account);
    assertEq(models[0]?.percentage, 12);
    assertEq(models[0]?.officialPercentage, 55);
    assertEq(models[0]?.reset_time, 't');
});

test('synthesizes from ledger when online models missing', () => {
    const account = baseAccount({
        estimated_quotas: {
            gemini: {
                model: 'gemini',
                percentage: 33,
                lastOnlinePct: 40,
            },
        },
    });
    const models = getDisplayQuotaModels(account);
    assertEq(models.length, 1);
    assertEq(models[0]?.percentage, 33);
    assertEq(models[0]?.name, 'gemini');
});

test('billing group display prefers ledger over official groups', () => {
    const account = baseAccount({
        estimated_quotas: {
            gemini: { model: 'gemini', percentage: 15, lastOnlinePct: 42 },
            claude: { model: 'claude', percentage: 80, lastOnlinePct: 80 },
        },
        quota: {
            models: [],
            last_updated: 1,
            quota_groups: [
                {
                    display_name: 'Gemini Models',
                    buckets: [
                        {
                            bucket_id: 'gemini-5h',
                            window: '5h',
                            remaining_fraction: 0.42,
                            reset_time: 't',
                        },
                    ],
                },
            ],
        },
    });
    const rows = getBillingGroupDisplays(account);
    const gemini = rows.find((r) => r.id === 'gemini');
    assertEq(gemini?.percentage, 15);
});

test('split display: Pro/Flash/Image share gemini ledger %; Claude independent', () => {
    const account = baseAccount({
        estimated_quotas: {
            gemini: { model: 'gemini', percentage: 42, lastOnlinePct: 50 },
            claude: { model: 'claude', percentage: 77, lastOnlinePct: 77 },
        },
    });
    const rows = getSplitQuotaDisplays(account);
    assertEq(rows.length, 4);
    const pro = rows.find((r) => r.id === 'gemini-pro');
    const flash = rows.find((r) => r.id === 'gemini-flash');
    const image = rows.find((r) => r.id === 'gemini-image');
    const claude = rows.find((r) => r.id === 'claude');
    assertEq(pro?.percentage, 42);
    assertEq(flash?.percentage, 42);
    assertEq(image?.percentage, 42);
    assertEq(claude?.percentage, 77);
    assertEq(pro?.protectedKey, 'gemini');
    assertEq(flash?.protectedKey, 'gemini');
    assertEq(image?.protectedKey, 'gemini');
    assertEq(claude?.protectedKey, 'claude');
});

test('split display: without ledger, Gemini slots share official 5h %', () => {
    const account = baseAccount({
        quota: {
            models: [],
            last_updated: 1,
            quota_groups: [
                {
                    display_name: 'Gemini Models',
                    buckets: [
                        {
                            bucket_id: 'gemini-5h',
                            window: '5h',
                            remaining_fraction: 0.63,
                            reset_time: 'reset-g',
                        },
                    ],
                },
                {
                    display_name: 'Claude / GPT',
                    buckets: [
                        {
                            bucket_id: '3p-5h',
                            window: '5h',
                            remaining_fraction: 0.21,
                            reset_time: 'reset-c',
                        },
                    ],
                },
            ],
        },
    });
    const rows = getSplitQuotaDisplays(account);
    const geminiPcts = rows
        .filter((r) => r.protectedKey === 'gemini')
        .map((r) => r.percentage);
    assertEq(geminiPcts.length, 3);
    assertEq(geminiPcts[0], 63);
    assertEq(geminiPcts[1], 63);
    assertEq(geminiPcts[2], 63);
    const claude = rows.find((r) => r.id === 'claude');
    assertEq(claude?.percentage, 21);
});

test('list display: showAll=false defaults to four split slots', () => {
    const account = baseAccount({
        estimated_quotas: {
            gemini: { model: 'gemini', percentage: 42, lastOnlinePct: 50 },
            claude: { model: 'claude', percentage: 77, lastOnlinePct: 77 },
        },
    });
    const rows = getListQuotaDisplays(account, { showAll: false });
    assertEq(rows.length, 4);
    assertEq(rows.filter((r) => r.protectedKey === 'gemini').every((r) => r.percentage === 42), true);
});

test('list display: showAll=true overlays billing % on online models', () => {
    const account = baseAccount({
        estimated_quotas: {
            gemini: { model: 'gemini', percentage: 42, lastOnlinePct: 90 },
            claude: { model: 'claude', percentage: 11, lastOnlinePct: 20 },
        },
        quota: {
            models: [
                { name: 'gemini-3.1-pro-high', percentage: 90, reset_time: '' },
                { name: 'gemini-3-flash', percentage: 88, reset_time: '' },
                { name: 'claude-sonnet-4-6', percentage: 20, reset_time: '' },
            ],
            last_updated: 1,
        },
    });
    const rows = getListQuotaDisplays(account, { showAll: true });
    assertEq(rows.length >= 3, true);
    const geminiRows = rows.filter((r) => r.protectedKey === 'gemini');
    const claudeRows = rows.filter((r) => r.protectedKey === 'claude');
    assertEq(geminiRows.every((r) => r.percentage === 42), true);
    assertEq(claudeRows.every((r) => r.percentage === 11), true);
});

test('list display: pin claude-only filters to claude slot(s)', () => {
    const account = baseAccount({
        estimated_quotas: {
            gemini: { model: 'gemini', percentage: 42, lastOnlinePct: 50 },
            claude: { model: 'claude', percentage: 77, lastOnlinePct: 77 },
        },
    });
    const rows = getListQuotaDisplays(account, {
        showAll: false,
        pinnedModels: ['claude-opus-4-6'],
    });
    assertEq(rows.length, 1);
    assertEq(rows[0]?.protectedKey, 'claude');
    assertEq(rows[0]?.percentage, 77);
});

test('liveLimit: exact model id and billing-group fallback both resolve 429', () => {
    const now = Math.floor(Date.now() / 1000);
    const account = baseAccount({
        live_limited_models: {
            'gemini-3-flash': {
                model: 'gemini-3-flash',
                status: 429,
                reason: 'QuotaExhausted',
                until: now + 120,
                detected_at: now,
                message: 'rate limited',
            },
        },
    });
    assertEq(getLiveLimitForModel(account, 'gemini-3-flash')?.status, 429);
    assertEq(getLiveLimitForModel(account, 'gemini-flash', 'gemini')?.status, 429);
    assertEq(getLiveLimitForModel(account, undefined, 'gemini')?.status, 429);
    assertEq(getLiveLimitForModel(account, 'claude', 'claude'), undefined);
});

test('tooltip shows official only when different', () => {
    assertEq(formatQuotaTooltip(10, 10), '本地估算 10%');
    assertEq(formatQuotaTooltip(10, 40), '本地估算 10% · 官方校准 40%');
    assertEq(formatQuotaTooltip(10, undefined), '本地估算 10%');
});

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
