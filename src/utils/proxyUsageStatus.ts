/** Live egress usage from real upstream requests (absent = unknown). */
export type ProxyEgressUsage = 'ok' | 'failed' | 'unknown';

export function resolveProxyUsageStatus(
  proxyId: string | undefined | null,
  map?: Record<string, string> | null,
): ProxyEgressUsage {
  if (!proxyId || !map) return 'unknown';
  const v = map[proxyId];
  if (v === 'ok' || v === 'failed') return v;
  return 'unknown';
}

/** Tailwind classes for the proxy binding badge. */
export function proxyUsageBadgeClass(status: ProxyEgressUsage): string {
  switch (status) {
    case 'ok':
      return 'bg-emerald-100 dark:bg-emerald-900/50 text-emerald-700 dark:text-emerald-300 border-emerald-200/50 dark:border-emerald-800/50';
    case 'failed':
      return 'bg-rose-100 dark:bg-rose-900/50 text-rose-700 dark:text-rose-300 border-rose-200/50 dark:border-rose-800/50';
    default:
      return 'bg-amber-100 dark:bg-amber-900/50 text-amber-700 dark:text-amber-300 border-amber-200/50 dark:border-amber-800/50';
  }
}

/** i18n key for badge tooltip. */
export function proxyUsageTooltipKey(status: ProxyEgressUsage): string {
  switch (status) {
    case 'ok':
      return 'accounts.proxy_usage_ok';
    case 'failed':
      return 'accounts.proxy_usage_failed';
    default:
      return 'accounts.proxy_usage_unknown';
  }
}
