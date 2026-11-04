use serde_json::Value;

/// 使用 Antigravity 的 loadCodeAssist API 获取 project_id
/// 这是获取 cloudaicompanionProject 的正确方式
///
/// `account_id` 用于与 AI 反代共用同一 egress（bound → pool → upstream → direct）。
/// [STRICT PROXY] 严格代理策略：若最终路由为 Direct（无池代理且无上游代理），
/// 一律报错拒绝，严禁本地直连 googleapis 探测 Project ID！
pub async fn fetch_project_id(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<String, String> {
    if let Some(id) = account_id {
        if let Some(pool) = crate::proxy::proxy_pool::get_global_proxy_pool() {
            if !pool.is_account_proxy_available(id).await {
                return Err(format!("严格代理策略拦截：账号 {} 未绑定健康代理或代理不通 (RED)，禁止直连探测 Project ID", id));
            }
        }
    }

    // 使用 Sandbox 环境，避免 Prod 环境的 429 错误
    let url = "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:loadCodeAssist";

    let request_body = serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });

    // [STRICT PROXY] 使用带 route 信息的接口，Direct 路由必须拒绝
    let (client, route) = if let Some(pool) = crate::proxy::proxy_pool::get_global_proxy_pool() {
        pool.get_effective_standard_client_with_route(account_id, 15).await
    } else {
        // 池未初始化：检查全局上游代理，未启用则拒绝
        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                (
                    crate::utils::http::get_standard_client(),
                    crate::proxy::proxy_pool::EgressRoute::Upstream,
                )
            } else {
                return Err("严格代理策略拦截：全局代理池未初始化且未配置启用全局上游代理，禁止直连探测 Project ID".to_string());
            }
        } else {
            return Err("严格代理策略拦截：无法读取应用配置确定代理出口，禁止直连探测 Project ID".to_string());
        }
    };

    if route == crate::proxy::proxy_pool::EgressRoute::Direct {
        return Err("严格代理策略拦截：未匹配到健康代理且上游代理未启用 (Direct)，禁止直连探测 Project ID".to_string());
    }

    let response = client
        .post(url)
        .bearer_auth(access_token)
        // .header("Host", "cloudcode-pa.googleapis.com") // 移除 Host header，因为已切换域名
        .header("User-Agent", crate::constants::get_account_user_agent(account_id).as_str())
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("loadCodeAssist 请求失败: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("loadCodeAssist 返回错误 {}: {}", status, body));
    }

    let data: Value = response
        .json()
        .await
        .map_err(|e| format!("解析响应失败: {}", e))?;

    // 提取 cloudaicompanionProject
    if let Some(project_id) = data.get("cloudaicompanionProject").and_then(|v| v.as_str()) {
        return Ok(project_id.to_string());
    }

    // 如果没有返回 project_id，说明账号无资格，返回错误以触发 token_manager 的稳定兜底逻辑
    Err("账号无资格获取官方 cloudaicompanionProject".to_string())
}