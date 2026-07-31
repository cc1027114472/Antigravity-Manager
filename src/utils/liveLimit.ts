import { Account, LiveLimitStatus } from '../types/account';
import { normalizeToBillingGroup } from './modelCategory';

function pickLatest(a: LiveLimitStatus, b: LiveLimitStatus): LiveLimitStatus {
    return a.until >= b.until ? a : b;
}

export function getLiveLimitForModel(
    account: Account,
    modelId?: string,
    protectedKey?: string
): LiveLimitStatus | undefined {
    const map = account.live_limited_models;
    if (!map) return undefined;

    if (modelId && map[modelId]) {
        return map[modelId];
    }

    const lowerId = modelId?.toLowerCase();
    if (lowerId) {
        for (const [key, value] of Object.entries(map)) {
            if (key.toLowerCase() === lowerId) return value;
        }
    }

    if (protectedKey && map[protectedKey]) {
        return map[protectedKey];
    }

    const billing =
        (protectedKey && normalizeToBillingGroup(protectedKey)) ||
        (modelId ? normalizeToBillingGroup(modelId) : null);

    if (!billing) return undefined;

    let best: LiveLimitStatus | undefined;
    for (const [key, value] of Object.entries(map)) {
        if (normalizeToBillingGroup(key) === billing) {
            best = best ? pickLatest(best, value) : value;
        }
    }
    return best;
}

export interface LiveLimitState {
    shouldShow: boolean;
    isActive: boolean;
    secondsRemaining: number;
    secondsAgo: number;
}

export function getLiveLimitState(liveLimit?: LiveLimitStatus): LiveLimitState {
    if (!liveLimit) {
        return {
            shouldShow: false,
            isActive: false,
            secondsRemaining: 0,
            secondsAgo: 0,
        };
    }

    const now = Math.floor(Date.now() / 1000);
    const secondsRemaining = Math.max(0, liveLimit.until - now);
    const secondsAgo = Math.max(0, now - liveLimit.detected_at);

    const isActive = secondsRemaining > 0;
    const shouldShow = isActive || secondsAgo < 600; // 10 minutes

    return {
        shouldShow,
        isActive,
        secondsRemaining,
        secondsAgo,
    };
}

export function formatCompactDuration(seconds: number): string {
    if (seconds <= 0) return '0s';

    const h = Math.floor(seconds / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    const s = Math.floor(seconds % 60);

    if (h > 0) {
        return `${h}h ${m}m`;
    }
    if (m > 0) {
        return `${m}m ${s}s`;
    }
    return `${s}s`;
}
