use crate::commands::proxy::ProxyServiceState;
use std::collections::HashMap;
use tauri::State;

/// Bind an account to a specific proxy
#[tauri::command]
pub async fn bind_account_proxy(
    state: State<'_, ProxyServiceState>,
    account_id: String,
    proxy_id: String,
) -> Result<(), String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        instance
            .axum_server
            .proxy_pool_manager
            .bind_account_to_proxy(account_id, proxy_id)
            .await
    } else {
        Err("Service not running".to_string())
    }
}

/// Unbind an account from its proxy
#[tauri::command]
pub async fn unbind_account_proxy(
    state: State<'_, ProxyServiceState>,
    account_id: String,
) -> Result<(), String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        instance
            .axum_server
            .proxy_pool_manager
            .unbind_account_proxy(account_id)
            .await;
        Ok(())
    } else {
        Err("Service not running".to_string())
    }
}

/// Get the proxy binding for a specific account
#[tauri::command]
pub async fn get_account_proxy_binding(
    state: State<'_, ProxyServiceState>,
    account_id: String,
) -> Result<Option<String>, String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        Ok(instance
            .axum_server
            .proxy_pool_manager
            .get_account_binding(&account_id))
    } else {
        Err("Service not running".to_string())
    }
}

/// Get all account proxy bindings
#[tauri::command]
pub async fn get_all_account_bindings(
    state: State<'_, ProxyServiceState>,
) -> Result<HashMap<String, String>, String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        // Since get_all_bindings returns a DashMap ref or clone, we need to convert it to HashMap for serialization
        // Assuming we add a method to ProxyPoolManager to get a snapshot
        Ok(instance
            .axum_server
            .proxy_pool_manager
            .get_all_bindings_snapshot())
    } else {
        Err("Service not running".to_string())
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchBindItem {
    pub account_id: String,
    pub proxy_id: String,
}

/// Batch upsert account↔proxy bindings
#[tauri::command]
pub async fn batch_bind_account_proxies(
    state: State<'_, ProxyServiceState>,
    bindings: Vec<BatchBindItem>,
) -> Result<crate::proxy::proxy_pool::BatchBindResult, String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        let entries = bindings
            .into_iter()
            .map(|b| (b.account_id, b.proxy_id))
            .collect();
        Ok(instance
            .axum_server
            .proxy_pool_manager
            .bind_accounts_batch(entries)
            .await)
    } else {
        Err("Service not running".to_string())
    }
}

/// Aggregate pool health snapshot (does not probe)
#[tauri::command]
pub async fn get_proxy_pool_health(
    state: State<'_, ProxyServiceState>,
) -> Result<crate::proxy::proxy_pool::PoolHealthSnapshot, String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        let account_ids = instance.token_manager.list_account_ids();
        Ok(instance
            .axum_server
            .proxy_pool_manager
            .pool_health_snapshot(&account_ids)
            .await)
    } else {
        Err("Service not running".to_string())
    }
}

/// Live egress usage from real upstream requests (`ok` / `failed`; absent = unknown).
#[tauri::command]
pub async fn get_proxy_egress_usage(
    state: State<'_, ProxyServiceState>,
) -> Result<HashMap<String, crate::proxy::proxy_pool::EgressUsageStatus>, String> {
    let instance_lock = state.instance.read().await;
    if let Some(instance) = instance_lock.as_ref() {
        Ok(instance
            .axum_server
            .proxy_pool_manager
            .get_egress_usage_snapshot())
    } else {
        Err("Service not running".to_string())
    }
}
