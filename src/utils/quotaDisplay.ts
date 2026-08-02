/**
 * Merge online quota snapshot with local estimated_quotas for UI display.
 * Billing truth: gemini | claude big buckets.
 * List UI: four slots (Pro / Flash / Image / Claude); Gemini slots share one bucket %.
 */
import type { Account, EstimatedModelQuota, ModelQuota, QuotaGroup } from '../types/account';
import {
    ensurePinnedImageSelector,
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

/** Four-column list slot: labels split, percentages bound to billing groups. */
export type SplitQuotaSlotId = 'gemini-pro' | 'gemini-flash' | 'gemini-image' | 'claude';

export type SplitQuotaDisplay = {
    id: SplitQuotaSlotId;
    label: string;
    /** Official billing group key used for % and protection lock */
    protectedKey: BillingGroup;
    percentage: number;
    officialPercentage?: number;
    reset_time?: string;
    /** True when ledger/official/online has any signal for this billing group */
    hasData: boolean;
};

const SPLIT_SLOTS: Array<{
    id: SplitQuotaSlotId;
    label: string;
    billing: BillingGroup;
}> = [
    { id: 'gemini-pro', label: 'G3.1 Pro', billing: 'gemini' },
    { id: 'gemini-flash', label: 'G3 Flash', billing: 'gemini' },
    { id: 'gemini-image', label: 'G3 Image', billing: 'gemini' },
    { id: 'claude', label: 'Claude', billing: 'claude' },
];

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

/**
 * Prefer 5h bucket from official quota_groups.
 * When weekly remaining is exhausted (≤0%), prefer weekly so list shows the binding reset.
 */
export function officialBillingPercentages(
    groups: QuotaGroup[] | undefined | null,
): Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> {
    if (!groups?.length) return {};
    const fiveH: Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> = {};
    const weekly: Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> = {};
    const fallback: Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> = {};

    for (const group of groups) {
        for (const bucket of group.buckets ?? []) {
            const id = (bucket.bucket_id || '').toLowerCase();
            const window = (bucket.window || '').toLowerCase();
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
            const is5h = window === '5h' || id.includes('5h');
            const isWeekly = window === 'weekly' || id.includes('weekly');
            if (is5h) fiveH[billing] = entry;
            else if (isWeekly) weekly[billing] = entry;
            else if (!fallback[billing]) fallback[billing] = entry;
        }
    }

    const out: Partial<Record<BillingGroup, { percentage: number; reset_time?: string }>> = {
        ...fallback,
        ...fiveH,
    };
    for (const [billing, entry] of Object.entries(weekly) as Array<
        [BillingGroup, { percentage: number; reset_time?: string }]
    >) {
        if (entry.percentage <= 0) {
            out[billing] = entry;
        } else if (!out[billing]) {
            out[billing] = entry;
        }
    }
    return out;
}

function onlineBillingMins(
    account: Account,
): Partial<Record<BillingGroup, number>> {
    const onlineMin: Partial<Record<BillingGroup, number>> = {};
    for (const m of account.quota?.models ?? []) {
        const g = normalizeToBillingGroup(m.name);
        if (!g) continue;
        onlineMin[g] =
            onlineMin[g] === undefined
                ? m.percentage
                : Math.min(onlineMin[g]!, m.percentage);
    }
    return onlineMin;
}

export type BillingQuotaResolved = {
    percentage: number;
    officialPercentage?: number;
    reset_time?: string;
    hasData: boolean;
};

/** Unified list/card row (split slots or expanded model rows). */
export type ListQuotaDisplay = {
    id: string;
    label: string;
    protectedKey: BillingGroup;
    percentage: number;
    officialPercentage?: number;
    reset_time?: string;
};

export type ListQuotaDisplayOptions = {
    showAll?: boolean;
    /** Raw pinned model ids from settings; billing-normalized when showAll is off. */
    pinnedModels?: string[];
};

function resolveBillingPct(
    account: Account,
    billing: BillingGroup,
): BillingQuotaResolved {
    const estimated = account.estimated_quotas;
    const official = officialBillingPercentages(account.quota?.quota_groups);
    const onlineMin = onlineBillingMins(account);

    const est = estimated?.[billing] ?? lookupEstimated(estimated, billing);
    const off = official[billing];
    const hasEst = !!est;
    const hasOff = off !== undefined;
    const hasOnline = onlineMin[billing] !== undefined;
    const hasData = hasEst || hasOff || hasOnline;

    const percentage =
        est?.percentage ??
        off?.percentage ??
        onlineMin[billing] ??
        0;

    const rawOfficial = est?.lastOnlinePct ?? off?.percentage ?? onlineMin[billing];
    const officialPercentage =
        rawOfficial !== undefined && rawOfficial !== percentage
            ? rawOfficial
            : undefined;

    return {
        percentage,
        officialPercentage,
        reset_time: off?.reset_time,
        hasData,
    };
}

/** Public alias for list overlay / callers that need billing bucket %. */
export function resolveBillingQuota(
    account: Account,
    billing: BillingGroup,
): BillingQuotaResolved {
    return resolveBillingPct(account, billing);
}

function modelLabel(name: string, displayName?: string): string {
    const lower = name.toLowerCase();
    // Keep list labels readable without pulling MODEL_CONFIG (icons / React).
    const SHORT: Record<string, string> = {
        'gemini-pro': 'G3.1 Pro',
        'gemini-flash': 'G3 Flash',
        'gemini-image': 'G3 Image',
        gemini: 'Gemini',
        claude: 'Claude',
        'gemini-3.1-pro-high': 'G3.1 Pro',
        'gemini-3-pro-high': 'G3.1 Pro',
        'gemini-3-flash': 'G3 Flash',
        'gemini-3-flash-agent': 'G3 Flash',
        'gemini-pro-agent': 'G3.1 Pro',
        'gemini-3.1-flash-image': 'G3 Image',
        'gemini-3-pro-image': 'G3 Image',
    };
    return displayName || SHORT[lower] || name;
}

function sortListRows(rows: ListQuotaDisplay[]): ListQuotaDisplay[] {
    const weight = (id: string): number => {
        const n = id.toLowerCase();
        if (n.includes('pro') && n.includes('image')) return 30;
        if (n.includes('flash') && n.includes('image')) return 31;
        if (n.includes('image')) return 32;
        if (n.includes('pro')) return 10;
        if (n.includes('flash')) return 20;
        if (n.includes('claude') || n.includes('opus') || n.includes('sonnet')) return 40;
        return 50;
    };
    return [...rows].sort((a, b) => {
        const d = weight(a.id) - weight(b.id);
        return d !== 0 ? d : a.id.localeCompare(b.id);
    });
}

function dedupeListRows(rows: ListQuotaDisplay[]): ListQuotaDisplay[] {
    const uniqueLabels = new Set<string>();
    const withDataPass = rows.filter((m) => {
        if (m.id.includes('thinking')) return false;
        const labelKey = `${m.label}-${m.protectedKey}`;
        if (uniqueLabels.has(labelKey)) return false;
        uniqueLabels.add(labelKey);
        return true;
    });
    return withDataPass.filter((m, index, self) => {
        const labelKey = `${m.label}-${m.protectedKey}`;
        return self.findIndex((t) => `${t.label}-${t.protectedKey}` === labelKey) === index;
    });
}

/**
 * Account list/card rows:
 * - showAll off + no pins → four split slots
 * - showAll off + pins → split slots filtered by pinned billing groups
 * - showAll on → all online/ledger models with billing % overlay
 */
export function getListQuotaDisplays(
    account: Account | null | undefined,
    options: ListQuotaDisplayOptions = {},
): ListQuotaDisplay[] {
    if (!account) return [];

    const { showAll = false, pinnedModels } = options;

    if (!showAll) {
        const splits = getSplitQuotaDisplays(account);
        const pins = (pinnedModels ?? []).map((p) => p.trim()).filter(Boolean);
        if (pins.length === 0) return splits;

        const billingPins = new Set(ensurePinnedImageSelector(pins));
        const filtered = splits.filter((row) => billingPins.has(row.protectedKey));
        return filtered.length > 0 ? filtered : splits;
    }

    const ledgerModels = getDisplayQuotaModels(account);
    const rows: ListQuotaDisplay[] = ledgerModels.map((m) => {
        const billing = normalizeToBillingGroup(m.name) ?? 'gemini';
        const resolved = resolveBillingPct(account, billing);
        return {
            id: m.name.toLowerCase(),
            label: modelLabel(m.name, m.display_name),
            protectedKey: billing,
            percentage: resolved.hasData ? resolved.percentage : m.percentage,
            officialPercentage: resolved.officialPercentage ?? m.officialPercentage,
            reset_time: resolved.reset_time || m.reset_time,
        };
    });

    return sortListRows(dedupeListRows(rows));
}

/**
 * List/card primary display: four columns.
 * Pro / Flash / Image share billing group `gemini`; Claude uses `claude`.
 */
export function getSplitQuotaDisplays(
    account: Account | null | undefined,
): SplitQuotaDisplay[] {
    if (!account) return [];

    return SPLIT_SLOTS.map((slot) => {
        const resolved = resolveBillingPct(account, slot.billing);
        return {
            id: slot.id,
            label: slot.label,
            protectedKey: slot.billing,
            percentage: resolved.percentage,
            officialPercentage: resolved.officialPercentage,
            reset_time: resolved.reset_time,
            hasData: resolved.hasData,
        };
    }).filter((row) => row.hasData || row.percentage > 0);
}

/**
 * Two official billing groups (kept for settings / debug).
 */
export function getBillingGroupDisplays(
    account: Account | null | undefined,
): BillingGroupDisplay[] {
    if (!account) return [];

    const labels: Record<BillingGroup, string> = {
        gemini: 'Gemini',
        claude: 'Claude',
    };

    return BILLING_GROUPS.map((id) => {
        const resolved = resolveBillingPct(account, id);
        return {
            id,
            label: labels[id],
            percentage: resolved.percentage,
            officialPercentage: resolved.officialPercentage,
            reset_time: resolved.reset_time,
        };
    }).filter((row) => {
        const resolved = resolveBillingPct(account, row.id);
        return resolved.hasData || resolved.percentage > 0;
    });
}

/**
 * Models list for UI: percentage from local ledger when available.
 * (Per-model detail / auxiliary — not the primary list view.)
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
    // Prefer split-slot billing % so Pro/Flash read the same gemini bucket
    const splits = getSplitQuotaDisplays(account);
    const slotId: SplitQuotaSlotId | null =
        category === 'gemini-pro'
            ? 'gemini-pro'
            : category === 'gemini-flash'
              ? 'gemini-flash'
              : category === 'claude'
                ? 'claude'
                : category === 'gemini-flash-image' || category === 'gemini-pro-image'
                  ? 'gemini-image'
                  : null;
    if (slotId) {
        const slot = splits.find((s) => s.id === slotId);
        if (slot) {
            return {
                name: slot.id,
                percentage: slot.percentage,
                reset_time: slot.reset_time || '',
                officialPercentage: slot.officialPercentage,
            };
        }
    }
    return findQuotaModel(getDisplayQuotaModels(account), category);
}

export function findDisplayImageQuotaModel(
    account: Account | null | undefined,
): DisplayModelQuota | undefined {
    const splits = getSplitQuotaDisplays(account);
    const slot = splits.find((s) => s.id === 'gemini-image');
    if (slot) {
        return {
            name: slot.id,
            percentage: slot.percentage,
            reset_time: slot.reset_time || '',
            officialPercentage: slot.officialPercentage,
        };
    }
    return findImageQuotaModel(getDisplayQuotaModels(account));
}
