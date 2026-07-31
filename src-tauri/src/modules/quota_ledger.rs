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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::QuotaLedgerConfig;

    #[test]
    fn burn_without_usage_uses_min() {
        let cfg = QuotaLedgerConfig {
            enabled: true,
            min_burn_pct: 1,
            tokens_per_percent: 20_000,
        };
        assert_eq!(cfg.compute_burn_pct(None), 1);
        assert_eq!(cfg.compute_burn_pct(Some(0)), 1);
    }

    #[test]
    fn burn_with_usage_takes_max() {
        let cfg = QuotaLedgerConfig {
            enabled: true,
            min_burn_pct: 1,
            tokens_per_percent: 20_000,
        };
        assert_eq!(cfg.compute_burn_pct(Some(100)), 1);
        assert_eq!(cfg.compute_burn_pct(Some(20_000)), 1);
        assert_eq!(cfg.compute_burn_pct(Some(20_001)), 2);
        assert_eq!(cfg.compute_burn_pct(Some(40_000)), 2);
        assert_eq!(cfg.compute_burn_pct(Some(40_001)), 3);
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
