//! Local quota ledger helpers: burn formula + protection from estimated %.

use crate::models::{Account, QuotaLedgerConfig, QuotaProtectionConfig};
use std::collections::HashMap;

/// Apply quota_protection using the given standard-id → percentage map.
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

    for std_id in &protection.monitored_models {
        let max_pct = percentages.get(std_id).cloned().unwrap_or(100);

        if max_pct <= threshold {
            if !account.protected_models.contains(std_id) {
                crate::modules::logger::log_info(&format!(
                    "[QuotaLedger] Triggering model protection: {} (Group: {} Est: {}% <= Thres: {}%)",
                    account.email, std_id, max_pct, threshold
                ));
                account.protected_models.insert(std_id.clone());
            }
        } else if account.protected_models.contains(std_id) {
            crate::modules::logger::log_info(&format!(
                "[QuotaLedger] Model protection recovered: {} (Group: {} Est: {}% > Thres: {}%)",
                account.email, std_id, max_pct, threshold
            ));
            account.protected_models.remove(std_id);
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
        .map(|(k, v)| (k.clone(), v.percentage))
        .collect()
}

/// Build percentage map from online quota models (group max by standard id).
pub fn online_percentage_map(account: &Account) -> HashMap<String, i32> {
    let mut group_max: HashMap<String, i32> = HashMap::new();
    let Some(ref q) = account.quota else {
        return group_max;
    };
    for model in &q.models {
        let std_id = crate::proxy::common::model_mapping::normalize_to_standard_id(&model.name)
            .unwrap_or_else(|| model.name.clone());
        let entry = group_max.entry(std_id).or_insert(-1);
        if model.percentage > *entry {
            *entry = model.percentage;
        }
    }
    group_max
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
    fn calibrate_overwrites_estimated_from_online() {
        use crate::models::quota::ModelQuota;
        use crate::models::{EstimatedModelQuota, QuotaData, TokenData};

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
        account.estimated_quotas.insert(
            "claude".into(),
            EstimatedModelQuota {
                model: "claude".into(),
                percentage: 3,
                last_online_pct: Some(50),
                last_calibrated_at: Some(1),
            },
        );

        let mut quota = QuotaData::new();
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
        let est = account.estimated_quotas.get("claude").expect("claude key");
        assert_eq!(est.percentage, 42);
        assert_eq!(est.last_online_pct, Some(42));
        assert!(est.last_calibrated_at.is_some());
    }
}
