/**
 * Standalone tests for quota display merge.
 * Run: npx tsx src/utils/__tests__/quotaDisplay.test.ts
 */
import type { Account } from '../../types/account';
import {
    formatQuotaTooltip,
    getBillingGroupDisplays,
    getDisplayQuotaModels,
} from '../quotaDisplay';

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

test('tooltip shows official only when different', () => {
    assertEq(formatQuotaTooltip(10, 10), '本地估算 10%');
    assertEq(formatQuotaTooltip(10, 40), '本地估算 10% · 官方校准 40%');
    assertEq(formatQuotaTooltip(10, undefined), '本地估算 10%');
});

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
