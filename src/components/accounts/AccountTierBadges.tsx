import React from 'react';
import { useTranslation } from 'react-i18next';
import { Coffee, Crown, Flame, Sun, Snowflake } from 'lucide-react';
import type { Account } from '../../types/account';
import { cn } from '../../utils/cn';

export function formatFatigueTime(seconds: number): string {
    if (seconds <= 60) return '< 1m';
    if (seconds < 3600) return `${Math.ceil(seconds / 60)}m`;
    const h = Math.floor(seconds / 3600);
    const m = Math.ceil((seconds % 3600) / 60);
    return m > 0 ? `${h}h ${m}m` : `${h}h`;
}

export function getFatigueReasonLabel(
    reason: string | undefined,
    t: (key: string, options?: any) => string,
): string {
    switch (reason) {
        case 'HourlyOutputLimitExceeded':
            return t('accounts.fatigue_reason_hourly');
        case 'DailyOutputLimitExceeded':
            return t('accounts.fatigue_reason_daily');
        case 'BothLimitsExceeded':
            return t('accounts.fatigue_reason_both');
        default:
            return reason || t('accounts.fatigue_reason_hourly');
    }
}

interface AccountTierBadgesProps {
    account: Account;
    isDisabled?: boolean;
    compact?: boolean;
}

export const AccountTierBadges: React.FC<AccountTierBadgesProps> = ({
    account,
    isDisabled = false,
    compact = false,
}) => {
    const { t } = useTranslation();

    if (isDisabled || account.proxy_disabled) {
        return null;
    }

    const isFatigued =
        account.fatigue_remaining_seconds != null && account.fatigue_remaining_seconds > 0;
    const tier = account.capacity_tier;

    return (
        <>
            {/* 软上限避峰休眠徽章 */}
            {isFatigued && (
                <span
                    className={cn(
                        'rounded-md font-bold flex items-center gap-1 shadow-sm border select-none transition-transform hover:scale-105',
                        'bg-purple-100 dark:bg-purple-950/60 text-purple-700 dark:text-purple-300 border-purple-200/60 dark:border-purple-800/60',
                        compact ? 'px-1.5 py-0.5 text-[9px]' : 'px-2 py-0.5 text-[10px]',
                    )}
                    title={t('accounts.fatigue_cooldown_tooltip', {
                        time: formatFatigueTime(account.fatigue_remaining_seconds!),
                        strikes: account.fatigue_strikes || 1,
                        reason: getFatigueReasonLabel(account.fatigue_reason, t),
                    })}
                >
                    <Coffee className={compact ? 'w-2.5 h-2.5 shrink-0' : 'w-2.5 h-2.5 shrink-0'} />
                    <span>
                        {t('accounts.fatigue_cooldown', {
                            time: formatFatigueTime(account.fatigue_remaining_seconds!),
                        })}
                    </span>
                </span>
            )}

            {/* 慢启动容量梯队徽章 */}
            {tier && (() => {
                switch (tier) {
                    case 'Tier3SweetSpot':
                        return (
                            <span
                                className={cn(
                                    'rounded-md font-bold flex items-center gap-1 shadow-sm border select-none transition-transform hover:scale-105',
                                    'bg-emerald-100 dark:bg-emerald-950/50 text-emerald-700 dark:text-emerald-300 border-emerald-200/60 dark:border-emerald-800/60',
                                    compact ? 'px-1.5 py-0.5 text-[9px]' : 'px-2 py-0.5 text-[10px]',
                                )}
                                title={t('accounts.capacity_tier_sweet_spot_tooltip')}
                            >
                                <Crown className="w-2.5 h-2.5 shrink-0 text-amber-500 fill-amber-400" />
                                <span>{t('accounts.capacity_tier_sweet_spot')}</span>
                            </span>
                        );
                    case 'Tier3Overheated':
                        return (
                            <span
                                className={cn(
                                    'rounded-md font-bold flex items-center gap-1 shadow-sm border select-none transition-transform hover:scale-105',
                                    'bg-amber-100 dark:bg-amber-950/50 text-amber-800 dark:text-amber-300 border-amber-200/60 dark:border-amber-800/60',
                                    compact ? 'px-1.5 py-0.5 text-[9px]' : 'px-2 py-0.5 text-[10px]',
                                )}
                                title={t('accounts.capacity_tier_overheated_tooltip')}
                            >
                                <Flame className="w-2.5 h-2.5 shrink-0 text-amber-600 dark:text-amber-400 fill-amber-500/30" />
                                <span>{t('accounts.capacity_tier_overheated')}</span>
                            </span>
                        );
                    case 'Tier2Warming':
                        return (
                            <span
                                className={cn(
                                    'rounded-md font-bold flex items-center gap-1 shadow-sm border select-none transition-transform hover:scale-105',
                                    'bg-sky-100 dark:bg-sky-950/50 text-sky-700 dark:text-sky-300 border-sky-200/60 dark:border-sky-800/60',
                                    compact ? 'px-1.5 py-0.5 text-[9px]' : 'px-2 py-0.5 text-[10px]',
                                )}
                                title={t('accounts.capacity_tier_warming_tooltip')}
                            >
                                <Sun className="w-2.5 h-2.5 shrink-0 text-amber-500" />
                                <span>{t('accounts.capacity_tier_warming')}</span>
                            </span>
                        );
                    case 'Tier1Cold':
                        return (
                            <span
                                className={cn(
                                    'rounded-md font-bold flex items-center gap-1 shadow-sm border select-none transition-transform hover:scale-105',
                                    'bg-slate-100 dark:bg-slate-800/70 text-slate-600 dark:text-slate-300 border-slate-200/60 dark:border-slate-700/60',
                                    compact ? 'px-1.5 py-0.5 text-[9px]' : 'px-2 py-0.5 text-[10px]',
                                )}
                                title={t('accounts.capacity_tier_cold_tooltip')}
                            >
                                <Snowflake className="w-2.5 h-2.5 shrink-0 text-sky-500" />
                                <span>{t('accounts.capacity_tier_cold')}</span>
                            </span>
                        );
                    default:
                        return null;
                }
            })()}
        </>
    );
};
