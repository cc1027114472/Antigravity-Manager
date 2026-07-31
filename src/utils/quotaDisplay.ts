/**
 * Merge online quota snapshot with local estimated_quotas for UI display.
 * Primary bar % = local ledger when present; official % kept for tooltip.
 */
import type { Account, EstimatedModelQuota, ModelQuota } from '../types/account';
import {
    findImageQuotaModel,
    findQuotaModel,
    getModelProtectionKey,
    type ModelCategory,
} from './modelCategory';

export type DisplayModelQuota = ModelQuota & {
    officialPercentage?: number;
};

function lookupEstimated(
    estimated: Record<string, EstimatedModelQuota> | undefined,
    modelName: string,
): EstimatedModelQuota | undefined {
    if (!estimated) return undefined;
    const direct = estimated[modelName];
    if (direct) return direct;
    const key = getModelProtectionKey(modelName);
    if (key && estimated[key]) return estimated[key];
    const lower = modelName.toLowerCase();
    for (const [k, v] of Object.entries(estimated)) {
        if (k.toLowerCase() === lower) return v;
        if (key && k.toLowerCase() === key.toLowerCase()) return v;
    }
    return undefined;
}

/**
 * Models list for UI: percentage from local ledger when available.
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
