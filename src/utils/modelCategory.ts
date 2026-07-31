/**
 * 模型分类工具函数（无 React / icons 依赖，可在 Node 环境直接导入）
 */

export type ModelCategory = 'gemini-pro' | 'gemini-flash' | 'gemini-pro-image' | 'gemini-flash-image' | 'claude' | 'other';

/** Official Google billing big buckets */
export type BillingGroup = 'gemini' | 'claude';

export const BILLING_GROUPS: BillingGroup[] = ['gemini', 'claude'];

export function categorizeModel(name: string): ModelCategory {
    const n = name.trim().toLowerCase();
    if (n === 'gemini') return 'gemini-flash';
    if (n === 'claude' || n === '3p') return 'claude';
    const isGemini = n.startsWith('gemini-') || n.startsWith('gemini');
    const isImage = (isGemini && n.includes('image')) || n.startsWith('image') || n.startsWith('imagen');
    if (isImage) return n.includes('flash') ? 'gemini-flash-image' : 'gemini-pro-image';
    if (isGemini && n.includes('flash')) return 'gemini-flash';
    if (isGemini && n.includes('pro')) return 'gemini-pro';
    if (n.includes('claude') || n.includes('opus') || n.includes('sonnet') || n.includes('haiku')) return 'claude';
    if (n.includes('gpt') || n.startsWith('o1') || n.startsWith('o3')) return 'claude';
    return 'other';
}

/** Map any model / legacy std id to official billing group. */
export function normalizeToBillingGroup(name: string): BillingGroup | null {
    const n = name.trim().toLowerCase();
    if (!n) return null;
    if (n === 'gemini' || n.startsWith('gemini')) return 'gemini';
    if (n === 'claude' || n === '3p') return 'claude';
    const cat = categorizeModel(n);
    if (cat === 'claude') return 'claude';
    if (cat !== 'other') return 'gemini';
    return null;
}

export interface ModelDisplayNameInput {
    name: string;
    display_name?: string;
}

export function getModelDisplayName(
    model: ModelDisplayNameInput | null | undefined,
    fallback?: string,
): string {
    if (model) {
        if (model.display_name) return model.display_name;
        if (model.name) return model.name;
    }
    return fallback ?? '';
}

/**
 * 按优先级查找配额模型：先精确匹配首选名，再按类别 fallback。
 */
export function findQuotaModel<T extends { name: string }>(
    models: T[] | undefined,
    category: ModelCategory,
): T | undefined {
    if (!models || models.length === 0) return undefined;
    const preferred: Partial<Record<ModelCategory, string[]>> = {
        'gemini-pro': ['gemini-pro-agent', 'gemini-3.1-pro-high', 'gemini-3.1-pro', 'gemini-3.1-pro-low', 'gemini-2.5-pro'],
        'gemini-flash': ['gemini-3-flash-agent', 'gemini-3-flash', 'gemini-3.5-flash', 'gemini'],
        'claude': ['claude-sonnet-4-6', 'claude-opus-4-6-thinking', 'claude'],
    };
    const names = preferred[category];
    if (names) {
        for (const name of names) {
            const found = models.find(m => m.name === name);
            if (found) return found;
        }
    }
    return models.find(m => categorizeModel(m.name) === category);
}

/** Protection / ledger key = official billing group. */
export function getModelProtectionKey(name: string): string | null {
    return normalizeToBillingGroup(name);
}

/**
 * 在任意图片类别中查找第一个实际模型。
 */
export function findImageQuotaModel<T extends { name: string }>(
    models: T[] | undefined,
): T | undefined {
    if (!models || models.length === 0) return undefined;
    return models.find(m => {
        const c = categorizeModel(m.name);
        return c === 'gemini-flash-image' || c === 'gemini-pro-image';
    });
}

/** @deprecated Pin list now uses billing groups; kept for compat. */
export const DEFAULT_IMAGE_PIN_SELECTOR = 'gemini';

export function ensurePinnedImageSelector(selectorIds: string[] | undefined): string[] {
    const pinned = selectorIds ? [...selectorIds] : [];
    const billingOnly = pinned
        .map(id => normalizeToBillingGroup(id))
        .filter((g): g is BillingGroup => !!g);
    if (billingOnly.length > 0) {
        return Array.from(new Set(billingOnly));
    }
    return ['gemini', 'claude'];
}

export interface QuotaModelSelection<T> {
    selectorId: string;
    selectionKey: string;
    model: T | undefined;
}

export function resolveQuotaModels<T extends { name: string }>(
    models: T[] | undefined,
    selectorIds: string[],
): QuotaModelSelection<T>[] {
    const seen = new Set<string>();
    const results: QuotaModelSelection<T>[] = [];

    for (const selectorId of selectorIds) {
        const normalizedId = selectorId.trim().toLowerCase();
        const billing = normalizeToBillingGroup(normalizedId);
        if (billing) {
            const selectionKey = `billing:${billing}`;
            if (seen.has(selectionKey)) continue;
            seen.add(selectionKey);
            const model =
                models?.find(m => m.name === billing)
                ?? models?.find(m => normalizeToBillingGroup(m.name) === billing);
            results.push({ selectorId: billing, selectionKey, model });
            continue;
        }

        const category = categorizeModel(normalizedId);
        const isImage = category === 'gemini-pro-image' || category === 'gemini-flash-image';
        const selectionKey = isImage
            ? 'billing:gemini'
            : category === 'other'
                ? `model:${normalizedId}`
                : `billing:${normalizeToBillingGroup(normalizedId) ?? category}`;

        if (seen.has(selectionKey)) continue;
        seen.add(selectionKey);

        const model = isImage
            ? findImageQuotaModel(models)
            : category === 'other'
                ? models?.find(m => m.name.trim().toLowerCase() === normalizedId)
                : findQuotaModel(models, category);

        results.push({
            selectorId: billing ?? selectorId,
            selectionKey,
            model,
        });
    }
    return results;
}
