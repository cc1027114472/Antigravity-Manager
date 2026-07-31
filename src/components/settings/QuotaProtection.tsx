import { Shield, Check } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { QuotaProtectionConfig } from '../../types/config';
import { Gemini, Claude } from '@lobehub/icons';

interface QuotaProtectionProps {
    config: QuotaProtectionConfig;
    onChange: (config: QuotaProtectionConfig) => void;
}

const BILLING_OPTIONS = [
    { id: 'gemini', label: 'Gemini', Icon: Gemini.Color },
    { id: 'claude', label: 'Claude', Icon: Claude.Color },
] as const;

const QuotaProtection = ({ config, onChange }: QuotaProtectionProps) => {
    const { t } = useTranslation();

    const handleEnabledChange = (enabled: boolean) => {
        let newConfig = { ...config, enabled };
        if (enabled && (!config.monitored_models || config.monitored_models.length === 0)) {
            newConfig.monitored_models = ['claude', 'gemini'];
        }
        onChange(newConfig);
    };

    const handlePercentageChange = (value: string) => {
        const percentage = parseInt(value) || 10;
        const clampedPercentage = Math.max(1, Math.min(99, percentage));
        onChange({ ...config, threshold_percentage: clampedPercentage });
    };

    const toggleModel = (model: string) => {
        const currentModels = config.monitored_models || [];
        let newModels: string[];

        if (currentModels.includes(model)) {
            if (currentModels.length <= 1) return;
            newModels = currentModels.filter(m => m !== model);
        } else {
            newModels = [...currentModels, model];
        }

        onChange({ ...config, monitored_models: newModels });
    };

    const exampleTotal = 150;
    const exampleThreshold = Math.floor(exampleTotal * config.threshold_percentage / 100);

    return (
        <div className="animate-in fade-in duration-500">
            <div className="flex items-center justify-between">
                <div className="flex items-center gap-4">
                    <div className="w-10 h-10 rounded-xl bg-rose-50 dark:bg-rose-900/20 flex items-center justify-center text-rose-500 group-hover:bg-rose-500 group-hover:text-white transition-all duration-300">
                        <Shield size={20} />
                    </div>
                    <div>
                        <div className="font-bold text-gray-900 dark:text-gray-100">
                            {t('settings.quota_protection.title')}
                        </div>
                        <p className="text-xs text-gray-500 dark:text-gray-400 mt-0.5">
                            {t('settings.quota_protection.enable_desc')}
                        </p>
                    </div>
                </div>

                <input
                    type="checkbox"
                    className="toggle toggle-error toggle-sm"
                    checked={config.enabled}
                    onChange={(e) => handleEnabledChange(e.target.checked)}
                />
            </div>

            {config.enabled && (
                <div className="mt-6 space-y-5 pl-14">
                    <div>
                        <label className="text-sm font-medium text-gray-700 dark:text-gray-300">
                            {t('settings.quota_protection.threshold_label')}
                        </label>
                        <div className="mt-2 flex items-center gap-3">
                            <input
                                type="range"
                                min={1}
                                max={99}
                                className="range range-error range-xs flex-1"
                                value={config.threshold_percentage}
                                onChange={(e) => handlePercentageChange(e.target.value)}
                            />
                            <span className="text-sm font-mono w-12 text-right">{config.threshold_percentage}%</span>
                        </div>
                    </div>

                    <div>
                        <label className="text-sm font-medium text-gray-700 dark:text-gray-300">
                            {t('settings.quota_protection.monitored_models_label')}
                        </label>
                        <p className="text-xs text-gray-500 dark:text-gray-400 mt-0.5 mb-3">
                            {t('settings.quota_protection.monitored_models_desc')}
                        </p>
                        <div className="flex flex-wrap gap-2">
                            {BILLING_OPTIONS.map(({ id, label, Icon }) => {
                                const isSelected = config.monitored_models?.includes(id);
                                return (
                                    <button
                                        key={id}
                                        type="button"
                                        onClick={() => toggleModel(id)}
                                        className={`flex items-center gap-2 px-3 py-2 rounded-xl border text-sm font-medium transition-all ${
                                            isSelected
                                                ? 'border-rose-400 bg-rose-50 text-rose-700 dark:bg-rose-900/30 dark:text-rose-300 dark:border-rose-700'
                                                : 'border-gray-200 dark:border-white/10 text-gray-600 dark:text-gray-400 hover:border-gray-300'
                                        }`}
                                    >
                                        <Icon size={16} />
                                        {label}
                                        {isSelected && <Check size={14} className="text-rose-500" />}
                                    </button>
                                );
                            })}
                        </div>
                    </div>

                    <div className="text-xs text-gray-500 dark:text-gray-400 space-y-1 bg-gray-50 dark:bg-white/5 p-3 rounded-xl">
                        <p>
                            {t('settings.quota_protection.example', {
                                percentage: config.threshold_percentage,
                                total: exampleTotal,
                                reserve: exampleThreshold,
                            })}
                        </p>
                        <p className="text-emerald-600 dark:text-emerald-400">
                            ✓ {t('settings.quota_protection.auto_restore_info')}
                        </p>
                    </div>
                </div>
            )}
        </div>
    );
};

export default QuotaProtection;
