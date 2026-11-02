import { useTranslation } from "react-i18next";
import { ShieldCheck, HelpCircle } from "lucide-react";
import { PayloadGuardConfig } from "../../types/config";

interface PayloadGuardSettingsProps {
    config?: PayloadGuardConfig;
    onChange: (config: PayloadGuardConfig) => void;
}

export default function PayloadGuardSettings({
    config,
    onChange,
}: PayloadGuardSettingsProps) {
    const { t } = useTranslation();

    const currentConfig: PayloadGuardConfig = {
        enabled: config?.enabled ?? true,
        max_input_tokens: config?.max_input_tokens ?? 150000,
        bytes_per_token_ceiling: config?.bytes_per_token_ceiling ?? 4.0,
    };

    const PRESETS = [
        { label: "100k", value: 100000 },
        { label: "150k", value: 150000 },
        { label: "200k", value: 200000 },
        { label: "300k", value: 300000 },
        { label: t("common.unlimited", { defaultValue: "不限" }), value: 0 },
    ];

    const handleToggle = (enabled: boolean) => {
        onChange({
            ...currentConfig,
            enabled,
        });
    };

    const handleMaxTokensChange = (val: number) => {
        onChange({
            ...currentConfig,
            max_input_tokens: Math.max(0, val),
        });
    };

    return (
        <div className="bg-slate-100 dark:bg-slate-800/80 rounded-xl p-4 border border-slate-200 dark:border-slate-700">
            <div className="flex items-center justify-between mb-3 gap-3">
                <div className="flex items-center gap-2">
                    <ShieldCheck className="w-4 h-4 text-emerald-500 shrink-0" />
                    <div>
                        <label className="text-xs font-semibold text-gray-800 dark:text-gray-200 inline-flex items-center gap-1.5 cursor-pointer">
                            {t("proxy.config.payload_guard.title", { defaultValue: "请求上下文大小防护 (Payload Guard)" })}
                            <span
                                className="tooltip tooltip-right"
                                data-tip={t("proxy.config.payload_guard.tooltip", {
                                    defaultValue: "在请求进入账号前计算 Token 数量。超限请求直接返回 413 拦截，避免大 Prompt 突刺打穿账号或触发 Google TOS 封号。",
                                })}
                            >
                                <HelpCircle size={13} className="text-gray-400 hover:text-gray-500 cursor-pointer" />
                            </span>
                        </label>
                        <p className="text-[11px] text-gray-500 dark:text-gray-400 mt-0.5">
                            {t("proxy.config.payload_guard.description", {
                                defaultValue: "单次请求最大输入限制，修改后即时热生效，无需重启服务。",
                            })}
                        </p>
                    </div>
                </div>

                <input
                    type="checkbox"
                    className="toggle toggle-sm toggle-success"
                    checked={currentConfig.enabled}
                    onChange={(e) => handleToggle(e.target.checked)}
                />
            </div>

            {currentConfig.enabled && (
                <div className="mt-3 pt-3 border-t border-slate-200/60 dark:border-slate-700/60 flex flex-wrap items-center justify-between gap-3">
                    <div className="flex items-center gap-2">
                        <span className="text-xs text-gray-600 dark:text-gray-300 font-medium">
                            {t("proxy.config.payload_guard.max_tokens", { defaultValue: "最大输入限制 (Tokens)" })}:
                        </span>
                        <div className="flex items-center gap-1.5">
                            <input
                                type="number"
                                min={0}
                                step={10000}
                                className="input input-bordered input-xs w-28 font-mono text-center font-bold text-emerald-600 dark:text-emerald-400"
                                value={currentConfig.max_input_tokens}
                                onChange={(e) => {
                                    const val = parseInt(e.target.value, 10);
                                    handleMaxTokensChange(isNaN(val) ? 0 : val);
                                }}
                            />
                            <span className="text-xs text-gray-400 font-mono">Tokens</span>
                        </div>
                    </div>

                    <div className="flex items-center gap-1">
                        {PRESETS.map((p) => {
                            const isSelected = currentConfig.max_input_tokens === p.value;
                            return (
                                <button
                                    key={p.value}
                                    type="button"
                                    onClick={() => handleMaxTokensChange(p.value)}
                                    className={`btn btn-xs px-2.5 font-mono text-[11px] ${
                                        isSelected
                                            ? "btn-success text-white shadow-sm"
                                            : "btn-ghost bg-white/70 dark:bg-slate-900/50 border border-slate-200 dark:border-slate-700 text-gray-600 dark:text-gray-300"
                                    }`}
                                >
                                    {p.label}
                                </button>
                            );
                        })}
                    </div>
                </div>
            )}
        </div>
    );
}
