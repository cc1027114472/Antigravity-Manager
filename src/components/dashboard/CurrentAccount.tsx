import { CheckCircle, Mail, Diamond, Gem, Circle, Tag, Lock } from 'lucide-react';
import { Account } from '../../types/account';
import { formatTimeRemaining } from '../../utils/format';
import { formatQuotaTooltip, getBillingGroupDisplays } from '../../utils/quotaDisplay';
import { useTranslation } from 'react-i18next';

interface CurrentAccountProps {
    account: Account | null;
    onSwitch?: () => void;
}

function CurrentAccount({ account, onSwitch }: CurrentAccountProps) {
    const { t } = useTranslation();
    if (!account) {
        return (
            <div className="bg-white dark:bg-base-100 rounded-xl p-4 shadow-sm border border-gray-100 dark:border-base-200">
                <h2 className="text-base font-semibold text-gray-900 dark:text-base-content mb-2 flex items-center gap-2">
                    <CheckCircle className="w-4 h-4 text-green-500" />
                    {t('dashboard.current_account')}
                </h2>
                <div className="text-center py-4 text-gray-400 dark:text-gray-500 text-sm">
                    {t('dashboard.no_active_account')}
                </div>
            </div>
        );
    }

    const billingRows = getBillingGroupDisplays(account);

    return (
        <div className="bg-white dark:bg-base-100 rounded-xl p-4 shadow-sm border border-gray-100 dark:border-base-200 h-full flex flex-col">
            <h2 className="text-base font-semibold text-gray-900 dark:text-base-content mb-3 flex items-center gap-2">
                <CheckCircle className="w-4 h-4 text-green-500" />
                {t('dashboard.current_account')}
            </h2>

            <div className="space-y-4 flex-1">
                <div className="flex items-center gap-3 mb-1">
                    <div className="flex items-center gap-2 flex-1 min-w-0">
                        <Mail className="w-3.5 h-3.5 text-gray-400" />
                        <span className="text-sm font-medium text-gray-700 dark:text-gray-300 truncate">{account.email}</span>
                    </div>
                    {account.quota?.subscription_tier && (() => {
                        const tier = account.quota.subscription_tier.toLowerCase();
                        if (tier.includes('ultra')) {
                            return (
                                <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-gradient-to-r from-purple-600 to-pink-600 text-white text-[10px] font-bold shadow-sm shrink-0">
                                    <Gem className="w-2.5 h-2.5 fill-current" />
                                    ULTRA
                                </span>
                            );
                        } else if (tier.includes('pro')) {
                            return (
                                <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-gradient-to-r from-blue-600 to-indigo-600 text-white text-[10px] font-bold shadow-sm shrink-0">
                                    <Diamond className="w-2.5 h-2.5 fill-current" />
                                    PRO
                                </span>
                            );
                        } else {
                            return (
                                <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-gray-100 dark:bg-white/10 text-gray-500 dark:text-gray-400 text-[10px] font-bold shadow-sm border border-gray-200 dark:border-white/10 shrink-0">
                                    <Circle className="w-2.5 h-2.5" />
                                    FREE
                                </span>
                            );
                        }
                    })()}
                    {account.custom_label && (
                        <span className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-orange-100 dark:bg-orange-900/30 text-orange-600 dark:text-orange-400 text-[10px] font-bold shadow-sm shrink-0">
                            <Tag className="w-2.5 h-2.5" />
                            {account.custom_label}
                        </span>
                    )}
                </div>

                {billingRows.map((row) => (
                    <div key={row.id} className="space-y-1.5">
                        <div className="flex justify-between items-baseline">
                            <span className="text-xs font-medium text-gray-600 dark:text-gray-400 flex items-center gap-1">
                                {account.protected_models?.includes(row.id) && (
                                    <Lock className="w-2.5 h-2.5 text-rose-500" />
                                )}
                                {row.label}
                            </span>
                            <div className="flex items-center gap-2">
                                <span className="text-[10px] text-gray-400 dark:text-gray-500">
                                    {row.reset_time
                                        ? `R: ${formatTimeRemaining(row.reset_time)}`
                                        : t('common.unknown')}
                                </span>
                                <span
                                    className={`text-xs font-bold ${
                                        row.percentage >= 50
                                            ? 'text-emerald-600 dark:text-emerald-400'
                                            : row.percentage >= 20
                                              ? 'text-amber-600 dark:text-amber-400'
                                              : 'text-rose-600 dark:text-rose-400'
                                    }`}
                                    title={formatQuotaTooltip(row.percentage, row.officialPercentage)}
                                >
                                    {row.percentage}%
                                </span>
                            </div>
                        </div>
                        <div className="w-full bg-gray-100 dark:bg-base-300 rounded-full h-1.5 overflow-hidden">
                            <div
                                className={`h-full rounded-full transition-all duration-700 ${
                                    row.percentage >= 50
                                        ? 'bg-gradient-to-r from-emerald-400 to-emerald-500'
                                        : row.percentage >= 20
                                          ? 'bg-gradient-to-r from-amber-400 to-amber-500'
                                          : 'bg-gradient-to-r from-rose-400 to-rose-500'
                                }`}
                                style={{ width: `${row.percentage}%` }}
                            />
                        </div>
                    </div>
                ))}
            </div>

            {onSwitch && (
                <button
                    onClick={onSwitch}
                    className="mt-4 w-full py-2 text-xs font-medium text-blue-600 dark:text-blue-400 hover:bg-blue-50 dark:hover:bg-blue-900/20 rounded-lg transition-colors"
                >
                    {t('dashboard.switch_account')}
                </button>
            )}
        </div>
    );
}

export default CurrentAccount;
