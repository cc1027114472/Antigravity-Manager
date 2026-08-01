//! Per-account overlapping in-flight request counters (observability).

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use dashmap::DashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Response header carrying peak overlapping concurrency for the account.
pub const IN_FLIGHT_PEAK_HEADER: &str = "x-in-flight-peak";

/// Insert `X-In-Flight-Peak` when peak is known (capture before guard Drop).
pub fn insert_peak_header(headers: &mut HeaderMap, peak: Option<u32>) {
    let Some(peak) = peak else {
        return;
    };
    if let Ok(v) = HeaderValue::from_str(&peak.to_string()) {
        headers.insert(HeaderName::from_static(IN_FLIGHT_PEAK_HEADER), v);
    }
}

pub fn parse_peak_header(headers: &HeaderMap) -> Option<u32> {
    headers
        .get(IN_FLIGHT_PEAK_HEADER)
        .or_else(|| headers.get("X-In-Flight-Peak"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

/// Refresh and return peak from an optional guard (call before error paths / response headers).
pub fn sample_peak(guard: &mut Option<AccountInflightGuard>) -> Option<u32> {
    guard.as_mut().map(|g| {
        g.refresh_peak();
        g.peak()
    })
}

/// Drop previous guard (if any) and begin tracking a new account.
pub fn replace_guard(
    guard: &mut Option<AccountInflightGuard>,
    tracker: &AccountInflightTracker,
    account_id: &str,
) {
    if let Some(old) = guard.take() {
        old.release();
    }
    *guard = Some(tracker.begin(account_id));
}

/// Build account-related response headers including optional in-flight peak.
pub fn account_headers(
    email: &str,
    account_id: Option<&str>,
    mapped_model: Option<&str>,
    peak: Option<u32>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(email) {
        headers.insert("X-Account-Email", v);
    }
    if let Some(id) = account_id {
        if let Ok(v) = HeaderValue::from_str(id) {
            headers.insert("X-Account-Id", v);
        }
    }
    if let Some(model) = mapped_model {
        if let Ok(v) = HeaderValue::from_str(model) {
            headers.insert("X-Mapped-Model", v);
        }
    }
    insert_peak_header(&mut headers, peak);
    headers
}

/// Shared map of account_id → current in-flight count.
#[derive(Debug, Default)]
pub struct AccountInflightTracker {
    counts: Arc<DashMap<String, AtomicUsize>>,
}

impl AccountInflightTracker {
    pub fn new() -> Self {
        Self {
            counts: Arc::new(DashMap::new()),
        }
    }

    /// Begin tracking one in-flight request for `account_id`.
    /// Drop the returned guard (or replace it) when the request finishes or rotates away.
    pub fn begin(&self, account_id: impl Into<String>) -> AccountInflightGuard {
        let account_id = account_id.into();
        let entry = self
            .counts
            .entry(account_id.clone())
            .or_insert_with(|| AtomicUsize::new(0));
        let current = entry.fetch_add(1, Ordering::SeqCst) + 1;
        AccountInflightGuard {
            tracker: Arc::clone(&self.counts),
            account_id,
            peak: current,
            active: true,
        }
    }

    pub fn current(&self, account_id: &str) -> usize {
        self.counts
            .get(account_id)
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(0)
    }
}

/// RAII guard: increments on create, decrements on drop; tracks peak for this request.
pub struct AccountInflightGuard {
    tracker: Arc<DashMap<String, AtomicUsize>>,
    account_id: String,
    peak: usize,
    active: bool,
}

impl AccountInflightGuard {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Peak in-flight count observed for this account while this guard was held
    /// (at least the count right after acquire; refreshed via `refresh_peak`).
    pub fn peak(&self) -> u32 {
        self.peak as u32
    }

    pub fn current(&self) -> usize {
        self.tracker
            .get(&self.account_id)
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Re-sample global account count into this request's peak (call before error logging).
    pub fn refresh_peak(&mut self) {
        let cur = self.current();
        if cur > self.peak {
            self.peak = cur;
        }
    }

    /// Release without waiting for Drop (e.g. before rotating to another account).
    pub fn release(mut self) {
        self.decrement();
        self.active = false;
    }
}

impl AccountInflightGuard {
    fn decrement(&mut self) {
        if !self.active {
            return;
        }
        if let Some(entry) = self.tracker.get(&self.account_id) {
            loop {
                let cur = entry.load(Ordering::SeqCst);
                if cur == 0 {
                    break;
                }
                if entry
                    .compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    break;
                }
            }
        }
        self.active = false;
    }
}

impl Drop for AccountInflightGuard {
    fn drop(&mut self) {
        self.decrement();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_increments_and_drop_decrements() {
        let t = AccountInflightTracker::new();
        assert_eq!(t.current("a1"), 0);
        {
            let g = t.begin("a1");
            assert_eq!(g.peak(), 1);
            assert_eq!(t.current("a1"), 1);
        }
        assert_eq!(t.current("a1"), 0);
    }

    #[test]
    fn peak_tracks_overlapping() {
        let t = AccountInflightTracker::new();
        let mut g1 = t.begin("a1");
        assert_eq!(g1.peak(), 1);
        let g2 = t.begin("a1");
        assert_eq!(g2.peak(), 2);
        g1.refresh_peak();
        assert_eq!(g1.peak(), 2);
        drop(g2);
        assert_eq!(t.current("a1"), 1);
        drop(g1);
        assert_eq!(t.current("a1"), 0);
    }

    #[test]
    fn release_then_drop_is_safe() {
        let t = AccountInflightTracker::new();
        let g = t.begin("a1");
        g.release();
        assert_eq!(t.current("a1"), 0);
    }

    #[test]
    fn drop_never_underflows() {
        let t = AccountInflightTracker::new();
        let g = t.begin("a1");
        // Manually zero (simulate bug) then drop should not wrap
        if let Some(e) = t.counts.get("a1") {
            e.store(0, Ordering::SeqCst);
        }
        drop(g);
        assert_eq!(t.current("a1"), 0);
    }
}
