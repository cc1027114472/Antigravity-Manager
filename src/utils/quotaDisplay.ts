/**
 * Merge online quota snapshot with local estimated_quotas for UI display.
 * Primary bar % = local ledger when present; official % kept for tooltip.
 * List views use official billing groups (gemini | claude).
 */
import type { Account, EstimatedModelQuota, ModelQuota, QuotaGroup } from '../types/account';
import {
    findImageQuotaModel,
    findQuotaModel,
    getModelProtectionKey,
    normalizeToBillingGroup,
    type BillingGroup,
    type ModelCategory,
    BILLING_GROUPS,
} from './modelCategory';

export type DisplayModelQuota = ModelQuota & {
    officialPercentage?: number;
};

export type BillingGroupDisplay = {
    id: BillingGroup;
    label: string;
    percentage: number;
    officialPercentage?: number;
    reset_time?: string;
};

function lookupEstimated(
    estimated: Record<string, EstimatedModelQuota> | undefined,
    modelName: string,
): EstimatedModelQuota | undefined {
    if (!estimated) return undefined;
    const billing = normalizeToBillingGroup(modelName);
    if (billing && estimated[billing]) return estimated[billing];
    const direct = estimated[modelName];
    if (direct) return direct;
    const key = getModelProtectionKey(modelName);
    if (key && estimated[key]) return estimated[key];
    const lower = modelName.toLowerCase();
    for (const [k, v] of Object.entries(estimated)) {
        if (k.toLowerCase() === lower) return v;
        if (billing && normalizeToBillingGroup(k) === billing) return v;
        if (key && k.toLowerCase() === key.toLowerCase()) return v;
    }
    return undefined;
}

function fractionToPct(fraction: number): number {
    return Math.round(Math.min(100, Math.max(0, fraction * 100)));
}

/** Prefer 5h bucket from official quota_groups. */
export function officialBillingPercentages(
    groups: QuotaGroup[] | undefined | null,
): Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> {
    if (!groups?.length) return {};
    const fiveH: Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> = {};
    const fallback: Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> = {};

    for (const group of groups) {
        for (const bucket of group.buckets ?? []) {
            const id = (bucket.bucket_id || '').toLowerCase();
            let billing: BillingGroup | null = null;
            if (id.startsWith('gemini') || id.includes('gemini')) billing = 'gemini';
            else if (id.startsWith('3p') || id.includes('3p') || id.includes('claude')) billing = 'claude';
            if (!billing) {
                const dn = (group.display_name || '').toLowerCase();
                if (dn.includes('gemini')) billing = 'gemini';
                else if (dn.includes('claude') || dn.includes('gpt') || dn.includes('3p')) billing = 'claude';
            }
            if (!billing) continue;
            const entry = {
                percentage: fractionToPct(bucket.remaining_fraction),
                reset_time: bucket.reset_time,
            };
            const is5h =
                (bucket.window || '').toLowerCase() === '5h' || id.includes('5h');
            if (is5h) fiveH[billing] = entry;
            else if (!fallback[billing]) fallback[billing] = entry;
        }
    }
    return { ...fallback, ...fiveH };
}

/**
 * Primary list display: two official billing groups.
 */
export function getBillingGroupDisplays(
    account: Account | null | undefined,
): BillingGroupDisplay[] {
    if (!account) return [];

    const estimated = account.estimated_quotas;
    const official = officialBillingPercentages(account.quota?.quota_groups);
    const labels: Record<BillingGroup, string> = {
        gemini: 'Gemini',
        claude: 'Claude',
    };

    // Fallback min from online models when no groups / ledger
    const onlineMin: Partial<Record<BillingGroup, number>> = {};
    for (const m of account.quota?.models ?? []) {
        const g = normalizeToBillingGroup(m.name);
        if (!g) continue;
        onlineMin[g] =
            onlineMin[g] === undefined
                ? m.percentage
                : Math.min(onlineMin[g]!, m.percentage);
    }

    return BILLING_GROUPS.map((id) => {
        const est = estimated?.[id] ?? lookupEstimated(estimated, id);
        const off = official[id];
        const percentage =
            est?.percentage ??
            off?.percentage ??
            onlineMin[id] ??
            0;
        const officialPercentage =
            est?.lastOnlinePct ?? off?.percentage ?? onlineMin[id];
        return {
            id,
            label: labels[id],
            percentage,
            officialPercentage:
                officialPercentage !== undefined && officialPercentage !== percentage
                    ? officialPercentage
                    : est?.lastOnlinePct !== undefined && est.lastOnlinePct !== percentage
                      ? est.lastOnlinePct
                      : off?.percentage !== percentage
                        ? off?.percentage
                        : undefined,
            reset_time: off?.reset_time,
        };
    }).filter((row) => {
        // Show row if we have any signal for this group
        const hasEst = !!(estimated && (estimated[row.id] || lookupEstimated(estimated, row.id)));
        const hasOff = official[row.id] !== undefined;
        const hasOnline = onlineMin[row.id] !== undefined;
        return hasEst || hasOff || hasOnline || row.percentage > 0;
    });
}

/**
 * Models list for UI: percentage from local ledger when available.
 * (Per-model detail / auxiliary — not the primary billing view.)
 */
export function getDisplayQuotaModels(account: Account | null | undefined): DisplayModelQuota[] {
    if (!account) return [];

    const estimated = account.estimated_quotas;
    const hasLedger = !!(estimated && Object.keys(estimated).length > 0);
    const onlineModels = account.quota?.models ?? [];

    if (onlineModels.length > 0) {
        return onlineModels.map((m) => {
            const est = hasLedger ? lookupEstimated(estimated, m.name) : undefined;
            if (!est) {
                return { ...m };
            }
            const official = est.lastOnlinePct ?? m.percentage;
            return {
                ...m,
                percentage: est.percentage,
                officialPercentage: official,
            };
        });
    }

    if (!hasLedger || !estimated) return [];

    return Object.entries(estimated).map(([stdId, est]) => ({
        name: est.model || stdId,
        percentage: est.percentage,
        reset_time: '',
        officialPercentage: est.lastOnlinePct,
    }));
}

/** Tooltip: show official calibration only when it differs from local. */
export function formatQuotaTooltip(
    localPct: number | undefined,
    officialPct: number | undefined,
): string {
    if (localPct === undefined || localPct === null || Number.isNaN(localPct)) {
        return '';
    }
    if (
        officialPct === undefined ||
        officialPct === null ||
        Number.isNaN(officialPct) ||
        officialPct === localPct
    ) {
        return `本地估算 ${localPct}%`;
    }
    return `本地估算 ${localPct}% · 官方校准 ${officialPct}%`;
}

export function findDisplayQuotaModel(
    account: Account | null | undefined,
    category: ModelCategory,
): DisplayModelQuota | undefined {
    return findQuotaModel(getDisplayQuotaModels(account), category);
}

export function findDisplayImageQuotaModel(
    account: Account | null | undefined,
): DisplayModelQuota | undefined {
    return findImageQuotaModel(getDisplayQuotaModels(account));
}
