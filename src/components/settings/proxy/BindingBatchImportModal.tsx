import { useState } from 'react';
import { createPortal } from 'react-dom';
import { X, Upload, FileText } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../../utils/request';
import { showToast } from '../../common/ToastContainer';

interface BindingBatchImportModalProps {
    isOpen: boolean;
    onClose: () => void;
    onImported?: () => void;
}

type BindingRow = { accountId: string; proxyId: string };

function parseBindings(text: string): { rows: BindingRow[]; parseErrors: string[] } {
    const trimmed = text.trim();
    const parseErrors: string[] = [];
    const rows: BindingRow[] = [];

    if (!trimmed) {
        return { rows, parseErrors: ['Empty input'] };
    }

    // JSON array
    if (trimmed.startsWith('[')) {
        try {
            const data = JSON.parse(trimmed);
            if (!Array.isArray(data)) {
                return { rows, parseErrors: ['JSON must be an array'] };
            }
            data.forEach((item: any, i: number) => {
                const accountId = item.account_id || item.accountId;
                const proxyId = item.proxy_id || item.proxyId;
                if (!accountId || !proxyId) {
                    parseErrors.push(`Row ${i + 1}: missing account_id/proxy_id`);
                    return;
                }
                rows.push({ accountId: String(accountId), proxyId: String(proxyId) });
            });
            return { rows, parseErrors };
        } catch (e) {
            return { rows, parseErrors: [`Invalid JSON: ${String(e)}`] };
        }
    }

    // CSV: account_id,proxy_id
    const lines = trimmed.split(/\r?\n/).filter(l => l.trim());
    let start = 0;
    if (lines[0] && /account[_ ]?id/i.test(lines[0]) && /proxy[_ ]?id/i.test(lines[0])) {
        start = 1;
    }
    for (let i = start; i < lines.length; i++) {
        const parts = lines[i].split(/[,;\t]/).map(p => p.trim().replace(/^"|"$/g, ''));
        if (parts.length < 2 || !parts[0] || !parts[1]) {
            parseErrors.push(`Line ${i + 1}: need account_id,proxy_id`);
            continue;
        }
        rows.push({ accountId: parts[0], proxyId: parts[1] });
    }
    return { rows, parseErrors };
}

export default function BindingBatchImportModal({ isOpen, onClose, onImported }: BindingBatchImportModalProps) {
    const { t } = useTranslation();
    const [rawText, setRawText] = useState('');
    const [busy, setBusy] = useState(false);
    const [lastResult, setLastResult] = useState<{ applied: number; errors: number; messages: string[] } | null>(null);

    if (!isOpen) return null;

    const handleImport = async () => {
        const { rows, parseErrors } = parseBindings(rawText);
        if (rows.length === 0) {
            showToast(parseErrors[0] || t('settings.proxy_pool.binding.batch_empty', 'No bindings to import'), 'error');
            return;
        }
        setBusy(true);
        setLastResult(null);
        try {
            const result = await request<{
                ok: boolean;
                appliedCount?: number;
                applied_count?: number;
                errorCount?: number;
                error_count?: number;
                errors?: Array<{ accountId?: string; account_id?: string; message: string }>;
            }>('batch_bind_account_proxies', {
                bindings: rows.map(r => ({ accountId: r.accountId, proxyId: r.proxyId })),
            });
            const applied = result.appliedCount ?? result.applied_count ?? 0;
            const errCount = result.errorCount ?? result.error_count ?? (result.errors?.length ?? 0);
            const messages = [
                ...parseErrors,
                ...(result.errors || []).map(e => `${e.accountId || e.account_id}: ${e.message}`),
            ];
            setLastResult({ applied, errors: errCount + parseErrors.length, messages });
            showToast(
                t('settings.proxy_pool.binding.batch_done', {
                    defaultValue: `Applied ${applied}, errors ${errCount}`,
                    applied,
                    errors: errCount,
                }),
                errCount > 0 ? 'error' : 'success'
            );
            onImported?.();
        } catch (e) {
            showToast(String(e), 'error');
        } finally {
            setBusy(false);
        }
    };

    return createPortal(
        <div className="fixed inset-0 z-[60] flex items-center justify-center p-4 bg-black/50 backdrop-blur-sm">
            <div className="bg-white dark:bg-gray-800 rounded-xl shadow-xl w-full max-w-lg flex flex-col max-h-[85vh]">
                <div className="flex items-center justify-between p-4 border-b border-gray-200 dark:border-gray-700">
                    <h3 className="text-lg font-semibold flex items-center gap-2">
                        <Upload className="w-5 h-5" />
                        {t('settings.proxy_pool.binding.batch_import', 'Import Bindings')}
                    </h3>
                    <button onClick={onClose} className="p-2 text-gray-500 hover:bg-gray-100 dark:hover:bg-gray-700 rounded-lg">
                        <X size={20} />
                    </button>
                </div>
                <div className="p-4 space-y-3 overflow-y-auto flex-1">
                    <p className="text-xs text-gray-500 flex items-start gap-2">
                        <FileText size={14} className="mt-0.5 shrink-0" />
                        {t('settings.proxy_pool.binding.batch_hint', 'JSON array [{account_id, proxy_id}] or CSV with header account_id,proxy_id')}
                    </p>
                    <textarea
                        className="textarea textarea-bordered w-full h-48 text-xs font-mono"
                        value={rawText}
                        onChange={(e) => setRawText(e.target.value)}
                        placeholder={'[{"account_id":"...","proxy_id":"..."}]\nor\naccount_id,proxy_id\n...'}
                    />
                    {lastResult && (
                        <div className="text-xs bg-gray-50 dark:bg-gray-900/40 rounded-lg p-3 space-y-1">
                            <div>OK: {lastResult.applied} / Errors: {lastResult.errors}</div>
                            {lastResult.messages.slice(0, 8).map((m, i) => (
                                <div key={i} className="text-rose-500 truncate">{m}</div>
                            ))}
                        </div>
                    )}
                </div>
                <div className="p-4 border-t border-gray-200 dark:border-gray-700 flex justify-end gap-2">
                    <button onClick={onClose} className="btn btn-sm btn-ghost">{t('common.close', 'Close')}</button>
                    <button onClick={handleImport} disabled={busy} className="btn btn-sm btn-primary">
                        {busy ? '...' : t('settings.proxy_pool.binding.batch_run', 'Import')}
                    </button>
                </div>
            </div>
        </div>,
        document.body
    );
}
