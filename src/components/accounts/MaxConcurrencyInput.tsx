import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '../../utils/cn';

interface MaxConcurrencyInputProps {
    value?: number | null;
    onSave: (value: number | null) => void | Promise<void>;
    className?: string;
}

/** Compact override: empty/0 = inherit global; >0 = per-account cap. */
export function MaxConcurrencyInput({ value, onSave, className }: MaxConcurrencyInputProps) {
    const { t } = useTranslation();
    const [draft, setDraft] = useState(value && value > 0 ? String(value) : '');
    const [saving, setSaving] = useState(false);

    useEffect(() => {
        setDraft(value && value > 0 ? String(value) : '');
    }, [value]);

    const commit = async () => {
        const parsed = draft.trim() === '' ? null : Number.parseInt(draft, 10);
        const cleaned =
            parsed !== null && !Number.isNaN(parsed) && parsed > 0 ? parsed : null;
        const current = value && value > 0 ? value : null;
        if (cleaned === current) {
            setDraft(cleaned ? String(cleaned) : '');
            return;
        }
        setSaving(true);
        try {
            await onSave(cleaned);
            setDraft(cleaned ? String(cleaned) : '');
        } catch {
            setDraft(value && value > 0 ? String(value) : '');
        } finally {
            setSaving(false);
        }
    };

    return (
        <input
            type="number"
            min={0}
            className={cn(
                'w-12 px-1 py-0.5 text-[10px] font-mono text-center border border-gray-200 dark:border-base-300 rounded bg-white dark:bg-base-200 text-gray-700 dark:text-gray-300 focus:outline-none focus:ring-1 focus:ring-blue-500 disabled:opacity-50',
                className
            )}
            placeholder={t('accounts.concurrency.inherit')}
            title={t('accounts.concurrency.tooltip')}
            aria-label={t('accounts.concurrency.label')}
            value={draft}
            disabled={saving}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => setDraft(e.target.value)}
            onBlur={() => void commit()}
            onKeyDown={(e) => {
                if (e.key === 'Enter') {
                    e.currentTarget.blur();
                } else if (e.key === 'Escape') {
                    setDraft(value && value > 0 ? String(value) : '');
                    e.currentTarget.blur();
                }
            }}
        />
    );
}
