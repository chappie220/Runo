//! A ChatGPT subscription as the model provider, the way the Codex CLI uses
//! it: sign in with the ChatGPT account (OAuth with PKCE), keep the tokens
//! fresh, and call the Codex backend's Responses API.
//!
//! The tokens live in `chatgpt-auth.json` under the state directory, owner
//! readable only. OpenAI rotates the refresh token on every refresh, so one
//! login must not be shared with another program that refreshes it too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::i18n::{Tr, tr};
use crate::llm::{ChatMessage, Completion, FunctionCall, Role, ToolCall, ToolSpec, Usage};

pub const ISSUER: &str = "https://auth.openai.com";
/// The Codex CLI's public OAuth client.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Codex's Responses endpoint for ChatGPT accounts.
pub const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";
/// The redirect registered for the client; the port is fixed.
const CALLBACK_PORT: u16 = 1455;
const REDIRECT: &str = "http://localhost:1455/auth/callback";
pub const AUTH_FILE: &str = "chatgpt-auth.json";
/// The model set by `login` when none fits the ChatGPT backend yet.
pub const DEFAULT_MODEL: &str = "gpt-5";
/// Renew this long before the access token expires.
const EARLY: i64 = 300;
/// Without a readable expiry, renew after this long, as Codex does.
const MAX_AGE: i64 = 8 * 86_400;

const NOT_SIGNED_IN: Tr = tr(
    "not signed in to ChatGPT: run `openclaw-rs login`",
    "还没有登录 ChatGPT：运行 `openclaw-rs login`",
);
const EXPIRED: Tr = tr(
    "the ChatGPT login has expired or was used elsewhere ({}): run `openclaw-rs login` again",
    "ChatGPT 登录已过期或在别处被使用（{}）：请重新运行 `openclaw-rs login`",
);
const OPEN_URL: Tr = tr(
    "Open this address in a browser and sign in with your ChatGPT account:\n\n{}\n\nIf the browser runs on this machine, that is all. Otherwise the last page will not load (it points at localhost:1455): copy its whole address from the address bar and paste it here, then press Enter. Or forward the port first: ssh -L 1455:localhost:1455 <this host>.",
    "在浏览器里打开下面的地址，用你的 ChatGPT 账号登录：\n\n{}\n\n如果浏览器就在这台机器上，登录后就完成了。否则最后一页会打不开（它指向 localhost:1455）：把地址栏里的完整地址复制下来粘贴到这里，再按回车。也可以先转发端口：ssh -L 1455:localhost:1455 <这台主机>。",
);
const PASTE: Tr = tr("Address or code: ", "地址或授权码：");
const BAD_PASTE: Tr = tr(
    "That is not the address the sign-in ended on (or its state does not match); try again.",
    "这不是登录最后跳转到的地址（或 state 不匹配），请再试一次。",
);
const PORT_BUSY: Tr = tr(
    "(Port 1455 is busy, so only pasting works this time.)",
    "（1455 端口被占用，这次只能粘贴地址。）",
);
const CALLBACK_PAGE: &str = "<!doctype html><meta charset=utf-8><title>openclaw-rs</title>\
    <p style=\"font:16px sans-serif\">Signed in. You can close this tab. 已登录，可以关闭此页面。</p>";
pub const SIGNED_IN: Tr = tr(
    "Signed in to ChatGPT{}. Saved to {}.",
    "已登录 ChatGPT{}。已保存到 {}。",
);
pub const SWITCHED: Tr = tr(
    "The model now runs on your ChatGPT subscription: model.provider = \"chatgpt\", model.model = \"{}\" (change it with `openclaw-rs config`).",
    "模型现在使用你的 ChatGPT 订阅：model.provider = \"chatgpt\"，model.model = \"{}\"（可以用 `openclaw-rs config` 修改）。",
);
pub const IMPORTED_WARNING: Tr = tr(
    "Imported from Codex. Each refresh replaces the refresh token, so whichever program refreshes second is signed out; prefer `openclaw-rs login` for a login of its own.",
    "已从 Codex 导入。每次刷新都会换掉 refresh token，所以两个程序共用时后刷新的那个会被登出；最好用 `openclaw-rs login` 单独登录一次。",
);
pub const SIGNED_OUT: Tr = tr("Signed out of ChatGPT.", "已退出 ChatGPT 登录。");
const STATUS: Tr = tr(
    "Signed in to ChatGPT{} ({}); the model provider is {}.",
    "已登录 ChatGPT{}（{}）；模型提供方是 {}。",
);
const NOT_IN_USE: Tr = tr(
    " Run `openclaw-rs login` again to switch the model to it.",
    "再运行一次 `openclaw-rs login` 就会把模型切换过去。",
);
const USAGE_LIMIT: Tr = tr(
    "the ChatGPT plan's usage limit is reached{}: {}",
    "ChatGPT 套餐的用量已达上限{}：{}",
);
const RESETS_IN: Tr = tr(" (resets in about {} min)", "（约 {} 分钟后重置）");

#[cfg(test)]
const TEXTS: &[Tr] = &[
    NOT_SIGNED_IN,
    EXPIRED,
    OPEN_URL,
    PASTE,
    BAD_PASTE,
    PORT_BUSY,
    SIGNED_IN,
    SWITCHED,
    IMPORTED_WARNING,
    SIGNED_OUT,
    STATUS,
    NOT_IN_USE,
    USAGE_LIMIT,
    RESETS_IN,
];

/// What a sign-in gives, as saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
    /// Unix seconds of the last sign-in or refresh.
    #[serde(default)]
    pub last_refresh: i64,
}

/// A JWT's claims, unverified: only read for display and expiry.
fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

impl Tokens {
    /// Tokens from a token endpoint response; a refresh may leave some out,
    /// which `old` then supplies.
    fn from_response(body: &Value, old: Option<&Tokens>) -> Result<Self> {
        let field = |name: &str| {
            body[name]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    old.map(|o| match name {
                        "id_token" => o.id_token.clone(),
                        "access_token" => o.access_token.clone(),
                        _ => o.refresh_token.clone(),
                    })
                })
                .with_context(|| format!("the sign-in gave no {name}"))
        };
        let mut tokens = Tokens {
            id_token: field("id_token")?,
            access_token: field("access_token")?,
            refresh_token: field("refresh_token")?,
            account_id: None,
            last_refresh: crate::store::now(),
        };
        tokens.account_id = tokens
            .auth_claim("chatgpt_account_id")
            .or_else(|| old.and_then(|o| o.account_id.clone()));
        Ok(tokens)
    }

    /// A claim under `https://api.openai.com/auth` in the ID token, else
    /// in the access token.
    fn auth_claim(&self, name: &str) -> Option<String> {
        [&self.id_token, &self.access_token].iter().find_map(|t| {
            claims(t)?["https://api.openai.com/auth"][name]
                .as_str()
                .map(str::to_owned)
        })
    }

    pub fn email(&self) -> Option<String> {
        claims(&self.id_token)?["email"].as_str().map(str::to_owned)
    }

    pub fn plan(&self) -> Option<String> {
        self.auth_claim("chatgpt_plan_type")
    }

    /// Whether the access token is about to expire (or may have).
    fn stale(&self, now: i64) -> bool {
        match claims(&self.access_token).and_then(|c| c["exp"].as_i64()) {
            Some(exp) => exp - EARLY <= now,
            None => now - self.last_refresh >= MAX_AGE,
        }
    }

    /// " as <email> (<plan>)", for messages.
    pub fn who(&self) -> String {
        match (self.email(), self.plan()) {
            (Some(email), Some(plan)) => format!(" ({email}, {plan})"),
            (Some(email), None) => format!(" ({email})"),
            (None, Some(plan)) => format!(" ({plan})"),
            (None, None) => String::new(),
        }
    }
}

pub fn auth_path(state: &Path) -> PathBuf {
    state.join(AUTH_FILE)
}

pub fn read(path: &Path) -> Result<Tokens> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(NOT_SIGNED_IN.now()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("{} is damaged", path.display()))
}

pub fn write(path: &Path, tokens: &Tokens) -> Result<()> {
    let text = serde_json::to_string_pretty(tokens)?;
    crate::setup::write_private(path, &text)
}

/// The tokens of one auth file, shared by every client in the process so
/// a refresh is never done twice with the same (single-use) refresh token.
pub struct Auth {
    path: PathBuf,
    issuer: String,
    http: reqwest::Client,
    tokens: tokio::sync::Mutex<Option<Tokens>>,
}

impl Auth {
    /// The process's `Auth` for `path`.
    pub fn shared(path: &Path, issuer: &str) -> Arc<Self> {
        static ALL: OnceLock<Mutex<HashMap<PathBuf, Arc<Auth>>>> = OnceLock::new();
        let mut all = ALL
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        all.entry(path.to_owned())
            .or_insert_with(|| {
                Arc::new(Auth {
                    path: path.to_owned(),
                    issuer: issuer.trim_end_matches('/').to_owned(),
                    http: http(),
                    tokens: tokio::sync::Mutex::new(None),
                })
            })
            .clone()
    }

    /// A usable access token and the account it is for, refreshed first
    /// when it is about to expire or when `force` says it was refused.
    pub async fn access(&self, force: bool) -> Result<(String, Option<String>)> {
        let mut held = self.tokens.lock().await;
        // The file wins when it is newer: a `login` while the Gateway runs.
        let on_disk = read(&self.path)?;
        let tokens = match held.take() {
            Some(mine) if mine.last_refresh >= on_disk.last_refresh => mine,
            _ => on_disk,
        };
        let now = crate::store::now();
        let tokens = if force || tokens.stale(now) {
            let fresh = refresh(&self.http, &self.issuer, &tokens).await?;
            write(&self.path, &fresh)?;
            fresh
        } else {
            tokens
        };
        let out = (tokens.access_token.clone(), tokens.account_id.clone());
        *held = Some(tokens);
        Ok(out)
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

async fn refresh(http: &reqwest::Client, issuer: &str, tokens: &Tokens) -> Result<Tokens> {
    let response = http
        .post(format!("{issuer}/oauth/token"))
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": tokens.refresh_token,
            "scope": "openid profile email",
        }))
        .send()
        .await
        .context("cannot reach the ChatGPT sign-in server")?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let why = body["error"]["code"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .or_else(|| body["error_description"].as_str())
            .unwrap_or("refused");
        if status.as_u16() == 400 || status.as_u16() == 401 {
            bail!(EXPIRED.with(&[why]));
        }
        bail!("ChatGPT token refresh failed with HTTP {status}: {why}");
    }
    Tokens::from_response(&body, Some(tokens))
}

/// A PKCE verifier and its S256 challenge.
struct Pkce {
    verifier: String,
    challenge: String,
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; n];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| anyhow::anyhow!("no randomness"))?;
    Ok(bytes)
}

fn pkce() -> Result<Pkce> {
    let encode = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let verifier = encode(&random_bytes(64)?);
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
    Ok(Pkce {
        challenge: encode(digest.as_ref()),
        verifier,
    })
}

fn authorize_url(issuer: &str, pkce: &Pkce, state: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("{issuer}/oauth/authorize")).expect("issuer URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codex_cli_rs");
    url.into()
}

/// The code in what was pasted: the address the sign-in ended on, its
/// query, or the bare code. `None` when the state does not match.
fn pasted_code(text: &str, state: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if !text.contains("code=") {
        return (!text.contains(char::is_whitespace) && !text.contains('/'))
            .then(|| text.to_owned());
    }
    let query = text.split_once('?').map_or(text, |(_, q)| q);
    let query = query.split('#').next().unwrap_or(query);
    let pairs: HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{query}"))
        .ok()?
        .query_pairs()
        .into_owned()
        .collect();
    if pairs.get("state").is_some_and(|s| s != state) {
        return None;
    }
    pairs.get("code").filter(|c| !c.is_empty()).cloned()
}

/// Waits on the local callback for the code; answers other requests 404.
async fn callback(listener: tokio::net::TcpListener, state: String) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buffer)).await
        {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buffer[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let code = target
            .starts_with("/auth/callback")
            .then(|| pasted_code(target, &state))
            .flatten();
        let (status, body) = match &code {
            Some(_) => ("200 OK", CALLBACK_PAGE),
            None => ("404 Not Found", "not found"),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
        let _ = socket.shutdown().await;
        if let Some(code) = code {
            return Ok(code);
        }
    }
}

/// Reads pasted lines until one holds the code.
async fn pasted(state: String) -> Result<String> {
    loop {
        eprint!("{}", PASTE.now());
        let line = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|n| (n, line))
        })
        .await??;
        if line.0 == 0 {
            // No terminal to paste in: keep waiting for the callback.
            std::future::pending::<()>().await;
        }
        match pasted_code(&line.1, &state) {
            Some(code) => return Ok(code),
            None => eprintln!("{}", BAD_PASTE.now()),
        }
    }
}

async fn exchange(issuer: &str, code: &str, verifier: &str) -> Result<Tokens> {
    let mut form = reqwest::Url::parse("http://form.invalid/").expect("static URL");
    form.query_pairs_mut()
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code_verifier", verifier);
    let response = http()
        .post(format!("{issuer}/oauth/token"))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form.query().unwrap_or("").to_owned())
        .send()
        .await
        .context("cannot reach the ChatGPT sign-in server")?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("ChatGPT sign-in refused with HTTP {status}: {body}");
    }
    Tokens::from_response(&body, None)
}

/// Signs in in a browser and returns the tokens.
pub async fn login(issuer: &str) -> Result<Tokens> {
    let pkce = pkce()?;
    let state = hex(&random_bytes(16)?);
    eprintln!(
        "{}\n",
        OPEN_URL.with(&[&authorize_url(issuer, &pkce, &state)])
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .ok();
    if listener.is_none() {
        eprintln!("{}", PORT_BUSY.now());
    }
    let from_browser = async {
        match listener {
            Some(listener) => callback(listener, state.clone()).await,
            None => std::future::pending().await,
        }
    };
    let code = tokio::select! {
        code = from_browser => code?,
        code = pasted(state.clone()) => code?,
    };
    eprintln!();
    exchange(issuer, &code, &pkce.verifier).await
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The tokens in a Codex CLI `auth.json`.
pub fn import(path: &Path) -> Result<Tokens> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let file: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    let saved = &file["tokens"];
    if !saved.is_object() {
        bail!(
            "{} holds no ChatGPT sign-in (only an API key?)",
            path.display()
        );
    }
    let mut tokens = Tokens::from_response(saved, None)?;
    if let Some(account) = saved["account_id"].as_str() {
        tokens.account_id = Some(account.to_owned());
    }
    // Unknown age: refresh on first use.
    tokens.last_refresh = 0;
    Ok(tokens)
}

/// What `openclaw-rs login` asks for.
pub enum Action {
    /// Sign in in a browser.
    Login,
    /// Take the sign-in of the Codex CLI.
    Import(PathBuf),
    Status,
    Logout,
}

/// Runs `openclaw-rs login`: signs in (or imports, shows, removes) and
/// switches the model to the subscription.
pub async fn run(action: Action, config: &Path, state: &Path) -> Result<()> {
    let path = auth_path(state);
    let tokens = match action {
        Action::Status => {
            let tokens = read(&path)?;
            let settings = crate::config::Config::load(config)?;
            let provider = if settings.chatgpt() {
                "chatgpt"
            } else {
                "openrouter"
            };
            let mut line = STATUS.with(&[&tokens.who(), &path.display().to_string(), provider]);
            if !settings.chatgpt() {
                line.push_str(NOT_IN_USE.now());
            }
            println!("{line}");
            return Ok(());
        }
        Action::Logout => {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("cannot remove {}", path.display()));
                }
            }
            println!("{}", SIGNED_OUT.now());
            return Ok(());
        }
        Action::Login => login(ISSUER).await?,
        Action::Import(ref from) => import(from)?,
    };
    std::fs::create_dir_all(state).with_context(|| format!("cannot create {}", state.display()))?;
    write(&path, &tokens)?;
    println!(
        "{}",
        SIGNED_IN.with(&[&tokens.who(), &path.display().to_string()])
    );
    let model = use_in_config(config)?;
    println!("{}", SWITCHED.with(&[&model]));
    if matches!(action, Action::Import(_)) {
        println!("{}", IMPORTED_WARNING.now());
    }
    Ok(())
}

/// Where Codex keeps its sign-in.
pub fn codex_auth_file() -> PathBuf {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"));
    home.join("auth.json")
}

/// Points `model` at the ChatGPT subscription in `config`: the provider,
/// and a model the backend knows when the current one is an OpenRouter id
/// or unset. Returns the model now set.
pub fn use_in_config(config: &Path) -> Result<String> {
    let mut doc = crate::setup::load(config)?;
    if !doc.contains_table("model") {
        doc["model"] = toml_edit::table();
    }
    let model = &mut doc["model"];
    model["provider"] = toml_edit::value("chatgpt");
    let current = model
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let chosen = if current.is_empty() || current.contains('/') {
        model["model"] = toml_edit::value(DEFAULT_MODEL);
        DEFAULT_MODEL.to_owned()
    } else {
        current
    };
    let text = doc.to_string();
    toml::from_str::<crate::config::Config>(&text).context("the new settings do not load")?;
    crate::setup::write_private(config, &text)?;
    Ok(chosen)
}

/// The Responses API request for a chat: the first system message becomes
/// the instructions, later ones developer messages.
pub fn request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    effort: Option<&str>,
) -> Value {
    let mut instructions = None;
    let mut input = Vec::new();
    let text_item = |role: &str, kind: &str, text: &str| json!({ "type": "message", "role": role, "content": [{ "type": kind, "text": text }] });
    for message in messages {
        let text = message.content.as_deref().unwrap_or("");
        match message.role {
            Role::System if instructions.is_none() => instructions = Some(text.to_owned()),
            Role::System => input.push(text_item("developer", "input_text", text)),
            Role::User => {
                let mut parts = vec![json!({ "type": "input_text", "text": text })];
                parts.extend(
                    message
                        .images
                        .iter()
                        .map(|url| json!({ "type": "input_image", "image_url": url })),
                );
                input.push(json!({ "type": "message", "role": "user", "content": parts }));
            }
            Role::Assistant => {
                if !text.is_empty() {
                    input.push(text_item("assistant", "output_text", text));
                }
                for call in message.tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    }));
                }
            }
            Role::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.as_deref().unwrap_or(""),
                "output": text,
            })),
        }
    }
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.function.name,
                "description": t.function.description,
                "parameters": t.function.parameters,
                "strict": false,
            })
        })
        .collect();
    let mut body = json!({
        "model": model,
        "instructions": instructions.unwrap_or_default(),
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
    });
    if let Some(effort) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    body
}

/// A refused request's message, made readable for the usage limit.
pub fn error_message(status: reqwest::StatusCode, body: &str) -> (String, bool) {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &value["error"];
    let message = error["message"]
        .as_str()
        .or_else(|| value["detail"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| body.chars().take(500).collect());
    if error["type"] == "usage_limit_reached" || error["code"] == "usage_limit_reached" {
        let resets = error["resets_in_seconds"]
            .as_u64()
            .map(|s| RESETS_IN.with(&[&s.div_ceil(60).to_string()]))
            .unwrap_or_default();
        return (USAGE_LIMIT.with(&[&resets, &message]), true);
    }
    (
        format!("model request failed with HTTP {status}: {message}"),
        false,
    )
}

/// Incremental decoder of the Responses API's server-sent events.
#[derive(Default)]
pub struct ResponsesParser {
    buffer: Vec<u8>,
    text: String,
    calls: Vec<ToolCall>,
    usage: Option<Usage>,
    model: Option<String>,
    finish_reason: Option<String>,
    /// Pages the answer cites (title, URL), from web searches.
    sources: Vec<(String, String)>,
    pub done: bool,
}

impl ResponsesParser {
    /// Feeds `bytes`, passing assistant text to `on_text` as it arrives.
    pub fn push(&mut self, bytes: &[u8], on_text: &mut dyn FnMut(&str)) -> Result<()> {
        self.buffer.extend_from_slice(bytes);
        while let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            if data == "[DONE]" {
                self.done = true;
                break;
            }
            let event: Value =
                serde_json::from_str(data).context("malformed model stream event")?;
            if let Some(text) = self.event(&event)? {
                on_text(&text);
            }
        }
        Ok(())
    }

    fn event(&mut self, event: &Value) -> Result<Option<String>> {
        match event["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if delta.is_empty() {
                    return Ok(None);
                }
                self.text.push_str(delta);
                return Ok(Some(delta.to_owned()));
            }
            "response.output_item.done" => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("function_call") => self.calls.push(ToolCall {
                        id: item["call_id"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("call_{}", self.calls.len())),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: item["name"].as_str().unwrap_or("").to_owned(),
                            arguments: item["arguments"]
                                .as_str()
                                .filter(|a| !a.trim().is_empty())
                                .unwrap_or("{}")
                                .to_owned(),
                        },
                    }),
                    Some("message") => {
                        self.cite(item);
                        if !self.text.is_empty() {
                            return Ok(None);
                        }
                        // Text normally comes as deltas; take it whole if it did not.
                        let text: String = item["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p["text"].as_str())
                            .collect();
                        if !text.is_empty() {
                            self.text.push_str(&text);
                            return Ok(Some(text));
                        }
                    }
                    _ => {}
                }
            }
            kind @ ("response.completed" | "response.done" | "response.incomplete") => {
                let response = &event["response"];
                let usage = &response["usage"];
                if usage.is_object() {
                    let int = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
                    self.usage = Some(Usage {
                        prompt_tokens: int("/input_tokens"),
                        cached_tokens: int("/input_tokens_details/cached_tokens"),
                        cache_write_tokens: 0,
                        completion_tokens: int("/output_tokens"),
                        cost: 0.0,
                    });
                }
                self.model = response["model"].as_str().map(str::to_owned);
                self.finish_reason = Some(if kind == "response.incomplete" {
                    response["incomplete_details"]["reason"]
                        .as_str()
                        .unwrap_or("incomplete")
                        .to_owned()
                } else if self.calls.is_empty() {
                    "stop".into()
                } else {
                    "tool_calls".into()
                });
                self.done = true;
            }
            "response.failed" => {
                let message = event["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("the response failed");
                return Err(crate::llm::StreamError(message.to_owned()).into());
            }
            "error" => {
                let message = event["message"]
                    .as_str()
                    .or_else(|| event["error"]["message"].as_str())
                    .unwrap_or("stream error");
                return Err(crate::llm::StreamError(message.to_owned()).into());
            }
            _ => {}
        }
        Ok(None)
    }

    /// Keeps the pages a message cites (`url_citation` annotations).
    fn cite(&mut self, item: &Value) {
        let annotations = item["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part["annotations"].as_array())
            .flatten();
        for annotation in annotations {
            let Some(url) = annotation["url"].as_str().filter(|u| !u.is_empty()) else {
                continue;
            };
            if self.sources.iter().any(|(_, seen)| seen == url) {
                continue;
            }
            let title = annotation["title"].as_str().unwrap_or("").trim();
            self.sources.push((title.to_owned(), url.to_owned()));
        }
    }

    pub fn finish(mut self) -> Result<Completion> {
        if !self.done {
            bail!("the model stream ended before the response was complete");
        }
        // Inline citations are only markers in the text; list the pages.
        if !self.sources.is_empty() {
            self.text.push_str("\n\nSources:");
            for (title, url) in &self.sources {
                self.text.push_str(&format!("\n- {title} {url}"));
            }
        }
        Ok(Completion {
            text: self.text,
            tool_calls: self.calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            model: self.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_have_both_languages() {
        crate::i18n::assert_complete(TEXTS);
    }

    fn jwt(claims: Value) -> String {
        let encode = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.sig",
            encode(br#"{"alg":"none"}"#),
            encode(claims.to_string().as_bytes())
        )
    }

    fn tokens(exp: i64) -> Tokens {
        Tokens {
            id_token: jwt(json!({
                "email": "me@example.org",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acc-1", "chatgpt_plan_type": "plus"
                }
            })),
            access_token: jwt(json!({ "exp": exp })),
            refresh_token: "r1".into(),
            account_id: Some("acc-1".into()),
            last_refresh: 100,
        }
    }

    #[test]
    fn tokens_read_their_claims() {
        let t = tokens(10_000);
        assert_eq!(t.email().as_deref(), Some("me@example.org"));
        assert_eq!(t.plan().as_deref(), Some("plus"));
        assert_eq!(t.who(), " (me@example.org, plus)");
        assert!(!t.stale(10_000 - EARLY - 1));
        assert!(t.stale(10_000 - EARLY));
        let refreshed = Tokens::from_response(
            &json!({ "access_token": jwt(json!({"exp": 1})), "id_token": t.id_token }),
            Some(&t),
        )
        .unwrap();
        assert_eq!(refreshed.refresh_token, "r1");
        assert_eq!(refreshed.account_id.as_deref(), Some("acc-1"));
        assert!(Tokens::from_response(&json!({}), None).is_err());
    }

    #[test]
    fn pkce_and_authorize_url() {
        let pkce = pkce().unwrap();
        assert_eq!(pkce.verifier.len(), 86);
        let digest =
            aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, pkce.verifier.as_bytes());
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref()),
            pkce.challenge
        );
        let url = reqwest::Url::parse(&authorize_url(ISSUER, &pkce, "st")).unwrap();
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/oauth/authorize");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], REDIRECT);
        assert_eq!(q["code_challenge"], pkce.challenge);
        assert_eq!(q["state"], "st");
        assert!(q["scope"].contains("offline_access"));
    }

    #[test]
    fn pasted_addresses_give_their_code() {
        let url = "http://localhost:1455/auth/callback?code=ac_1&scope=x&state=st";
        assert_eq!(pasted_code(url, "st").as_deref(), Some("ac_1"));
        assert_eq!(
            pasted_code("code=ac_2&state=st", "st").as_deref(),
            Some("ac_2")
        );
        assert_eq!(pasted_code(url, "other"), None);
        assert_eq!(pasted_code("  ac_3 \n", "st").as_deref(), Some("ac_3"));
        assert_eq!(pasted_code("not a code", "st"), None);
        assert_eq!(pasted_code("", "st"), None);
    }

    #[test]
    fn codex_auth_files_import() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let t = tokens(5);
        std::fs::write(
            &file,
            json!({ "OPENAI_API_KEY": null, "tokens": {
                "id_token": t.id_token, "access_token": t.access_token,
                "refresh_token": "r9", "account_id": "acc-9" },
                "last_refresh": "2026-01-01T00:00:00Z" })
            .to_string(),
        )
        .unwrap();
        let imported = import(&file).unwrap();
        assert_eq!(imported.refresh_token, "r9");
        assert_eq!(imported.account_id.as_deref(), Some("acc-9"));
        assert_eq!(imported.last_refresh, 0);
        std::fs::write(&file, r#"{"OPENAI_API_KEY": "sk-x"}"#).unwrap();
        assert!(import(&file).is_err());
    }

    #[test]
    fn config_switches_to_chatgpt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[model]\nmodel = \"anthropic/claude-x\"\n").unwrap();
        assert_eq!(use_in_config(&path).unwrap(), DEFAULT_MODEL);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine"), "{text}");
        let config: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(config.model.provider, crate::config::Provider::Chatgpt);
        std::fs::write(&path, "[model]\nmodel = \"gpt-5-codex\"\n").unwrap();
        assert_eq!(use_in_config(&path).unwrap(), "gpt-5-codex");
    }

    #[test]
    fn chats_become_responses_input() {
        let mut user = ChatMessage::user("看图");
        user.images = vec!["data:image/png;base64,AA==".into()];
        let call = ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "shell".into(),
                arguments: "{}".into(),
            },
        };
        let messages = vec![
            ChatMessage::system("you are claw"),
            ChatMessage::system("summary so far"),
            user,
            ChatMessage::assistant_tool_calls(Some("let me look".into()), vec![call]),
            ChatMessage::tool_result("c1", "ok"),
        ];
        let tools = [ToolSpec::function(
            "shell",
            "run",
            json!({"type": "object"}),
        )];
        let body = request_body("gpt-5", &messages, &tools, Some("high"));
        assert_eq!(body["instructions"], "you are claw");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["content"][1]["type"], "input_image");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(
            input[4],
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"})
        );
        assert_eq!(body["tools"][0]["name"], "shell");
        assert!(
            request_body("gpt-5", &messages, &[], None)
                .get("reasoning")
                .is_none()
        );
    }

    #[test]
    fn response_events_parse() {
        let stream = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"你\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"好\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"你好\"}]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c7\",\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":900,\"input_tokens_details\":{\"cached_tokens\":800},\"output_tokens\":12}}}\n\n",
        );
        let mut parser = ResponsesParser::default();
        let mut shown = String::new();
        for chunk in stream.as_bytes().chunks(5) {
            parser.push(chunk, &mut |t| shown.push_str(t)).unwrap();
        }
        assert_eq!(shown, "你好");
        let done = parser.finish().unwrap();
        assert_eq!(done.text, "你好");
        assert_eq!(done.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(done.tool_calls[0].id, "c7");
        assert_eq!(
            done.tool_calls[0].function.arguments,
            "{\"command\":\"ls\"}"
        );
        assert_eq!(done.model.as_deref(), Some("gpt-5"));
        let usage = done.usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.cached_tokens,
                usage.completion_tokens
            ),
            (900, 800, 12)
        );

        let mut failed = ResponsesParser::default();
        let err = failed
            .push(
                b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"overloaded\"}}}\n",
                &mut |_| {},
            )
            .unwrap_err();
        assert!(err.downcast_ref::<crate::llm::StreamError>().is_some());
        assert!(ResponsesParser::default().finish().is_err());
    }

    #[test]
    fn usage_limits_read_well() {
        let body = r#"{"error":{"type":"usage_limit_reached","message":"You've hit your limit","resets_in_seconds":1810}}"#;
        let (message, limit) = error_message(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert!(limit);
        assert!(
            message.contains("31") && message.contains("You've hit your limit"),
            "{message}"
        );
        let (message, limit) = error_message(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"detail":"Unsupported model"}"#,
        );
        assert!(!limit && message.contains("Unsupported model"), "{message}");
    }

    #[tokio::test]
    async fn access_refreshes_stale_tokens_once_and_saves_them() {
        use axum::routing::post;
        let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = refreshes.clone();
        let fresh_access = jwt(json!({ "exp": crate::store::now() + 3600 }));
        let access_for_server = fresh_access.clone();
        let app = axum::Router::new().route(
            "/oauth/token",
            post(move |axum::Json(body): axum::Json<Value>| {
                let counter = counter.clone();
                let access = access_for_server.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(body["grant_type"], "refresh_token");
                    if body["refresh_token"] != "r1" {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error": {"code": "refresh_token_reused"}})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({"access_token": access, "refresh_token": "r2"})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUTH_FILE);
        write(&path, &tokens(1)).unwrap();
        let auth = Auth::shared(&path, &issuer);
        let (access, account) = auth.access(false).await.unwrap();
        assert_eq!(access, fresh_access);
        assert_eq!(account.as_deref(), Some("acc-1"));
        // Fresh now: no second refresh, and the rotated token is saved.
        auth.access(false).await.unwrap();
        assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(read(&path).unwrap().refresh_token, "r2");
        // A refused refresh token reads as a login to redo.
        let err = auth.access(true).await.unwrap_err().to_string();
        assert!(err.contains("refresh_token_reused"), "{err}");
        std::fs::remove_file(&path).unwrap();
        assert!(auth.access(false).await.is_err());
    }

    #[tokio::test]
    async fn the_client_signs_in_retries_a_refused_token_and_reads_the_stream() {
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fresh = jwt(json!({ "exp": crate::store::now() + 3600, "n": 2 }));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",",
            "\"call_id\":\"c1\",\"name\":\"clock\",\"arguments\":\"{}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5\",",
            "\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n",
        );
        let app =
            axum::Router::new()
                .route(
                    "/oauth/token",
                    post({
                        let fresh = fresh.clone();
                        move || {
                            let fresh = fresh.clone();
                            async move {
                                axum::Json(json!({"access_token": fresh, "refresh_token": "r2"}))
                            }
                        }
                    }),
                )
                .route(
                    "/responses",
                    post({
                        let (calls, seen) = (calls.clone(), seen.clone());
                        move |headers: axum::http::HeaderMap,
                              axum::Json(body): axum::Json<Value>| {
                            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                            let header = |name: &str| {
                                headers
                                    .get(name)
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .to_owned()
                            };
                            seen.lock().unwrap().push((
                                header("authorization"),
                                header("chatgpt-account-id"),
                                body,
                            ));
                            async move {
                                // The first token is refused, as if revoked.
                                let status = if first { 401 } else { 200 };
                                axum::response::Response::builder()
                                    .status(status)
                                    .header("content-type", "text/event-stream")
                                    .body(axum::body::Body::from(if first { "{}" } else { stream }))
                                    .unwrap()
                            }
                        }
                    }),
                );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUTH_FILE);
        // Not yet stale, so only the refusal makes it refresh.
        write(&path, &tokens(crate::store::now() + 3600)).unwrap();
        let config = crate::config::ModelConfig {
            base_url: url.clone(),
            model: "gpt-5".into(),
            max_retries: 0,
            ..Default::default()
        };
        let client = crate::llm::Client::new(&config, String::new())
            .unwrap()
            .with_chatgpt(Auth::shared(&path, &url));
        let messages = [ChatMessage::system("be kind"), ChatMessage::user("hello")];
        let mut shown = String::new();
        let completion = client
            .complete(&messages, &[], &mut |t| shown.push_str(t))
            .await
            .unwrap();
        assert_eq!(shown, "Hi");
        assert_eq!(completion.text, "Hi");
        assert_eq!(completion.tool_calls[0].function.name, "clock");
        assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(completion.usage.unwrap().prompt_tokens, 7);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0].0, seen[1].0);
        assert_eq!(seen[1].0, format!("Bearer {fresh}"));
        assert_eq!(seen[1].1, "acc-1");
        assert_eq!(seen[1].2["instructions"], "be kind");
        assert_eq!(seen[1].2["store"], false);
        assert_eq!(read(&path).unwrap().refresh_token, "r2");
    }

    #[test]
    fn cited_pages_are_listed_after_the_answer() {
        let mut parser = ResponsesParser::default();
        let item = json!({"type": "message", "content": [{
            "type": "output_text",
            "text": "Rain tomorrow.",
            "annotations": [
                {"type": "url_citation", "url": "https://a.example", "title": "A"},
                {"type": "url_citation", "url": "https://a.example", "title": "A"},
                {"type": "url_citation", "url": "https://b.example", "title": "B"}
            ]
        }]});
        let events = [
            json!({"type": "response.output_item.done", "item": {"type": "web_search_call"}}),
            json!({"type": "response.output_item.done", "item": item}),
            json!({"type": "response.completed", "response": {}}),
        ];
        let stream: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let mut shown = String::new();
        parser
            .push(stream.as_bytes(), &mut |t| shown.push_str(t))
            .unwrap();
        let completion = parser.finish().unwrap();
        assert_eq!(shown, "Rain tomorrow.");
        assert_eq!(
            completion.text,
            "Rain tomorrow.\n\nSources:\n- A https://a.example\n- B https://b.example"
        );
    }

    #[tokio::test]
    async fn web_search_asks_for_the_hosted_tool() {
        use axum::routing::post;
        let seen = Arc::new(Mutex::new(Value::Null));
        let app = axum::Router::new().route(
            "/responses",
            post({
                let seen = seen.clone();
                move |axum::Json(body): axum::Json<Value>| {
                    *seen.lock().unwrap() = body;
                    async move {
                        let events = [
                            json!({"type": "response.output_text.delta", "delta": "found"}),
                            json!({"type": "response.completed", "response": {}}),
                        ];
                        events
                            .iter()
                            .map(|e| format!("data: {e}\n\n"))
                            .collect::<String>()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUTH_FILE);
        write(&path, &tokens(crate::store::now() + 3600)).unwrap();
        let config = crate::config::ModelConfig {
            base_url: url.clone(),
            model: "gpt-5".into(),
            ..Default::default()
        };
        let client = crate::llm::Client::new(&config, String::new())
            .unwrap()
            .with_chatgpt(Auth::shared(&path, &url));
        let found = client.web_search("weather in Beijing").await.unwrap();
        assert_eq!(found.text, "found");
        let body = seen.lock().unwrap().clone();
        assert_eq!(body["tools"], json!([{"type": "web_search"}]));
        assert_eq!(body["input"][0]["content"][0]["text"], "weather in Beijing");
    }
}
