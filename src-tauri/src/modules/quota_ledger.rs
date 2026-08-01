//! Local quota ledger helpers: burn formula + protection from estimated %.
//! Billing keys are official big buckets: `gemini` | `claude` (see model_mapping).

use crate::models::quota::{QuotaBucket, QuotaData, QuotaGroup};
use crate::models::{Account, EstimatedModelQuota, QuotaLedgerConfig, QuotaProtectionConfig};
use crate::proxy::common::model_mapping::{
    migrate_protected_models_set, migrate_to_billing_group_id, normalize_to_billing_group,
    BILLING_CLAUDE, BILLING_GEMINI,
};
use std::collections::HashMap;

/// Apply quota_protection using billing-group → percentage map.
/// Uses `<=` threshold so reserve at exactly threshold is protected.
pub fn apply_protection_from_percentages(
    account: &mut Account,
    percentages: &HashMap<String, i32>,
    protection: &QuotaProtectionConfig,
) {
    if !protection.enabled {
        return;
    }
    let threshold = protection.threshold_percentage as i32;

    // Ensure protected_models only contains billing groups going forward
    account.protected_models = migrate_protected_models_set(&account.protected_models);

    for raw_id in &protection.monitored_models {
        let Some(billing_id) = migrate_to_billing_group_id(raw_id) else {
            continue;
        };
        let max_pct = percentages.get(&billing_id).cloned().unwrap_or(100);

        if max_pct <= threshold {
            if !account.protected_models.contains(&billing_id) {
                crate::modules::logger::log_info(&format!(
                    "[QuotaLedger] Triggering model protection: {} (Group: {} Est: {}% <= Thres: {}%)",
                    account.email, billing_id, max_pct, threshold
                ));
                account.protected_models.insert(billing_id);
            }
        } else if account.protected_models.contains(&billing_id) {
            crate::modules::logger::log_info(&format!(
                "[QuotaLedger] Model protection recovered: {} (Group: {} Est: {}% > Thres: {}%)",
                account.email, billing_id, max_pct, threshold
            ));
            account.protected_models.remove(&billing_id);
        }
    }

    if account.proxy_disabled
        && account
            .proxy_disabled_reason
            .as_ref()
            .map_or(false, |r| r == "quota_protection")
    {
        crate::modules::logger::log_info(&format!(
            "[QuotaLedger] Migrating account {} from account-level to model-level protection",
            account.email
        ));
        account.proxy_disabled = false;
        account.proxy_disabled_reason = None;
        account.proxy_disabled_at = None;
    }
}

/// Build percentage map from estimated_quotas (preferred) or empty.
pub fn estimated_percentage_map(account: &Account) -> HashMap<String, i32> {
    account
        .estimated_quotas
        .iter()
        .filter_map(|(k, v)| {
            let billing = migrate_to_billing_group_id(k).unwrap_or_else(|| k.clone());
            Some((billing, v.percentage))
        })
        .fold(HashMap::new(), |mut acc, (k, pct)| {
            // If duplicates after migration, keep min (conservative)
            let entry = acc.entry(k).or_insert(pct);
            if pct < *entry {
                *entry = pct;
            }
            acc
        })
}

/// Map official quota_groups buckets → billing group percentages.
/// Prefers `5h` window; falls back to `weekly` / any other window.
pub fn billing_percentages_from_quota_groups(groups: &[QuotaGroup]) -> HashMap<String, i32> {
    // billing -> (preferred_pct from 5h, fallback_pct)
    let mut five_h: HashMap<String, i32> = HashMap::new();
    let mut fallback: HashMap<String, i32> = HashMap::new();

    for group in groups {
        for bucket in &group.buckets {
            if let Some((billing, pct)) = bucket_to_billing_pct(bucket) {
                let is_5h = bucket.window.eq_ignore_ascii_case("5h")
                    || bucket.bucket_id.to_lowercase().contains("5h");
                if is_5h {
                    five_h.insert(billing, pct);
                } else {
                    fallback.entry(billing).or_insert(pct);
                }
            }
        }
        // Also try display_name heuristics when buckets lack gemini-/3p- prefixes
        if five_h.is_empty() && fallback.is_empty() {
            let dn = group.display_name.to_lowercase();
            if let Some(first) = group.buckets.first() {
                let pct = fraction_to_pct(first.remaining_fraction);
                if dn.contains("gemini") {
                    fallback.insert(BILLING_GEMINI.to_string(), pct);
                } else if dn.contains("claude") || dn.contains("gpt") || dn.contains("3p") {
                    fallback.insert(BILLING_CLAUDE.to_string(), pct);
                }
            }
        }
    }

    let mut out = fallback;
    for (k, v) in five_h {
        out.insert(k, v);
    }
    out
}

fn fraction_to_pct(fraction: f64) -> i32 {
    ((fraction * 100.0).round() as i32).clamp(0, 100)
}

fn bucket_to_billing_pct(bucket: &QuotaBucket) -> Option<(String, i32)> {
    let id = bucket.bucket_id.to_lowercase();
    let billing = if id.starts_with("gemini") || id.contains("gemini") {
        BILLING_GEMINI.to_string()
    } else if id.starts_with("3p") || id.contains("3p") || id.contains("claude") {
        BILLING_CLAUDE.to_string()
    } else {
        return None;
    };
    Some((billing, fraction_to_pct(bucket.remaining_fraction)))
}

/// Build percentage map from online sources using official billing groups.
/// Prefer `quota_groups` (5h); fall back to per-model min within each billing group.
pub fn online_percentage_map(account: &Account) -> HashMap<String, i32> {
    let Some(ref q) = account.quota else {
        return HashMap::new();
    };
    online_percentage_map_from_quota(q)
}

pub fn online_percentage_map_from_quota(quota: &QuotaData) -> HashMap<String, i32> {
    if let Some(ref groups) = quota.quota_groups {
        let from_groups = billing_percentages_from_quota_groups(groups);
        if !from_groups.is_empty() {
            return from_groups;
        }
    }

    // Fallback: aggregate fetchAvailableModels by billing group (min = conservative)
    let mut group_min: HashMap<String, i32> = HashMap::new();
    for model in &quota.models {
        let Some(billing) = normalize_to_billing_group(&model.name) else {
            continue;
        };
        let entry = group_min.entry(billing).or_insert(model.percentage);
        if model.percentage < *entry {
            *entry = model.percentage;
        }
    }
    group_min
}

/// Percentages used for protection/selection when ledger may be on/off.
pub fn effective_percentage_map(
    account: &Account,
    ledger: &QuotaLedgerConfig,
) -> HashMap<String, i32> {
    if ledger.enabled && !account.estimated_quotas.is_empty() {
        estimated_percentage_map(account)
    } else {
        online_percentage_map(account)
    }
}

/// Merge legacy fine-grained estimated_quotas into billing-group keys (min remaining).
pub fn migrate_estimated_quotas(
    estimated: HashMap<String, EstimatedModelQuota>,
) -> HashMap<String, EstimatedModelQuota> {
    let mut merged: HashMap<String, EstimatedModelQuota> = HashMap::new();
    for (key, est) in estimated {
        let Some(billing) = migrate_to_billing_group_id(&key) else {
            continue;
        };
        match merged.get_mut(&billing) {
            Some(existing) => {
                if est.percentage < existing.percentage {
                    existing.percentage = est.percentage;
                }
                // Keep more recent calibration metadata when present
                match (existing.last_calibrated_at, est.last_calibrated_at) {
                    (Some(a), Some(b)) if b >= a => {
                        existing.last_online_pct = est.last_online_pct.or(existing.last_online_pct);
                        existing.last_calibrated_at = est.last_calibrated_at;
                    }
                    (None, Some(_)) => {
                        existing.last_online_pct = est.last_online_pct;
                        existing.last_calibrated_at = est.last_calibrated_at;
                    }
                    _ => {}
                }
                existing.model = billing.clone();
            }
            None => {
                merged.insert(
                    billing.clone(),
                    EstimatedModelQuota {
                        model: billing,
                        percentage: est.percentage,
                        last_online_pct: est.last_online_pct,
                        last_calibrated_at: est.last_calibrated_at,
                    },
                );
            }
        }
    }
    merged
}

/// Cooldown between online calibrations triggered by local ledger crossing the
/// protection threshold (per account + billing group).
pub const THRESHOLD_CALIBRATE_COOLDOWN_SECS: i64 = 600;

/// Decision for a single burn against the protection threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThresholdCrossDecision {
    /// Local % crossed from above threshold to `<= threshold`.
    pub crossed: bool,
    /// Persist / memory protect immediately (no pending calibrate).
    pub should_protect_now: bool,
    /// Spawn async online calibrate before deciding protection.
    pub should_trigger_calibrate: bool,
}

/// Evaluate burn vs protection threshold.
///
/// - Crossing (`current > threshold` → `new <= threshold`) with protect on:
///   delay protect and trigger calibrate unless cooldown is active (then protect
///   on local estimate as fallback).
/// - Already at/below threshold: protect now, no calibrate.
/// - Protection off: never protect / calibrate from this path.
pub fn evaluate_threshold_cross(
    protection_on: bool,
    current_pct: i32,
    new_pct: i32,
    threshold: i32,
    in_cooldown: bool,
) -> ThresholdCrossDecision {
    if !protection_on {
        return ThresholdCrossDecision {
            crossed: false,
            should_protect_now: false,
            should_trigger_calibrate: false,
        };
    }

    let crossed = current_pct > threshold && new_pct <= threshold;
    if crossed {
        if in_cooldown {
            ThresholdCrossDecision {
                crossed: true,
                should_protect_now: true,
                should_trigger_calibrate: false,
            }
        } else {
            ThresholdCrossDecision {
                crossed: true,
                should_protect_now: false,
                should_trigger_calibrate: true,
            }
        }
    } else {
        ThresholdCrossDecision {
            crossed: false,
            should_protect_now: new_pct <= threshold,
            should_trigger_calibrate: false,
        }
    }
}

/// Whether a previous calibrate timestamp is still inside the cooldown window.
pub fn is_calibrate_cooldown_active(
    last_ts: Option<i64>,
    now_ts: i64,
    cooldown_secs: i64,
) -> bool {
    match last_ts {
        Some(last) => now_ts.saturating_sub(last) < cooldown_secs,
        None => false,
    }
}

pub fn threshold_calibrate_cooldown_key(account_id: &str, billing_group: &str) -> String {
    format!("{}:{}", account_id, billing_group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::QuotaLedgerConfig;

    #[test]
    fn burn_without_usage_uses_min() {
        let cfg = QuotaLedgerConfig::default();
        assert_eq!(cfg.compute_burn_pct("gemini-3.1-pro", None, None, None), 1);
        assert_eq!(
            cfg.compute_burn_pct("gemini-3.1-pro", Some(0), Some(0), Some(0)),
            1
        );
    }

    #[test]
    fn burn_pro_input_1000_is_one_pct() {
        let cfg = QuotaLedgerConfig::default();
        // ΔCU = 1000/1000 × 1.0 = 1 → ceil(1/500*100) = 1
        assert_eq!(
            cfg.compute_burn_pct("gemini-3.1-pro", Some(1000), Some(0), Some(0)),
            1
        );
    }

    #[test]
    fn burn_cache_subset_not_double_counted() {
        let cfg = QuotaLedgerConfig::default();
        // billable_in=600, cache=400 → (600*1 + 400*0.15)/1000 = 0.66 CU → 1%
        let with_cache =
            cfg.compute_burn_pct("gemini-3.1-pro", Some(1000), Some(0), Some(400));
        assert_eq!(with_cache, 1);

        // Same tokens all as non-cached input would be 1 CU → 1%
        // Larger burn when cache is NOT subtracted (verify subset path is cheaper than full input)
        let mut no_subset = QuotaLedgerConfig::default();
        no_subset.cache_is_subset_of_input = false;
        let double =
            no_subset.compute_burn_pct("gemini-3.1-pro", Some(1000), Some(0), Some(400));
        // (1000*1 + 400*0.15)/1000 = 1.06 CU → ceil(1.06/500*100)=1 still
        // Use bigger numbers to see difference:
        let subset_big =
            cfg.compute_burn_pct("gemini-3.1-pro", Some(50_000), Some(0), Some(40_000));
        // billable=10000 + cache*0.15=6000 → 16 CU → ceil(16/500*100)=4
        assert_eq!(subset_big, 4);
        let full_big =
            no_subset.compute_burn_pct("gemini-3.1-pro", Some(50_000), Some(0), Some(40_000));
        // 50k + 6k = 56 CU → ceil(56/500*100)=12
        assert_eq!(full_big, 12);
        assert!(full_big > subset_big);
    }

    #[test]
    fn burn_flash_cheaper_than_pro_opus_more() {
        let cfg = QuotaLedgerConfig::default();
        // in=10000, out=0 → weighted CU base = 10
        let pro = cfg.compute_burn_pct("gemini-3.1-pro-high", Some(10_000), Some(0), None);
        let flash = cfg.compute_burn_pct("gemini-3-flash", Some(10_000), Some(0), None);
        let opus =
            cfg.compute_burn_pct("claude-opus-4-6-thinking", Some(10_000), Some(0), None);
        // Pro: 10 CU → 2%; Flash 0.25× → 2.5 CU → 1%; Opus 8× → 80 CU → 16%
        assert_eq!(pro, 2);
        assert_eq!(flash, 1);
        assert_eq!(opus, 16);
        assert!(flash < pro);
        assert!(opus > pro);
    }

    #[test]
    fn resolve_multiplier_exact_family_fallback() {
        let cfg = QuotaLedgerConfig::default();
        assert!((cfg.resolve_model_multiplier("gemini-3-flash") - 0.25).abs() < 1e-9);
        assert!((cfg.resolve_model_multiplier("claude-sonnet-4-6-thinking") - 3.0).abs() < 1e-9);
        // Family: sonnet via keyword when not exact... wait exact exists for thinking.
        // Unknown model → fallback 3.0
        assert!((cfg.resolve_model_multiplier("some-unknown-model-xyz") - 3.0).abs() < 1e-9);
        // Family flash-lite before flash
        assert!((cfg.resolve_model_multiplier("gemini-custom-flash-lite") - 0.1).abs() < 1e-9);
        // Prefix strip
        assert!(
            (cfg.resolve_model_multiplier("antigravity/gemini-3-flash") - 0.25).abs() < 1e-9
        );
        // image before pro for unmatched image ids
        assert!((cfg.resolve_model_multiplier("nano-banana-image") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn burn_tiny_nonzero_ceils_to_min() {
        let cfg = QuotaLedgerConfig::default();
        // 10 input tokens Pro → 0.01 CU → ceil(0.004)=1%
        assert_eq!(
            cfg.compute_burn_pct("gemini-3.1-pro", Some(10), Some(0), None),
            1
        );
    }

    #[test]
    fn burn_output_weighted_higher() {
        let cfg = QuotaLedgerConfig::default();
        // 10_000 out × 3.5 / 1000 = 35 CU → ~ceil(35/500*100) (f64 may ceil 7.0→8)
        // 10_000 in → 10 CU → 2%
        let out_burn =
            cfg.compute_burn_pct("gemini-3.1-pro", Some(0), Some(10_000), None);
        let in_burn =
            cfg.compute_burn_pct("gemini-3.1-pro", Some(10_000), Some(0), None);
        assert_eq!(in_burn, 2);
        assert!(out_burn > in_burn);
        assert!((7..=8).contains(&out_burn));
    }

    #[test]
    fn threshold_cross_delays_protect_and_triggers_calibrate() {
        let d = evaluate_threshold_cross(true, 15, 8, 10, false);
        assert!(d.crossed);
        assert!(!d.should_protect_now);
        assert!(d.should_trigger_calibrate);
    }

    #[test]
    fn threshold_cross_in_cooldown_protects_without_calibrate() {
        let d = evaluate_threshold_cross(true, 15, 8, 10, true);
        assert!(d.crossed);
        assert!(d.should_protect_now);
        assert!(!d.should_trigger_calibrate);
    }

    #[test]
    fn already_below_threshold_protects_no_calibrate() {
        let d = evaluate_threshold_cross(true, 8, 5, 10, false);
        assert!(!d.crossed);
        assert!(d.should_protect_now);
        assert!(!d.should_trigger_calibrate);
    }

    #[test]
    fn protection_off_never_calibrates() {
        let d = evaluate_threshold_cross(false, 15, 8, 10, false);
        assert!(!d.crossed);
        assert!(!d.should_protect_now);
        assert!(!d.should_trigger_calibrate);
    }

    #[test]
    fn calibrate_cooldown_window() {
        assert!(!is_calibrate_cooldown_active(None, 1000, 600));
        assert!(is_calibrate_cooldown_active(Some(500), 1000, 600));
        assert!(!is_calibrate_cooldown_active(Some(300), 1000, 600));
        assert!(is_calibrate_cooldown_active(
            Some(1000),
            1000 + THRESHOLD_CALIBRATE_COOLDOWN_SECS - 1,
            THRESHOLD_CALIBRATE_COOLDOWN_SECS
        ));
        assert!(!is_calibrate_cooldown_active(
            Some(1000),
            1000 + THRESHOLD_CALIBRATE_COOLDOWN_SECS,
            THRESHOLD_CALIBRATE_COOLDOWN_SECS
        ));
    }

    #[test]
    fn protection_uses_le_threshold() {
        let mut account = Account::new(
            "a1".into(),
            "a@test.com".into(),
            crate::models::TokenData::new(
                "x".into(),
                "y".into(),
                3600,
                Some("a@test.com".into()),
                None,
                None,
                false,
                None,
            ),
        );
        let mut pcts = HashMap::new();
        pcts.insert("claude".to_string(), 10);
        let protection = QuotaProtectionConfig {
            enabled: true,
            threshold_percentage: 10,
            monitored_models: vec!["claude".to_string()],
        };
        apply_protection_from_percentages(&mut account, &pcts, &protection);
        assert!(account.protected_models.contains("claude"));

        pcts.insert("claude".to_string(), 11);
        apply_protection_from_percentages(&mut account, &pcts, &protection);
        assert!(!account.protected_models.contains("claude"));
    }

    #[test]
    fn calibrate_from_quota_groups_prefers_5h() {
        use crate::models::quota::{QuotaBucket, QuotaGroup};
        use crate::models::{QuotaData, TokenData};

        let mut account = Account::new(
            "a1".into(),
            "a@test.com".into(),
            TokenData::new(
                "x".into(),
                "y".into(),
                3600,
                Some("a@test.com".into()),
                None,
                None,
                false,
                None,
            ),
        );

        let mut quota = QuotaData::new();
        quota.quota_groups = Some(vec![
            QuotaGroup {
                display_name: "Gemini Models".into(),
                description: None,
                buckets: vec![
                    QuotaBucket {
                        bucket_id: "gemini-weekly".into(),
                        window: "weekly".into(),
                        remaining_fraction: 0.9,
                        reset_time: String::new(),
                        display_name: None,
                        description: None,
                    },
                    QuotaBucket {
                        bucket_id: "gemini-5h".into(),
                        window: "5h".into(),
                        remaining_fraction: 0.42,
                        reset_time: String::new(),
                        display_name: None,
                        description: None,
                    },
                ],
            },
            QuotaGroup {
                display_name: "Claude and GPT models".into(),
                description: None,
                buckets: vec![QuotaBucket {
                    bucket_id: "3p-5h".into(),
                    window: "5h".into(),
                    remaining_fraction: 0.77,
                    reset_time: String::new(),
                    display_name: None,
                    description: None,
                }],
            },
        ]);

        account.calibrate_estimated_from_quota(&quota);
        assert_eq!(
            account.estimated_quotas.get("gemini").map(|e| e.percentage),
            Some(42)
        );
        assert_eq!(
            account.estimated_quotas.get("claude").map(|e| e.percentage),
            Some(77)
        );
    }

    #[test]
    fn calibrate_fallback_models_uses_billing_min() {
        use crate::models::quota::ModelQuota;
        use crate::models::{QuotaData, TokenData};

        let mut account = Account::new(
            "a1".into(),
            "a@test.com".into(),
            TokenData::new(
                "x".into(),
                "y".into(),
                3600,
                Some("a@test.com".into()),
                None,
                None,
                false,
                None,
            ),
        );

        let mut quota = QuotaData::new();
        quota.add_model(ModelQuota {
            name: "gemini-3-flash".into(),
            percentage: 20,
            reset_time: String::new(),
            display_name: None,
            supports_images: None,
            supports_thinking: None,
            thinking_budget: None,
            recommended: None,
            max_tokens: None,
            max_output_tokens: None,
            supported_mime_types: None,
        });
        quota.add_model(ModelQuota {
            name: "gemini-3.1-pro-high".into(),
            percentage: 80,
            reset_time: String::new(),
            display_name: None,
            supports_images: None,
            supports_thinking: None,
            thinking_budget: None,
            recommended: None,
            max_tokens: None,
            max_output_tokens: None,
            supported_mime_types: None,
        });
        quota.add_model(ModelQuota {
            name: "claude-sonnet-4-6".into(),
            percentage: 42,
            reset_time: String::new(),
            display_name: None,
            supports_images: None,
            supports_thinking: None,
            thinking_budget: None,
            recommended: None,
            max_tokens: None,
            max_output_tokens: None,
            supported_mime_types: None,
        });

        account.calibrate_estimated_from_quota(&quota);
        assert_eq!(
            account.estimated_quotas.get("gemini").map(|e| e.percentage),
            Some(20)
        );
        assert_eq!(
            account.estimated_quotas.get("claude").map(|e| e.percentage),
            Some(42)
        );
    }

    #[test]
    fn migrate_estimated_takes_min() {
        let mut legacy = HashMap::new();
        legacy.insert(
            "gemini-3-flash".into(),
            EstimatedModelQuota {
                model: "gemini-3-flash".into(),
                percentage: 10,
                last_online_pct: Some(10),
                last_calibrated_at: Some(1),
            },
        );
        legacy.insert(
            "gemini-3-pro-high".into(),
            EstimatedModelQuota {
                model: "gemini-3-pro-high".into(),
                percentage: 50,
                last_online_pct: Some(50),
                last_calibrated_at: Some(2),
            },
        );
        let migrated = migrate_estimated_quotas(legacy);
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated.get("gemini").map(|e| e.percentage), Some(10));
    }
}
