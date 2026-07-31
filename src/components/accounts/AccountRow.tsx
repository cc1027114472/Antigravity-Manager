import { ArrowRightLeft, RefreshCw, Trash2, Download, Info, Lock, Ban, Diamond, Gem, Circle, ToggleLeft, ToggleRight, Fingerprint } from 'lucide-react';
import { Account } from '../../types/account';
import { cn } from '../../utils/cn';
import { useTranslation } from 'react-i18next';
import { formatQuotaTooltip, getListQuotaDisplays } from '../../utils/quotaDisplay';
import { getLiveLimitForModel } from '../../utils/liveLimit';
import { useConfigStore } from '../../stores/useConfigStore';
import { QuotaItem } from './QuotaItem';
import { Gemini, Claude } from '@lobehub/icons';
import { MODEL_CONFIG } from '../../config/modelConfig';

interface AccountRowProps {
    account: Account;
    selected: boolean;
    onSelect: () => void;
    isCurrent: boolean;
    isRefreshing: boolean;
    isSwitching?: boolean;
    onSwitch: () => void;
    onRefresh: () => void;
    onViewDevice: () => void;
    onViewDetails: () => void;
    onExport: () => void;
    onDelete: () => void;
    onToggleProxy: () => void;
}

function AccountRow({ account, selected, onSelect, isCurrent, isRefreshing, isSwitching = false, onSwitch, onRefresh, onViewDetails, onExport, onDelete, onToggleProxy, onViewDevice }: AccountRowProps) {
    const { t } = useTranslation();
    const { config, showAllQuotas } = useConfigStore();
    const displayModels = getListQuotaDisplays(account, {
        showAll: showAllQuotas,
        pinnedModels: config?.pinned_quota_models?.models,
    });
    const isDisabled = Boolean(account.disabled);

    return (
        <tr className={cn(
            "group hover:bg-gray-50 dark:hover:bg-base-200 transition-colors border-b border-gray-100 dark:border-base-200",
            isCurrent && "bg-blue-50/50 dark:bg-blue-900/10",
            (isRefreshing || isDisabled) && "opacity-70"
        )}>
            <td className="pl-6 py-1 w-12">
                <input
                    type="checkbox"
                    className="checkbox checkbox-xs rounded border-2 border-gray-400 dark:border-gray-500 checked:border-blue-600 checked:bg-blue-600 [--chkbg:theme(colors.blue.600)] [--chkfg:white]"
                    checked={selected}
                    onChange={() => onSelect()}
                    onClick={(e) => e.stopPropagation()}
                />
            </td>

            <td className="px-4 py-1">
                <div className="flex items-center gap-3">
                    <span className={cn(
                        "font-medium text-sm truncate max-w-[180px] xl:max-w-none transition-colors",
                        isCurrent ? "text-blue-700 dark:text-blue-400" : "text-gray-900 dark:text-base-content"
                    )} title={account.email}>
                        {account.email}
                    </span>

                    <div className="flex items-center gap-1.5 shrink-0">
                        {isCurrent && (
                            <span className="px-2 py-0.5 rounded-md bg-blue-100 dark:bg-blue-900/50 text-blue-700 dark:text-blue-300 text-[10px] font-bold shadow-sm border border-blue-200/50 dark:border-blue-800/50">
                                {t('accounts.current').toUpperCase()}
                            </span>
                        )}

                        {isDisabled && (
                            <span
                                className="px-2 py-0.5 rounded-md bg-rose-100 dark:bg-rose-900/50 text-rose-700 dark:text-rose-300 text-[10px] font-bold flex items-center gap-1 shadow-sm border border-rose-200/50"
                                title={account.disabled_reason || t('accounts.disabled_tooltip')}
                            >
                                <Ban className="w-2.5 h-2.5" />
                                <span>{t('accounts.disabled')}</span>
                            </span>
                        )}

                        {account.proxy_disabled && (
                            <span
                                className="px-2 py-0.5 rounded-md bg-orange-100 dark:bg-orange-900/50 text-orange-700 dark:text-orange-300 text-[10px] font-bold flex items-center gap-1 shadow-sm border border-orange-200/50"
                                title={account.proxy_disabled_reason || t('accounts.proxy_disabled_tooltip')}
                            >
                                <Ban className="w-2.5 h-2.5" />
                                <span>{t('accounts.proxy_disabled')}</span>
                            </span>
                        )}

                        {account.quota?.is_forbidden && (
                            <span className="px-2 py-0.5 rounded-md bg-red-100 dark:bg-red-900/50 text-red-600 dark:text-red-400 text-[10px] font-bold flex items-center gap-1 shadow-sm border border-red-200/50" title={t('accounts.forbidden_tooltip')}>
                                <Lock className="w-2.5 h-2.5" />
                                <span>{t('accounts.forbidden')}</span>
                            </span>
                        )}

                        {account.quota?.subscription_tier && (() => {
                            const tier = account.quota.subscription_tier.toLowerCase();
                            if (tier.includes('ultra')) {
                                return (
                                    <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-gradient-to-r from-purple-600 to-pink-600 text-white text-[10px] font-bold shadow-sm hover:scale-105 transition-transform cursor-default">
                                        <Gem className="w-2.5 h-2.5 fill-current" />
                                        ULTRA
                                    </span>
                                );
                            } else if (tier.includes('pro')) {
                                return (
                                    <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-gradient-to-r from-blue-600 to-indigo-600 text-white text-[10px] font-bold shadow-sm hover:scale-105 transition-transform cursor-default">
                                        <Diamond className="w-2.5 h-2.5 fill-current" />
                                        PRO
                                    </span>
                                );
                            } else {
                                return (
                                    <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-gray-100 dark:bg-white/10 text-gray-600 dark:text-gray-400 text-[10px] font-bold shadow-sm border border-gray-200 dark:border-white/10 hover:bg-gray-200 transition-colors cursor-default">
                                        <Circle className="w-2.5 h-2.5" />
                                        FREE
                                    </span>
                                );
                            }
                        })()}
                    </div>
                </div>
            </td>

            <td className="px-4 py-1">
                {account.quota?.is_forbidden ? (
                    <div className="flex items-center gap-2 text-xs text-red-500 dark:text-red-400 bg-red-50/50 dark:bg-red-900/10 p-1.5 rounded-lg border border-red-100 dark:border-red-900/30">
                        <Ban className="w-4 h-4 shrink-0" />
                        <span>{t('accounts.forbidden_msg')}</span>
                    </div>
                ) : (
                    <div className={cn(
                        "grid gap-x-4 gap-y-1 py-0",
                        displayModels.length === 1 ? "grid-cols-1" : "grid-cols-2"
                    )}>
                        {displayModels.map((row) => (
                            <QuotaItem
                                key={row.id}
                                label={row.label}
                                percentage={row.percentage}
                                resetTime={row.reset_time}
                                isProtected={account.protected_models?.includes(row.protectedKey)}
                                liveLimit={getLiveLimitForModel(account, row.id, row.protectedKey)}
                                Icon={
                                    MODEL_CONFIG[row.id]?.Icon ||
                                    (row.protectedKey === 'claude' ? Claude.Color : Gemini.Color)
                                }
                                quotaTitle={formatQuotaTooltip(row.percentage, row.officialPercentage)}
                            />
                        ))}
                    </div>
                )}
            </td>

            <td className="px-4 py-1">
                <div className="flex flex-col">
                    <span className="text-xs font-medium text-gray-600 dark:text-gray-400 font-mono whitespace-nowrap">
                        {new Date(account.last_used * 1000).toLocaleDateString()}
                    </span>
                    <span className="text-[10px] text-gray-400 dark:text-gray-500 font-mono whitespace-nowrap leading-tight">
                        {new Date(account.last_used * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}
                    </span>
                </div>
            </td>

            <td className="px-2 py-1 sticky right-0 bg-inherit">
                <div className="flex items-center justify-center gap-0.5 opacity-60 group-hover:opacity-100 transition-opacity">
                    <button className="btn btn-ghost btn-xs btn-square" onClick={onSwitch} disabled={isSwitching} title={t('accounts.switch')}>
                        <ArrowRightLeft className="w-3.5 h-3.5" />
                    </button>
                    <button className="btn btn-ghost btn-xs btn-square" onClick={onRefresh} disabled={isRefreshing} title={t('accounts.refresh')}>
                        <RefreshCw className={cn("w-3.5 h-3.5", isRefreshing && "animate-spin")} />
                    </button>
                    <button className="btn btn-ghost btn-xs btn-square" onClick={onViewDetails} title={t('accounts.details')}>
                        <Info className="w-3.5 h-3.5" />
                    </button>
                    <button className="btn btn-ghost btn-xs btn-square" onClick={onViewDevice} title={t('accounts.device')}>
                        <Fingerprint className="w-3.5 h-3.5" />
                    </button>
                    <button className="btn btn-ghost btn-xs btn-square" onClick={onExport} title={t('accounts.export')}>
                        <Download className="w-3.5 h-3.5" />
                    </button>
                    <button className="btn btn-ghost btn-xs btn-square" onClick={onToggleProxy} title={t('accounts.toggle_proxy')}>
                        {account.proxy_disabled ? <ToggleLeft className="w-3.5 h-3.5" /> : <ToggleRight className="w-3.5 h-3.5" />}
                    </button>
                    <button className="btn btn-ghost btn-xs btn-square text-rose-500" onClick={onDelete} title={t('accounts.delete')}>
                        <Trash2 className="w-3.5 h-3.5" />
                    </button>
                </div>
            </td>
        </tr>
    );
}

export default AccountRow;
