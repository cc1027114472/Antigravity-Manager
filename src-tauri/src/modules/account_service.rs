use crate::models::{Account, TokenData};
use crate::modules;

/// 账号服务层 - 彻底解除对 Tauri 运行时的依赖
pub struct AccountService {
    pub integration: crate::modules::integration::SystemManager,
}

impl AccountService {
    pub fn new(integration: crate::modules::integration::SystemManager) -> Self {
        Self { integration }
    }

    /// 添加账号逻辑
    pub async fn add_account(&self, refresh_token: &str) -> Result<Account, String> {
        self.add_account_with_options(refresh_token, None, false)
            .await
    }

    /// 添加账号逻辑（支持在首次 OAuth/Project 探测阶段就使用指定的专属代理，防止新号先用随机 IP 再切专属 IP 触发跨 IP 风控）
    pub async fn add_account_with_options(
        &self,
        refresh_token: &str,
        preferred_proxy_id: Option<&str>,
        skip_initial_quota: bool,
    ) -> Result<Account, String> {
        // 无论是否指定了专属代理，都在导入探测阶段统一使用纯内存临时路由覆盖（不写盘 gui_config.json，
        // 不受 max_accounts=1 阻塞），并在 Go Sidecar 中按 `tmp_` 前缀走一次性短连接（不留连接池泄漏），
        // 确保步骤 1 (oauth2/token)、步骤 2 (userinfo)、步骤 3 (loadCodeAssist) 及步骤 6 (fetchAvailableModels)
        // 全程使用同一个代理 IP，杜绝导入瞬间跨 3~4 个 IP 跳变触发风控。
        let temp_bound_id = if let Some(pool) = crate::proxy::proxy_pool::get_global_proxy_pool() {
            let tid = format!("tmp_import_{}", uuid::Uuid::new_v4());
            match pool
                .pin_transient_proxy_for_import(&tid, preferred_proxy_id)
                .await
            {
                Ok(Some(_)) => Some(tid),
                _ => None,
            }
        } else {
            None
        };
        let probe_ctx = temp_bound_id.as_deref();

        // 1. 获取 Token
        let token_res = match modules::oauth::refresh_access_token(refresh_token, probe_ctx).await {
            Ok(v) => v,
            Err(e) => {
                if let (Some(tid), Some(pool)) = (&temp_bound_id, crate::proxy::proxy_pool::get_global_proxy_pool()) {
                    pool.unpin_transient_proxy(tid);
                }
                return Err(e);
            }
        };

        // 2. 获取用户信息
        let user_info = match modules::oauth::get_user_info(&token_res.access_token, probe_ctx).await {
            Ok(v) => v,
            Err(e) => {
                if let (Some(tid), Some(pool)) = (&temp_bound_id, crate::proxy::proxy_pool::get_global_proxy_pool()) {
                    pool.unpin_transient_proxy(tid);
                }
                return Err(e);
            }
        };

        // 3. 获取项目 ID (尝试)
        let project_id = crate::proxy::project_resolver::fetch_project_id(
            &token_res.access_token,
            probe_ctx,
        )
        .await
        .ok();

        // 4. 构造 TokenData
        let token = TokenData::new(
            token_res.access_token.clone(),
            refresh_token.to_string(),
            token_res.expires_in,
            Some(user_info.email.clone()),
            project_id.clone(),
            None,
            false, // 个人账号默认不开启 GCP TOS
            token_res.id_token.clone(),
        )
        .with_oauth_client_key(token_res.oauth_client_key.clone());

        // 5. 持久化
        let upsert_res =
            modules::upsert_account(user_info.email.clone(), user_info.get_display_name(), token);

        // 将导入探测阶段所用的同一出口 IP 粘性移交给新落盘的真实 account.id，并清理临时覆盖
        let mut account = match upsert_res {
            Ok(acc) => {
                if let (Some(tid), Some(pool)) =
                    (&temp_bound_id, crate::proxy::proxy_pool::get_global_proxy_pool())
                {
                    pool.inherit_sticky_egress(tid, &acc.id);
                    pool.unpin_transient_proxy(tid);
                }
                acc
            }
            Err(e) => {
                if let (Some(tid), Some(pool)) =
                    (&temp_bound_id, crate::proxy::proxy_pool::get_global_proxy_pool())
                {
                    pool.unpin_transient_proxy(tid);
                }
                return Err(e);
            }
        };

        // 6. 自动获取配额信息（批量导入紧接着要绑定专属代理时跳过，避免在未绑定代理前走随机 IP；
        //    单条添加时复用第 3 步已拿到的 project_id，避免 1 秒内重复打两次 loadCodeAssist）
        if !skip_initial_quota {
            let email_for_log = account.email.clone();
            let access_token = token_res.access_token.clone();
            match modules::quota::fetch_quota_with_cache(
                &access_token,
                &email_for_log,
                project_id.as_deref(),
                Some(&account.id),
            )
            .await
            {
                Ok((quota_data, new_project_id)) => {
                    account.quota = Some(quota_data);
                    if let Some(pid) = new_project_id {
                        account.token.project_id = Some(pid);
                    }
                    // 保存更新后的账号信息
                    if let Err(e) = modules::account::save_account(&account) {
                        modules::logger::log_warn(&format!(
                            "[Service] Failed to save quota for {}: {}",
                            email_for_log, e
                        ));
                    } else {
                        modules::logger::log_info(&format!(
                            "[Service] Fetched quota for new account: {}",
                            email_for_log
                        ));
                    }
                }
                Err(e) => {
                    modules::logger::log_warn(&format!(
                        "[Service] Failed to fetch quota for {}: {}",
                        email_for_log, e
                    ));
                }
            }
        }

        modules::logger::log_info(&format!(
            "[Service] Added/Updated account: {}",
            account.email
        ));
        Ok(account)
    }

    /// 免校验直接添加/更新账号（适用于指纹浏览器批量推送等无法直连 Google 探测的场景）
    pub fn add_account_direct(
        &self,
        email: &str,
        refresh_token: &str,
    ) -> Result<Account, String> {
        let clean_email = email.trim();
        if clean_email.is_empty() {
            return Err("Email cannot be empty for direct import".to_string());
        }
        if refresh_token.trim().is_empty() {
            return Err("Refresh token cannot be empty".to_string());
        }

        let token = TokenData::new(
            "".to_string(), // access_token 为空，等待首次调度时异步刷新
            refresh_token.trim().to_string(),
            0, // expires_in = 0 标记需要刷新
            Some(clean_email.to_string()),
            None,
            None,
            false,
            None,
        );

        let account = modules::upsert_account(clean_email.to_string(), None, token)?;
        modules::logger::log_info(&format!(
            "[Service] Directly imported account without OAuth probe: {}",
            account.email
        ));
        Ok(account)
    }

    /// 删除账号逻辑
    pub fn delete_account(&self, account_id: &str) -> Result<(), String> {
        modules::delete_account(account_id)?;
        self.integration.update_tray();
        Ok(())
    }

    /// 切换账号逻辑
    pub async fn switch_account(
        &self,
        account_id: &str,
        target_ide: Option<&str>,
    ) -> Result<(), String> {
        modules::account::switch_account(account_id, target_ide, &self.integration).await
    }

    /// 列表获取
    pub fn list_accounts(&self) -> Result<Vec<Account>, String> {
        modules::list_accounts()
    }

    /// 获取当前 ID
    pub fn get_current_id(&self) -> Result<Option<String>, String> {
        modules::get_current_account_id()
    }

    // --- OAuth 逻辑 ---

    pub async fn prepare_oauth_url(
        &self,
        oauth_client_key: Option<String>,
    ) -> Result<String, String> {
        let handle = match &self.integration {
            modules::integration::SystemManager::Desktop(h) => Some(h.clone()),
            modules::integration::SystemManager::Headless => None,
        };
        modules::oauth_server::prepare_oauth_url(handle, oauth_client_key).await
    }

    pub async fn start_oauth_login(
        &self,
        oauth_client_key: Option<String>,
    ) -> Result<Account, String> {
        let handle = match &self.integration {
            modules::integration::SystemManager::Desktop(h) => Some(h.clone()),
            modules::integration::SystemManager::Headless => None,
        };
        let token_res = modules::oauth_server::start_oauth_flow(handle, oauth_client_key).await?;
        self.process_oauth_token(token_res).await
    }

    pub async fn complete_oauth_login(&self) -> Result<Account, String> {
        let handle = match &self.integration {
            modules::integration::SystemManager::Desktop(h) => Some(h.clone()),
            modules::integration::SystemManager::Headless => None,
        };
        let token_res = modules::oauth_server::complete_oauth_flow(handle).await?;
        self.process_oauth_token(token_res).await
    }

    pub fn cancel_oauth_login(&self) {
        modules::oauth_server::cancel_oauth_flow();
    }

    pub async fn submit_oauth_code(
        &self,
        code: String,
        state: Option<String>,
    ) -> Result<(), String> {
        modules::oauth_server::submit_oauth_code(code, state).await
    }

    async fn process_oauth_token(
        &self,
        token_res: modules::oauth::TokenResponse,
    ) -> Result<Account, String> {
        let refresh_token = token_res
            .refresh_token
            .ok_or_else(|| "未获取到 Refresh Token。请撤销权限后重试。".to_string())?;

        // [FIX #1583] 生成临时 UUID 作为账号上下文
        let temp_account_id = uuid::Uuid::new_v4().to_string();

        let user_info =
            modules::oauth::get_user_info(&token_res.access_token, Some(&temp_account_id)).await?;
        let project_id = crate::proxy::project_resolver::fetch_project_id(
            &token_res.access_token,
            Some(&temp_account_id),
        )
        .await
        .ok();

        let token_data = crate::models::TokenData::new(
            token_res.access_token,
            refresh_token,
            token_res.expires_in,
            Some(user_info.email.clone()),
            project_id,
            None,
            false, // 默认不开启，由后续逻辑或用户手动调整
            token_res.id_token,
        )
        .with_oauth_client_key(token_res.oauth_client_key.clone());

        let account = modules::upsert_account(
            user_info.email.clone(),
            user_info.get_display_name(),
            token_data,
        )?;

        // 发送 UI 更新通知 (通过 integration)
        self.integration.update_tray();

        Ok(account)
    }
}
