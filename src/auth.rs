use crate::api::models::LoginOutcome;
use crate::api::ApiClient;
use crate::config::Config;
use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Persist a successful login into the config.
pub fn apply_login(config: &mut Config, outcome: &LoginOutcome) -> Result<()> {
    config.token = Some(outcome.token.clone());
    if let Some(user_id) = &outcome.user_id {
        config.user_id = Some(user_id.clone());
    }
    config.save()?;
    Ok(())
}

fn prepare_config(config: &mut Config) {
    config.ensure_client_id();
    config.ensure_system_id();
    config.ensure_device_id().ok();
}

/// Register this client with UU and persist the server-assigned device id.
/// The official Windows client does this before account operations; a random
/// client-side id is rejected by signed endpoints.
pub async fn ensure_registered(config: &mut Config) -> Result<()> {
    prepare_config(config);
    if config.device_registered {
        return Ok(());
    }
    let client_id = config.ensure_client_id();
    let system_id = config.ensure_system_id();
    let device_id = config.ensure_device_id()?;
    let name = config
        .device_name
        .clone()
        .unwrap_or_else(crate::config::default_device_name);
    let api = ApiClient::from_config(config, None)?;
    let (server_device_id, _) = api
        .init_device(&name, &client_id, &system_id, &device_id)
        .await?;
    config.device_id = Some(server_device_id);
    config.device_registered = true;
    config.save()?;
    Ok(())
}

pub async fn login_mobile(
    config: &mut Config,
    country_code: &str,
    mobile: &str,
    code: &str,
) -> Result<LoginOutcome> {
    ensure_registered(config).await?;
    let api = ApiClient::from_config(config, None)?;
    let outcome = api.login_mobile(country_code, mobile, code).await?;
    apply_login(config, &outcome)?;
    Ok(outcome)
}

pub async fn login_token(config: &mut Config, token: &str) -> Result<LoginOutcome> {
    ensure_registered(config).await?;
    let api = ApiClient::from_config(config, None)?;
    let outcome = api.login_uu_token(token).await?;
    apply_login(config, &outcome)?;
    Ok(outcome)
}

pub async fn login_mumu_token(config: &mut Config, token: &str) -> Result<LoginOutcome> {
    ensure_registered(config).await?;
    let api = ApiClient::from_config(config, None)?;
    let outcome = api.login_mumu_token(token).await?;
    apply_login(config, &outcome)?;
    Ok(outcome)
}

/// Refresh user info and keep `user_id` in sync.
pub async fn refresh_identity(config: &mut Config) -> Result<Value> {
    let token = config.require_token()?;
    ensure_registered(config).await?;
    let mut api = ApiClient::from_config(config, Some(token))?;
    if config.user_id.is_none() {
        if let Some(claims) = crate::util::jwt_claims(api.token().unwrap_or_default()) {
            if let Some(sub) = claims.get("sub").and_then(|v| v.as_str()) {
                api.set_user_id(sub);
            }
        }
    }
    let info = api.user_info().await?;
    if let Some(user_id) = crate::api::value_user_id(&info) {
        if config.user_id.as_deref() != Some(user_id.as_str()) {
            config.user_id = Some(user_id);
            config.save()?;
        }
    }
    Ok(info)
}

/// Token -> user id cache used for authenticated API calls.
pub struct AuthCache {
    api_base: String,
    skip_auth: bool,
    ttl: Duration,
    inner: Mutex<HashMap<String, (String, Instant)>>,
}

impl AuthCache {
    pub fn new(api_base: impl Into<String>, skip_auth: bool) -> Self {
        Self {
            api_base: api_base.into(),
            skip_auth,
            ttl: Duration::from_secs(600),
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn authenticate(&self, token: &str) -> Result<String> {
        let token = crate::util::normalize_token(token);
        if token.is_empty() {
            return Err(Error::Unauthorized("empty token".to_owned()));
        }
        if self.skip_auth {
            let user = token
                .strip_prefix("local:")
                .unwrap_or(token.as_str())
                .to_owned();
            return Ok(if user.is_empty() {
                "local".to_owned()
            } else {
                user
            });
        }
        if let Ok(cache) = self.inner.lock() {
            if let Some((user_id, at)) = cache.get(&token) {
                if at.elapsed() < self.ttl {
                    return Ok(user_id.clone());
                }
            }
        }
        let mut api = ApiClient::new(&self.api_base, Some(token.clone()))?;
        if let Some(claims) = crate::util::jwt_claims(&token) {
            let claim = |key: &str| {
                claims
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned)
            };
            if let (Some(client_id), Some(device_id), Some(system_id)) =
                (claim("client_id"), claim("device_id"), claim("system_id"))
            {
                api.set_identity(client_id, device_id, system_id, claim("sub"));
            } else if let Some(sub) = claim("sub") {
                api.set_user_id(sub);
            }
        }
        let info = api.user_info().await.map_err(|err| {
            if err.is_unauthorized() {
                Error::Unauthorized("account token rejected by UU API".to_owned())
            } else {
                err
            }
        })?;
        let user_id = crate::api::value_user_id(&info)
            .ok_or_else(|| Error::Protocol("user info did not contain a user id".to_owned()))?;
        if let Ok(mut cache) = self.inner.lock() {
            cache.insert(token, (user_id.clone(), Instant::now()));
        }
        Ok(user_id)
    }
}
