pub mod models;

use crate::error::{Error, Result};
use crate::util::{find_i64, find_string, normalize_platform, normalize_token};
use hmac::{Hmac, Mac};
use models::{Device, LoginOutcome, RoomInfo};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SIGN_KEY: &[u8] = b"alWiSzXZTLu3WfFnw13uBru3";
const VERSION_NAME: &str = "4.43.0.3607";
const VERSION_CODE: &str = "44303607";
const PACKAGE_NAME: &str = "com.netease.uuremote";
const CHANNEL: &str = "nochannel";
const LANGUAGE: &str = "zh-CN";
const COUNTRY: &str = "CN";
const PLATFORM: &str = "1";

#[derive(Clone)]
pub struct ApiClient {
    http: Client,
    base: String,
    token: Option<String>,
    client_id: String,
    device_id: String,
    system_id: String,
    user_id: Option<String>,
}

impl ApiClient {
    pub fn new(base: impl Into<String>, token: Option<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(format!("openuur/{}", crate::VERSION))
            .build()?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            token: token.map(|t| normalize_token(&t)),
            client_id: uuid::Uuid::new_v4().to_string(),
            device_id: crate::util::random_device_id(),
            system_id: "deaddead-dead-dead-dead-deaddeaddead".to_owned(),
            user_id: None,
        })
    }

    pub fn from_config(config: &crate::config::Config, token: Option<String>) -> Result<Self> {
        let mut client = Self::new(&config.api_base, token)?;
        if let Some(client_id) = &config.client_id {
            if !client_id.trim().is_empty() {
                client.client_id = client_id.clone();
            }
        }
        if let Some(device_id) = &config.device_id {
            if !device_id.trim().is_empty() {
                client.device_id = device_id.clone();
            }
        }
        if let Some(system_id) = &config.system_id {
            if !system_id.trim().is_empty() {
                client.system_id = system_id.clone();
            }
        }
        client.user_id = config.user_id.clone();
        Ok(client)
    }

    pub fn set_identity(
        &mut self,
        client_id: impl Into<String>,
        device_id: impl Into<String>,
        system_id: impl Into<String>,
        user_id: Option<String>,
    ) {
        self.client_id = client_id.into();
        self.device_id = device_id.into();
        self.system_id = system_id.into();
        self.user_id = user_id;
    }

    pub fn set_user_id(&mut self, user_id: impl Into<String>) {
        self.user_id = Some(user_id.into());
    }

    /// The canonical signed header map (lowercase keys) and its HMAC-SHA256
    /// signature.
    ///
    /// Recovered from `GameViewer.exe`: the canonical string is the sorted
    /// `x-param-*` headers joined as `key=value&` (without a trailing `&`),
    /// and the signed message is `METHOD + PATH + canonical + BODY`.
    fn sign(&self, method: &str, path: &str, body: &str) -> (BTreeMap<String, String>, String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();
        let mut nonce_bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce_bytes);
        self.sign_with(method, path, body, &timestamp, &hex::encode(nonce_bytes))
    }

    fn sign_with(
        &self,
        method: &str,
        path: &str,
        body: &str,
        timestamp: &str,
        nonce: &str,
    ) -> (BTreeMap<String, String>, String) {
        let mut headers = BTreeMap::new();
        headers.insert("x-param-plat".to_owned(), PLATFORM.to_owned());
        headers.insert("x-param-vn".to_owned(), VERSION_NAME.to_owned());
        headers.insert("x-param-vc".to_owned(), VERSION_CODE.to_owned());
        headers.insert("x-param-pkgn".to_owned(), PACKAGE_NAME.to_owned());
        headers.insert("x-param-chn".to_owned(), CHANNEL.to_owned());
        headers.insert("x-param-lang".to_owned(), LANGUAGE.to_owned());
        headers.insert("x-param-cnt".to_owned(), COUNTRY.to_owned());
        headers.insert("x-param-client-id".to_owned(), self.client_id.clone());
        headers.insert("x-param-device-id".to_owned(), self.device_id.clone());
        headers.insert("x-param-system-id".to_owned(), self.system_id.clone());
        headers.insert("x-param-ts".to_owned(), timestamp.to_owned());
        headers.insert("x-param-nonce".to_owned(), nonce.to_owned());
        if let Some(user_id) = &self.user_id {
            if !user_id.trim().is_empty() {
                headers.insert("x-param-user-id".to_owned(), user_id.clone());
            }
        }

        let canonical = headers
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let message = format!("{method}{path}{canonical}{body}");
        let mut mac =
            Hmac::<Sha256>::new_from_slice(SIGN_KEY).expect("HMAC accepts keys of any size");
        mac.update(message.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        (headers, signature)
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = Some(normalize_token(&token.into()));
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else if path.starts_with('/') {
            format!("{}{}", self.base, path)
        } else {
            format!("{}/{}", self.base, path)
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
        bearer: Option<&str>,
    ) -> Result<Value> {
        let url = self.url(path);
        let body_text = body.clone().unwrap_or_default();
        let (headers, signature) = self.sign(method.as_str(), path, &body_text);
        let mut req = self
            .http
            .request(method, &url)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, format!("openuur/{}", crate::VERSION));
        if let Some(token) = bearer.or(self.token.as_deref()) {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        for (key, value) in &headers {
            req = req.header(header_name(key), value);
        }
        req = req.header("X-Param-SIGN", signature);
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body);
        }
        let response = req.send().await?;
        response_json(response, &url).await
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.send(Method::GET, path, None, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let body = serde_json::to_string(&body)?;
        self.send(Method::POST, path, Some(body), None).await
    }

    /// `POST /api/v1/login/by_mobile`
    ///
    /// Recovered request body from `GameViewer.exe`:
    /// `{"country_code": "...", "mobile": "...", "code": "..."}`.
    pub async fn login_mobile(
        &self,
        country_code: &str,
        mobile: &str,
        code: &str,
    ) -> Result<LoginOutcome> {
        let raw = self
            .post(
                "/api/v1/login/by_mobile",
                json!({
                    "country_code": country_code,
                    "mobile": mobile,
                    "code": code,
                }),
            )
            .await?;
        login_outcome(raw)
    }

    /// `POST /api/v1/login/by_uu_token`
    ///
    /// Recovered body key: `access_token`.
    pub async fn login_uu_token(&self, access_token: &str) -> Result<LoginOutcome> {
        let raw = self
            .post(
                "/api/v1/login/by_uu_token",
                json!({ "access_token": access_token }),
            )
            .await?;
        login_outcome(raw)
    }

    /// `POST /api/v1/login/by_mumu_token`
    pub async fn login_mumu_token(&self, access_token: &str) -> Result<LoginOutcome> {
        let raw = self
            .post(
                "/api/v1/login/by_mumu_token",
                json!({ "access_token": access_token }),
            )
            .await?;
        login_outcome(raw)
    }

    /// `POST /api/v1/login/by_qrcode`
    ///
    /// The recovered builder sends `qrcode_jump_url` and `token`.
    pub async fn login_qrcode(&self, jump_url: &str, token: &str) -> Result<LoginOutcome> {
        let raw = self
            .post(
                "/api/v1/login/by_qrcode",
                json!({ "qrcode_jump_url": jump_url, "token": token }),
            )
            .await?;
        login_outcome(raw)
    }

    /// `POST /api/v1/qrcode/gen/login`: returns `qrcode_jump_url` (QR
    /// content) and the status-query ticket.
    pub async fn qrcode_gen_login(&self) -> Result<Value> {
        let raw = self
            .send(
                Method::POST,
                "/api/v1/qrcode/gen/login",
                Some(json!({}).to_string()),
                None,
            )
            .await?;
        Ok(raw)
    }

    /// `POST /api/v1/qrcode/login/status`: long poll; `login_status` is the
    /// last state already shown (1 waiting, 2 scanned, 3/4 final).
    pub async fn qrcode_login_status(
        &self,
        token: &str,
        jump_url: &str,
        login_status: i64,
    ) -> Result<Value> {
        let raw = self
            .send(
                Method::POST,
                "/api/v1/qrcode/login/status",
                Some(
                    json!({
                        "token": token,
                        "qrcode_jump_url": jump_url,
                        "login_status": login_status,
                    })
                    .to_string(),
                ),
                None,
            )
            .await?;
        Ok(raw)
    }

    pub async fn check_uu_token(&self, access_token: &str) -> Result<Value> {
        self.post(
            "/api/v1/login/check_uu_token",
            json!({ "access_token": access_token }),
        )
        .await
    }

    /// `POST /api/v1/security/mobile/code`
    pub async fn request_mobile_code(&self, country_code: &str, mobile: &str) -> Result<Value> {
        // Recovered body from the SMS request builder:
        // {"country_code": "...", "mobile": "...", "type": "login"}
        self.post(
            "/api/v1/security/mobile/code",
            json!({
                "country_code": country_code,
                "mobile": mobile,
                "type": "login",
            }),
        )
        .await
    }

    /// `POST /api/v1/user/logout`
    pub async fn logout(&self) -> Result<Value> {
        self.post("/api/v1/user/logout", json!({})).await
    }

    /// `GET /api/v1/user/info`
    pub async fn user_info(&self) -> Result<Value> {
        self.get("/api/v1/user/info").await
    }

    /// `POST /api/v1/device/windows/init`
    ///
    /// Registers a fresh client identity and returns the server-assigned
    /// device id. Recovered body fields from the Windows client:
    /// `name`, `client_id`, `system_id`, `machine_guid`, `os`, `base_board`,
    /// `cpu`, `video` (list), `mac`, `memory`, `screen`.
    pub async fn init_device(
        &self,
        name: &str,
        client_id: &str,
        system_id: &str,
        _device_id: &str,
    ) -> Result<(String, Value)> {
        let body = json!({
            "name": name,
            "client_id": client_id,
            "system_id": system_id,
            "machine_guid": client_id,
            "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            "base_board": "openuur",
            "cpu": std::env::consts::ARCH,
            "video": ["openuur"],
            "mac": format!("{:012x}", {
                let mut b = [0u8; 6];
                rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
                u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]])
            }),
            "memory": "0",
            "screen": "0x0",
        });
        tracing::debug!(body = %body, "device init request");
        let raw = self.post("/api/v1/device/windows/init", body).await?;
        tracing::debug!(response = %raw, "device init response");
        let server_device_id = find_string(&raw, &["device_id", "deviceId"])
            .ok_or_else(|| Error::Protocol("device init did not return device_id".to_owned()))?;
        Ok((server_device_id, raw))
    }

    /// `GET /api/v1/device/list`
    pub async fn device_list(&self) -> Result<Vec<Device>> {
        let raw = self.get("/api/v1/device/list").await?;
        Ok(parse_devices(&raw))
    }

    /// `GET /api/v1/device/controllable`
    pub async fn controllable_devices(&self) -> Result<Vec<Device>> {
        let raw = self.get("/api/v1/device/controllable").await?;
        Ok(parse_devices(&raw))
    }

    /// `GET /api/v1/device/{id}`
    pub async fn device_detail(&self, device_id: &str) -> Result<Value> {
        self.get(&format!("/api/v1/device/{device_id}")).await
    }

    /// Rename a device.
    ///
    /// The live API wants `PUT /api/v1/device/{id}` with an `alias` field
    /// (a bare `device_name` is rejected with `missing field "alias"`).
    /// Older shapes are kept as fallbacks for other account backends.
    pub async fn rename_device(&self, device_id: &str, name: &str) -> Result<Value> {
        let path = format!("/api/v1/device/{device_id}");
        let bodies = [
            json!({ "alias": name }),
            json!({ "alias": name, "device_name": name }),
            json!({ "device_name": name }),
        ];
        let methods = [Method::PUT, Method::POST];
        let mut last_error = None;
        for method in methods {
            for body in &bodies {
                let body = serde_json::to_string(body)?;
                tracing::debug!("trying device rename: {method} {path} {body}");
                match self.send(method.clone(), &path, Some(body), None).await {
                    Ok(value) => {
                        tracing::info!("device rename accepted: {method} {path}");
                        return Ok(value);
                    }
                    Err(err) if err.is_shape_rejected() => {
                        if let Error::Api { body, .. } = &err {
                            tracing::debug!("device rename rejected: {method} {path}: {body}");
                        }
                        last_error = Some(err);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        // Last-resort shape seen in some account APIs: update a preference.
        let body = serde_json::to_string(&json!({ "device_name": name }))?;
        match self
            .send(
                Method::POST,
                &format!("{path}/preferences"),
                Some(body),
                None,
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(err) if err.is_shape_rejected() => Err(last_error.unwrap_or(err)),
            Err(err) => Err(err),
        }
    }

    /// `POST /api/v1/device/wallpaper`
    ///
    /// Recovered body: `{"wallpaper_url": "<url>"}`. We include `device_id`
    /// when supplied because the account may host several devices; servers
    /// that ignore unknown fields are unaffected.
    pub async fn set_wallpaper(&self, device_id: Option<&str>, url: &str) -> Result<Value> {
        let body = match device_id {
            Some(id) => json!({ "wallpaper_url": url, "device_id": id }),
            None => json!({ "wallpaper_url": url }),
        };
        match self.post("/api/v1/device/wallpaper", body).await {
            Ok(value) => Ok(value),
            Err(Error::Api { status: 400, .. }) if device_id.is_some() => {
                self.post("/api/v1/device/wallpaper", json!({ "wallpaper_url": url }))
                    .await
            }
            Err(err) => Err(err),
        }
    }

    /// Upload a local image to a caller-selected multipart endpoint and return
    /// the resulting URL. UU's own file-picker upload flow is not fully
    /// recovered, so this endpoint is configurable.
    pub async fn upload_file(&self, endpoint: &str, path: &Path) -> Result<String> {
        let bytes = tokio::fs::read(path).await?;
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("wallpaper.png")
            .to_owned();
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .map_err(|e| Error::Protocol(format!("invalid upload mime type: {e}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let url = self.url(endpoint);
        let mut req = self.http.post(&url).multipart(form);
        if let Some(token) = self.token.as_deref() {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = req.send().await?;
        let value = response_json(response, &url).await?;
        find_string(
            &value,
            &["url", "download_url", "file_url", "fileUrl", "src"],
        )
        .ok_or_else(|| Error::Protocol("upload response did not contain a URL".to_owned()))
    }

    /// `GET /api/v1/tool/fp/token?filetype=1`
    ///
    /// UU's file-picker token endpoint. The response shape is not fully
    /// recoverable from the static sample, so callers receive the raw JSON and
    /// the upload path extracts common token/URL names.
    pub async fn upload_token(&self) -> Result<Value> {
        self.get("/api/v1/tool/fp/token?filetype=1").await
    }

    /// Upload a wallpaper through UU's file picker and return its public URL.
    ///
    /// The token endpoint is known; the exact multipart field names of the
    /// picker are not. We try the observed UU-style `file` + `token` form,
    /// then a raw PUT with the token headers, and accept only a URL returned
    /// by the server. `UU_WALLPAPER_UPLOAD_URL` / config `upload_url` can
    /// override the upload endpoint for deployments with a different picker.
    pub async fn upload_wallpaper(
        &self,
        path: &Path,
        override_endpoint: Option<&str>,
    ) -> Result<String> {
        let bytes = tokio::fs::read(path).await?;
        if bytes.is_empty() {
            return Err(Error::Protocol("wallpaper file is empty".to_owned()));
        }
        if bytes.len() > 32 * 1024 * 1024 {
            return Err(Error::Protocol(
                "wallpaper is larger than UU's 32 MiB upload limit".to_owned(),
            ));
        }
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("wallpaper.jpg")
            .to_owned();
        let mime = match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/jpeg",
        };

        let token_response = self.upload_token().await.unwrap_or(Value::Null);
        let token = find_string(
            &token_response,
            &["token", "upload_token", "access_token", "file_token"],
        );
        let endpoint = override_endpoint
            .map(str::to_owned)
            .or_else(|| {
                find_string(
                    &token_response,
                    &[
                        "req_url",
                        "reqUrl",
                        "upload_url",
                        "uploadUrl",
                        "endpoint",
                        "url",
                        "host",
                    ],
                )
            })
            .or_else(|| std::env::var("UU_WALLPAPER_UPLOAD_URL").ok())
            .unwrap_or_else(|| "https://nrd-ugc.fp.ps.netease.com/file/".to_owned());
        let endpoint = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint
        } else {
            format!("https://{endpoint}")
        };

        let part = reqwest::multipart::Part::bytes(bytes.clone())
            .file_name(file_name.clone())
            .mime_str(mime)
            .map_err(|error| Error::Protocol(format!("invalid wallpaper mime: {error}")))?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        form = form.text("filetype", "1");
        if let Some(token) = &token {
            form = form.text("token", token.clone());
        }
        let mut request = self.http.post(&endpoint).multipart(form);
        if let Some(token) = &token {
            request = request
                .header("Token", token)
                .header("X-Nos-Token", token)
                .header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            let value = response_json(response, &endpoint).await?;
            if let Some(url) = find_string(
                &value,
                &[
                    "url",
                    "download_url",
                    "file_url",
                    "fileUrl",
                    "src",
                    "location",
                ],
            ) {
                return Ok(url);
            }
        }

        // A configured endpoint may be a plain upload service that accepts a
        // raw PUT. The official file picker does not (it returns 405), so this
        // is only attempted for an explicit override.
        if override_endpoint.is_some() {
            let mut request = self
                .http
                .put(&endpoint)
                .header(reqwest::header::CONTENT_TYPE, mime)
                .body(bytes);
            if let Some(token) = &token {
                request = request
                    .header("Token", token)
                    .header("X-Nos-Token", token)
                    .header(AUTHORIZATION, format!("Bearer {token}"));
            }
            let response = request.send().await?;
            let value = response_json(response, &endpoint).await?;
            if let Some(url) = find_string(
                &value,
                &[
                    "url",
                    "download_url",
                    "file_url",
                    "fileUrl",
                    "src",
                    "location",
                ],
            ) {
                return Ok(url);
            }
        }

        Err(Error::Protocol(format!(
            "wallpaper upload endpoint rejected the request (HTTP {status}); \
             the official file picker needs a deployment-specific token header. \
             Set UU_WALLPAPER_UPLOAD_URL to a reachable upload endpoint."
        )))
    }

    /// `POST /api/v1/room/create` with the host's empty body.
    pub async fn create_host_room(&self) -> Result<RoomInfo> {
        let raw = self.post("/api/v1/room/create", json!({})).await?;
        Ok(RoomInfo { raw })
    }

    /// `POST /api/v1/room/create`
    pub async fn create_room(&self, last_controlled_interval: i64) -> Result<RoomInfo> {
        let raw = self
            .post(
                "/api/v1/room/create",
                json!({ "last_controlled_interval": last_controlled_interval }),
            )
            .await?;
        Ok(RoomInfo { raw })
    }

    /// `POST /api/v1/room/join/by_device/{device_id}`
    pub async fn join_room(&self, device_id: &str, force_join: bool) -> Result<RoomInfo> {
        let raw = self
            .post(
                &format!("/api/v1/room/join/by_device/{device_id}"),
                json!({ "force_join": force_join }),
            )
            .await?;
        Ok(RoomInfo { raw })
    }

    /// Resolve a device by exact id first, then by unique name.
    pub async fn resolve_device(&self, query: &str) -> Result<Device> {
        let devices = self.device_list().await?;
        resolve_in_devices(&devices, query)
    }
}

pub fn resolve_in_devices(devices: &[Device], query: &str) -> Result<Device> {
    if let Some(device) = devices.iter().find(|d| d.device_id == query) {
        return Ok(device.clone());
    }
    let matches: Vec<_> = devices
        .iter()
        .filter(|d| d.device_name == query)
        .cloned()
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => Err(Error::DeviceNotFound(query.to_owned())),
        _ => Err(Error::AmbiguousDevice {
            name: query.to_owned(),
            matches: matches
                .iter()
                .map(|d| format!("{} ({})", d.device_name, d.device_id))
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

fn login_outcome(raw: Value) -> Result<LoginOutcome> {
    let token = find_string(
        &raw,
        &[
            "token",
            "access_token",
            "accessToken",
            "uu_token",
            "uuToken",
            "auth_token",
            "authorization",
        ],
    )
    .map(|t| normalize_token(&t))
    .filter(|t| !t.is_empty())
    .ok_or_else(|| Error::Protocol("login response did not contain a token".to_owned()))?;
    let user_id = find_string(
        &raw,
        &["user_id", "userId", "uid", "userIdStr", "accountId"],
    );
    Ok(LoginOutcome {
        token,
        user_id,
        raw,
    })
}

fn parse_devices(raw: &Value) -> Vec<Device> {
    let mut out = Vec::new();
    collect_device_values(raw, &mut out, 0);
    let mut seen = std::collections::HashSet::new();
    out.into_iter()
        .filter(|device| seen.insert(device.device_id.clone()))
        .collect()
}

fn collect_device_values(value: &Value, out: &mut Vec<Device>, depth: usize) {
    if depth > 8 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                if let Some(device) = Device::from_value(item) {
                    out.push(device);
                } else {
                    collect_device_values(item, out, depth + 1);
                }
            }
        }
        Value::Object(map) => {
            for key in [
                "devices",
                "list",
                "items",
                "data",
                "result",
                "device_list",
                "deviceList",
                "current_device",
                "my_binded_devices",
                "others_shared_devices",
                "shared_devices",
                "online_devices",
            ] {
                if let Some(v) = map.get(key) {
                    collect_device_values(v, out, depth + 1);
                }
            }
            if out.is_empty() && (map.contains_key("deviceId") || map.contains_key("device_id")) {
                if let Some(device) = Device::from_value(value) {
                    out.push(device);
                }
            }
        }
        _ => {}
    }
}

async fn response_json(response: Response, url: &str) -> Result<Value> {
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(Error::Unauthorized(format!("{url}: HTTP {status}")));
        }
        return Err(Error::api(status.as_u16(), text));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    match serde_json::from_str(&text) {
        Ok(value) => Ok(value),
        Err(_) => Ok(Value::String(text)),
    }
}

/// Extract a numeric platform or user id from a JSON value.
pub fn value_user_id(value: &Value) -> Option<String> {
    find_string(value, &["user_id", "userId", "uid", "accountId", "id"])
}

pub fn value_platform(value: &Value) -> Option<String> {
    value.get("platform").and_then(normalize_platform)
}

pub fn value_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    find_i64(value, keys)
}

fn header_name(lower: &str) -> &'static str {
    match lower {
        "x-param-plat" => "X-Param-PLAT",
        "x-param-vn" => "X-Param-VN",
        "x-param-vc" => "X-Param-VC",
        "x-param-pkgn" => "X-Param-PKGN",
        "x-param-chn" => "X-Param-CHN",
        "x-param-lang" => "X-Param-LANG",
        "x-param-cnt" => "X-Param-CNT",
        "x-param-client-id" => "X-Param-client-id",
        "x-param-device-id" => "X-Param-device-id",
        "x-param-system-id" => "X-Param-System-ID",
        "x-param-user-id" => "X-Param-user-id",
        "x-param-ts" => "X-Param-TS",
        "x-param-nonce" => "X-Param-Nonce",
        _ => "X-Param-Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_recovered_algorithm() {
        let mut client = ApiClient::new("https://api.nrd.nie.163.com", None).unwrap();
        client.set_identity(
            "11111111-1111-4111-8111-111111111111",
            "aeawvoirq4bdb5uk",
            "deaddead-dead-dead-dead-deaddeaddead",
            None,
        );
        let body = r#"{"country_code":"86","mobile":"19900000000","type":"login"}"#;
        let (_, signature) = client.sign_with(
            "POST",
            "/api/v1/security/mobile/code",
            body,
            "1700000000",
            "00112233445566778899aabbccddeeff",
        );
        assert_eq!(
            signature,
            "fde2614ed5acfcee20c79810e7297c2cc1649461fcbf7db9024d8daaa5c56432"
        );
    }
}
