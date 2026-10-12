//! Runtime configuration: one TOML file under the state directory, with
//! environment overrides for secrets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Leaves the name to the identity chosen on first start.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful personal assistant running on OpenClaw.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Language of CLI output and the program's own chat replies; `None`
    /// follows the locale. Root keys must come before the tables in TOML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<crate::i18n::Lang>,
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub gateway: GatewayConfig,
    pub qq: QqConfig,
    pub feishu: FeishuConfig,
    pub zulip: ZulipConfig,
    pub mail: MailConfig,
    pub search: SearchConfig,
    pub browser: BrowserConfig,
    pub fetch: FetchConfig,
    pub mcp: McpConfig,
    pub skills: SkillsConfig,
    pub limits: crate::limits::LimitsConfig,
    pub update: crate::update::UpdateConfig,
    pub guide: GuideConfig,
    pub access: crate::access::AccessConfig,
    pub heartbeat: crate::heartbeat::HeartbeatConfig,
    pub life: crate::life::LifeConfig,
}

/// Guided conversation: messages sent while a turn runs join that turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuideConfig {
    /// Off: a message sent during a turn waits and runs as its own turn.
    pub enabled: bool,
    /// Which model picks the moment; `off` inserts at the very next step.
    pub provider: ReviewProvider,
    /// Default: `typesafe/jev-1.13` (openrouter), model.model (openrouter-chat),
    /// `jev-latest` (typesafe), or `kev-latest` (typesafe with base_url).
    pub model: Option<String>,
    /// TypeSafe key; prefer `TYPESAFE_API_KEY`. Not sent to a local base_url.
    pub api_key: Option<String>,
    /// Loopback origin of a local Kev System One server.
    pub base_url: Option<String>,
    /// Probability that now is a good moment at or above which the messages go in.
    pub insert_at: f64,
    /// Steps a message may be held back before it goes in anyway.
    pub max_wait_steps: usize,
    pub timeout_secs: u64,
    /// When messages go in, the agent replies to them at once (written by
    /// agent.summary_model, else model.model): what it understood, what it will do.
    pub ack: bool,
}

impl Default for GuideConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: ReviewProvider::Openrouter,
            model: None,
            api_key: None,
            base_url: None,
            insert_at: 0.5,
            max_wait_steps: 3,
            timeout_secs: 10,
            ack: true,
        }
    }
}

impl GuideConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var("TYPESAFE_API_KEY")
            .ok()
            .or_else(|| self.api_key.clone())
            .filter(|k| !k.trim().is_empty())
    }
}

/// Where `web_search` looks things up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    /// OpenRouter's `web` plugin with the configured key; billed per search.
    Openrouter,
    /// A SearXNG instance with the JSON format enabled (`searxng_url`).
    Searxng,
    /// The ChatGPT subscription's own web search (`openclaw-rs login`).
    Chatgpt,
    /// No `web_search` tool.
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    /// Model that runs OpenRouter searches (default: model.model).
    pub model: Option<String>,
    pub searxng_url: Option<String>,
    pub max_results: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: SearchProvider::Openrouter,
            model: None,
            searxng_url: None,
            max_results: 5,
        }
    }
}

/// `browser`: a Chromium-family browser installed on the host, driven over
/// the DevTools protocol. Nothing is bundled; without one the tool is absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Off: no `browser` tool even when a browser is installed.
    pub enabled: bool,
    /// Browser to start; default: the first of chromium, chromium-browser,
    /// google-chrome, microsoft-edge, brave on PATH (or in /Applications).
    pub executable: Option<PathBuf>,
    /// Use a browser that is already running with remote debugging instead
    /// of starting one, e.g. `http://127.0.0.1:9222`.
    pub cdp_url: Option<String>,
    /// Without a window; `false` needs a display.
    pub headless: bool,
    /// Extra command-line flags, e.g. `--proxy-server=socks5://127.0.0.1:1080`.
    pub args: Vec<String>,
    /// Seconds one action (open, click, ...) may take.
    pub timeout_secs: u64,
    /// The browser is closed after this many seconds without use, freeing its memory.
    pub idle_secs: u64,
    /// Characters of page text returned per read.
    pub max_chars: usize,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            cdp_url: None,
            headless: true,
            args: Vec::new(),
            timeout_secs: 30,
            idle_secs: 300,
            max_chars: 8000,
        }
    }
}

/// `web_fetch`: downloads a page and reads its text, without a browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FetchConfig {
    /// Off: no `web_fetch` tool.
    pub enabled: bool,
    /// Let owners fetch pages on this host and the local network (the
    /// router, a NAS); never guests.
    pub private_network: bool,
    /// Seconds one download may take.
    pub timeout_secs: u64,
    /// Bytes downloaded at most; a longer page is cut.
    pub max_bytes: usize,
    /// Characters of page text returned per call.
    pub max_chars: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            private_network: false,
            timeout_secs: 20,
            max_bytes: 2_000_000,
            max_chars: 8000,
        }
    }
}

/// Skills in `<workspace>/skills/<name>/SKILL.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    pub enabled: bool,
    /// Skills left out, by name.
    pub disabled: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: Vec::new(),
        }
    }
}

/// MCP servers whose tools the agent may use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// By name; the name prefixes the server's tools (`<name>__<tool>`).
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One MCP server: a local program (`command`) or a remote endpoint (`url`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub enabled: bool,
    /// Program speaking MCP on stdin/stdout, e.g. `npx` or `uvx`.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Extra environment; `${NAME}` reads the Gateway's own.
    pub env: BTreeMap<String, String>,
    /// Working directory; default: where openclaw-rs was started.
    pub cwd: Option<PathBuf>,
    /// Streamable HTTP endpoint, instead of `command`.
    pub url: Option<String>,
    /// HTTP headers, e.g. `Authorization = "Bearer ${TOKEN}"`.
    pub headers: BTreeMap<String, String>,
    /// Offer only these tools (by the server's names); empty: all.
    pub tools: Vec<String>,
    /// `ask` has every call approved first; `deny` turns the server off.
    pub permission: Permission,
    /// Seconds one tool call may take.
    pub timeout_secs: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            tools: Vec::new(),
            permission: Permission::Allow,
            timeout_secs: 60,
        }
    }
}

/// Transport security for a mail connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailSecurity {
    /// Implicit TLS (IMAP 993, SMTP 465).
    Tls,
    /// SMTP STARTTLS (587); IMAP always uses implicit TLS.
    Starttls,
    /// Plain text; only accepted for loopback hosts such as local mail bridges.
    None,
}

/// Whose mail the bot reads, and to whom it may send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailMode {
    /// Reads and answers only mail from `allow`.
    #[default]
    Allow,
    /// Reads mail from anyone; sends only to `allow`.
    Receive,
    /// Reads mail from anyone and answers anyone.
    Open,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    pub enabled: bool,
    pub mode: MailMode,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: MailSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: MailSecurity,
    pub username: String,
    /// Prefer `MAIL_PASSWORD`. Many providers (QQ Mail, 163) need an app authorization code.
    pub password: Option<String>,
    /// Sender address for replies (default: username).
    pub from: Option<String>,
    /// Addresses (or `@domain`) the bot reads (mode `allow`, required there)
    /// or sends to (mode `receive`).
    pub allow: Vec<String>,
    pub mailbox: String,
    pub poll_secs: u64,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: MailMode::Allow,
            imap_host: String::new(),
            imap_port: 993,
            imap_security: MailSecurity::Tls,
            smtp_host: String::new(),
            smtp_port: 465,
            smtp_security: MailSecurity::Tls,
            username: String::new(),
            password: None,
            from: None,
            allow: Vec::new(),
            mailbox: "INBOX".into(),
            poll_secs: 60,
        }
    }
}

impl MailConfig {
    pub fn password(&self) -> Option<String> {
        std::env::var("MAIL_PASSWORD")
            .ok()
            .or_else(|| self.password.clone())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QqConfig {
    pub enabled: bool,
    pub app_id: String,
    /// Prefer `QQ_APP_SECRET`; a secret here is stored in plain text.
    pub app_secret: Option<String>,
    /// User or group openids that may talk to the bot; empty allows everyone.
    pub allow: Vec<String>,
    pub api_base: String,
    pub token_url: String,
    /// Sends a short chatty reply of a few paragraphs as that many messages,
    /// a moment apart, the way people text.
    pub split_replies: bool,
}

impl Default for QqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: None,
            allow: Vec::new(),
            api_base: "https://api.bot.qq.com".into(),
            token_url: "https://api.bot.qq.com/app/getAppAccessToken".into(),
            split_replies: true,
        }
    }
}

impl QqConfig {
    pub fn app_secret(&self) -> Option<String> {
        std::env::var("QQ_APP_SECRET")
            .ok()
            .or_else(|| self.app_secret.clone())
            .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeishuConfig {
    pub enabled: bool,
    pub app_id: String,
    /// Prefer `FEISHU_APP_SECRET`; a secret here is stored in plain text.
    pub app_secret: Option<String>,
    /// Sender open_ids (`ou_…`) or group chat_ids (`oc_…`) that may talk
    /// to the bot; empty allows everyone.
    pub allow: Vec<String>,
    /// `https://open.feishu.cn`, or `https://open.larksuite.com` for Lark.
    pub api_base: String,
    /// As `qq.split_replies`.
    pub split_replies: bool,
}

impl Default for FeishuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: None,
            allow: Vec::new(),
            api_base: "https://open.feishu.cn".into(),
            split_replies: true,
        }
    }
}

impl FeishuConfig {
    pub fn app_secret(&self) -> Option<String> {
        std::env::var("FEISHU_APP_SECRET")
            .ok()
            .or_else(|| self.app_secret.clone())
            .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ZulipConfig {
    pub enabled: bool,
    /// The organization's address, e.g. `https://example.zulipchat.com`.
    pub site: String,
    /// The bot's email, shown on its settings page.
    pub email: String,
    /// Prefer `ZULIP_API_KEY`; a key here is stored in plain text.
    pub api_key: Option<String>,
    /// Sender emails or channel (stream) names that may talk to the bot;
    /// empty allows everyone in the organization.
    pub allow: Vec<String>,
    /// As `qq.split_replies`.
    pub split_replies: bool,
}

impl Default for ZulipConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            site: String::new(),
            email: String::new(),
            api_key: None,
            allow: Vec::new(),
            split_replies: true,
        }
    }
}

impl ZulipConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var("ZULIP_API_KEY")
            .ok()
            .or_else(|| self.api_key.clone())
            .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    pub bind: String,
    /// Required for non-loopback binds; prefer `OPENCLAW_RS_TOKEN`.
    pub token: Option<String>,
    /// What `serve` offers over HTTP; QQ, email and cron run in every mode.
    pub web: WebMode,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:18789".into(),
            token: None,
            web: WebMode::Ui,
        }
    }
}

/// What the Gateway listens for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum WebMode {
    /// The Web UI and the WebSocket API.
    #[default]
    Ui,
    /// Only the WebSocket API (and /health), for other clients.
    Api,
    /// No HTTP listener at all: QQ, email and scheduled jobs only.
    Off,
}

impl GatewayConfig {
    pub fn token(&self) -> Option<String> {
        std::env::var("OPENCLAW_RS_TOKEN")
            .ok()
            .or_else(|| self.token.clone())
            .filter(|t| !t.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Who serves the model: OpenRouter (or any compatible server at
    /// `base_url`), or a ChatGPT subscription signed in with `login`.
    pub provider: Provider,
    pub base_url: String,
    /// Required: there is no default model, so the operator always chooses.
    pub model: String,
    /// Prefer `OPENROUTER_API_KEY`; a key here is stored in plain text.
    pub api_key: Option<String>,
    pub request_timeout_secs: u64,
    /// Models OpenRouter tries, in order, when `model` fails (OpenRouter's
    /// `models` parameter; other servers ignore it).
    pub fallbacks: Vec<String>,
    /// Retries of a failed request (connection errors, HTTP 408/429/5xx, a
    /// stream that fails before any text), with exponential backoff.
    pub max_retries: u32,
    pub prompt_cache: PromptCache,
    /// How hard reasoning models think: `auto` (unset; the lowest level the
    /// model supports from `low` up), `off` (send nothing, the model
    /// decides), or a level (`none`, `minimal`, `low`, `medium`, `high`,
    /// `xhigh`, `max`).
    pub reasoning_effort: Option<String>,
}

/// Who serves the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// OpenRouter, or another OpenAI-compatible Chat Completions server.
    #[default]
    Openrouter,
    /// A ChatGPT plan, through the Codex backend's Responses API.
    Chatgpt,
}

/// Whether to mark `cache_control` breakpoints for prompt caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCache {
    /// For `anthropic/` and `google/` models, which cache only at breakpoints;
    /// others (OpenAI, DeepSeek, …) cache on their own.
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Upper bound on model calls per user turn, so a tool loop cannot run forever.
    pub max_steps: usize,
    /// Token budget for each model call: system prompt, tool specs and
    /// history. `auto` (the default) takes it from the model's context
    /// window, leaving room for the answer; or a count such as "128k" or
    /// "1m". Older history stays in SQLite.
    pub context_tokens: ContextBudget,
    /// Model that writes the context summary, e.g. a cheaper one; default:
    /// model.model.
    pub summary_model: Option<String>,
    /// Model for the calls that carry images (files people send, browser
    /// screenshots); default: model.model. Set it when the main model has
    /// no image input.
    pub vision_model: Option<String>,
    /// Model that transcribes voice messages; default: model.model. Set it
    /// when the main model has no audio input.
    pub audio_model: Option<String>,
    /// Saved memories looked up from each message and shown with it; 0 turns
    /// recall off.
    pub recall_limit: usize,
    /// Token cap on the recalled memories of one turn.
    pub recall_tokens: usize,
    /// Tells the model the local time with each message, and how long ago
    /// the person last wrote when it has been a while.
    pub time_sense: bool,
    /// Where files sent with messages are saved (under `inbox/`); set at
    /// startup from `tools.workspace`, never read from the file.
    #[serde(skip)]
    pub workspace: PathBuf,
    /// `[limits]`, set at startup; never read from `[agent]`.
    #[serde(skip)]
    pub limits: crate::limits::LimitsConfig,
    /// What `auto` came to at startup, from the model's window; never read
    /// from the file.
    #[serde(skip)]
    pub auto_context: Option<usize>,
    /// From `[life]` at startup: carried into the owner's turns when on.
    #[serde(skip)]
    pub life: Option<crate::life::Inner>,
}

impl AgentConfig {
    /// The budget in tokens: the one set, or what `auto` came to (64k when
    /// the model's window could not be found).
    pub fn context_budget(&self) -> usize {
        match self.context_tokens {
            ContextBudget::Tokens(n) => n,
            ContextBudget::Auto => self.auto_context.unwrap_or(FALLBACK_CONTEXT),
        }
    }
}

/// The budget when `auto` finds no window for the model.
pub const FALLBACK_CONTEXT: usize = 64_000;

/// `agent.context_tokens`: `auto`, or a token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextBudget {
    #[default]
    Auto,
    Tokens(usize),
}

impl ContextBudget {
    pub fn parse(text: &str) -> Option<Self> {
        if text.trim().eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        parse_tokens(text).map(Self::Tokens)
    }
}

impl Serialize for ContextBudget {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::Tokens(n) => s.serialize_str(&exact_tokens(*n)),
        }
    }
}

impl<'de> Deserialize<'de> for ContextBudget {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Number(n) if n > 0 => Ok(Self::Tokens(n as usize)),
            Raw::Number(_) => Err(serde::de::Error::custom(
                "the context budget must be above 0",
            )),
            Raw::Text(text) => Self::parse(&text).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "{text:?} is not a context budget: use auto, or a count such as 128k or 1m"
                ))
            }),
        }
    }
}

/// "32000", "32k", "1.5m", "200_000" → tokens; k is 1000 and m 1,000,000.
pub fn parse_tokens(text: &str) -> Option<usize> {
    let text = text.trim().to_lowercase().replace(['_', ','], "");
    let (number, unit) = match text.strip_suffix('k') {
        Some(n) => (n, 1_000.0),
        None => match text.strip_suffix('m') {
            Some(n) => (n, 1_000_000.0),
            None => (text.as_str(), 1.0),
        },
    };
    let n: f64 = number.trim().parse().ok()?;
    let tokens = (n * unit).round();
    (1.0..1e12).contains(&tokens).then_some(tokens as usize)
}

/// Tokens written back exactly, with k or m: 32000 → "32k", 1500000 →
/// "1.5m", 131072 → "131.072k".
pub fn exact_tokens(n: usize) -> String {
    let trim = |s: String| s.trim_end_matches('0').trim_end_matches('.').to_owned();
    if n >= 1_000_000 && n.is_multiple_of(1_000) {
        format!("{}m", trim(format!("{:.3}", n as f64 / 1e6)))
    } else if n >= 1_000 {
        format!("{}k", trim(format!("{:.3}", n as f64 / 1e3)))
    } else {
        n.to_string()
    }
}

/// Tokens for people, rounded: 950, 1.2k, 64k, 131k, 1.05m.
pub fn short_tokens(n: usize) -> String {
    let trim = |s: String| s.trim_end_matches('0').trim_end_matches('.').to_owned();
    if n >= 1_000_000 {
        format!("{}m", trim(format!("{:.2}", n as f64 / 1e6)))
    } else if n >= 10_000 {
        format!("{}k", (n as f64 / 1e3).round())
    } else if n >= 1_000 {
        format!("{}k", trim(format!("{:.1}", n as f64 / 1e3)))
    } else {
        n.to_string()
    }
}

/// What a tool category may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Working directory for relative paths and shell commands (default: <state dir>/workspace).
    pub workspace: Option<PathBuf>,
    pub shell: Permission,
    /// Covers write_file and edit_file; reads are always allowed.
    pub write: Permission,
    /// `deny` removes `identity_set`; otherwise every draft, including the
    /// first, is saved only after a person approves it.
    pub identity: Permission,
    /// `send_mail`; `deny` removes it. Recipients also follow `mail.mode`.
    pub mail: Permission,
    pub shell_timeout_secs: u64,
    /// Per-stream cap on tool output returned to the model, and on shell output
    /// buffered while a command runs.
    pub max_output_bytes: usize,
    /// Model review of `shell` commands before anyone is asked.
    pub review: ReviewConfig,
}

/// Which model judges whether a shell command is dangerous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewProvider {
    /// Every `ask` command goes to a person.
    Off,
    /// A decision model on OpenRouter's decisions API (`typesafe/jev-1.13`), with the OpenRouter key.
    Openrouter,
    /// Any OpenRouter chat model, asked for a JSON rating, with the OpenRouter key.
    #[serde(rename = "openrouter-chat")]
    OpenrouterChat,
    /// TypeSafe AI's own System One API (`jev-latest` hosted, or a local Kev server).
    Typesafe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    pub provider: ReviewProvider,
    /// Default: `typesafe/jev-1.13` (openrouter), model.model (openrouter-chat),
    /// `jev-latest` (typesafe), or `kev-latest` (typesafe with base_url).
    pub model: Option<String>,
    /// TypeSafe key; prefer `TYPESAFE_API_KEY`. Not sent to a local base_url.
    pub api_key: Option<String>,
    /// Loopback origin of a local Kev System One server, e.g. http://127.0.0.1:8009.
    pub base_url: Option<String>,
    /// Danger probability below which a command runs without asking.
    pub allow_below: f64,
    /// Danger probability at or above which a command is declined without asking.
    pub deny_at: f64,
    pub timeout_secs: u64,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            provider: ReviewProvider::Off,
            model: None,
            api_key: None,
            base_url: None,
            allow_below: 0.2,
            deny_at: 0.9,
            timeout_secs: 20,
        }
    }
}

impl ReviewConfig {
    pub fn api_key(&self) -> Option<String> {
        std::env::var("TYPESAFE_API_KEY")
            .ok()
            .or_else(|| self.api_key.clone())
            .filter(|k| !k.trim().is_empty())
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace: None,
            shell: Permission::Ask,
            write: Permission::Ask,
            identity: Permission::Ask,
            mail: Permission::Ask,
            shell_timeout_secs: 120,
            max_output_bytes: 16 * 1024,
            review: ReviewConfig::default(),
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Openrouter,
            base_url: DEFAULT_BASE_URL.into(),
            model: String::new(),
            api_key: None,
            request_timeout_secs: 300,
            fallbacks: Vec::new(),
            max_retries: 3,
            prompt_cache: PromptCache::Auto,
            reasoning_effort: None,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_steps: 25,
            context_tokens: ContextBudget::Auto,
            summary_model: None,
            vision_model: None,
            audio_model: None,
            recall_limit: 5,
            recall_tokens: 800,
            time_sense: true,
            workspace: PathBuf::new(),
            limits: crate::limits::LimitsConfig::default(),
            auto_context: None,
            life: None,
        }
    }
}

impl Config {
    /// Whether any channel that strangers can write to is on.
    pub fn any_channel(&self) -> bool {
        self.qq.enabled || self.feishu.enabled || self.zulip.enabled || self.mail.enabled
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// The main model; there is none until the operator sets one.
    pub fn model_id(&self) -> Result<&str> {
        let model = self.model.model.trim();
        if model.is_empty() {
            anyhow::bail!(crate::cli_text::NO_MODEL.now());
        }
        Ok(model)
    }

    /// Whether the model is a ChatGPT subscription.
    pub fn chatgpt(&self) -> bool {
        self.model.provider == Provider::Chatgpt
    }

    /// The key the model is called with: the OpenRouter key, or none
    /// (empty) for a ChatGPT subscription, which signs in instead.
    pub fn model_key(&self) -> Result<String> {
        if self.chatgpt() {
            return Ok(self.api_key().unwrap_or_default());
        }
        self.api_key()
    }

    /// Where the helpers that only speak OpenRouter (search, voice, review
    /// and guide deciders) go: the model settings as they are, or for a
    /// ChatGPT subscription the same model at OpenRouter as `openai/<id>`.
    pub fn openrouter_model(&self) -> ModelConfig {
        if !self.chatgpt() {
            return self.model.clone();
        }
        let model = self.model.model.trim();
        ModelConfig {
            provider: Provider::Openrouter,
            base_url: DEFAULT_BASE_URL.into(),
            model: if model.contains('/') {
                model.to_owned()
            } else {
                format!("openai/{model}")
            },
            ..self.model.clone()
        }
    }

    /// The settings for a side model (summaries, images): on a ChatGPT
    /// subscription an id with a `/` is an OpenRouter model.
    pub fn side_model(&self, model: &str) -> ModelConfig {
        let base = if self.chatgpt() && model.contains('/') {
            self.openrouter_model()
        } else {
            self.model.clone()
        };
        ModelConfig {
            model: model.to_owned(),
            ..base
        }
    }

    /// Environment wins so secrets never need to live in the config file.
    pub fn api_key(&self) -> Result<String> {
        std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .or_else(|| {
                self.model
                    .api_key
                    .clone()
                    .filter(|key| !key.trim().is_empty())
            })
            .context(
                "no OpenRouter API key: run `openclaw-rs init`, or set OPENROUTER_API_KEY or model.api_key in config.toml",
            )
    }
}

/// State root: `OPENCLAW_RS_HOME`, else `~/.openclaw-rs`.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENCLAW_RS_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".openclaw-rs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_budgets_read_and_write_with_k_and_m() {
        let read = |t: &str| toml::from_str::<Config>(&format!("[agent]\ncontext_tokens = {t}\n"));
        assert_eq!(
            read("32000").unwrap().agent.context_tokens,
            ContextBudget::Tokens(32_000)
        );
        assert_eq!(
            read("\"128K\"").unwrap().agent.context_tokens,
            ContextBudget::Tokens(128_000)
        );
        assert_eq!(
            read("\"1.5m\"").unwrap().agent.context_tokens,
            ContextBudget::Tokens(1_500_000)
        );
        assert_eq!(
            read("\"auto\"").unwrap().agent.context_tokens,
            ContextBudget::Auto
        );
        assert!(read("\"lots\"").is_err() && read("0").is_err());
        assert_eq!(Config::default().agent.context_tokens, ContextBudget::Auto);
        for n in [
            999, 32_000, 64_000, 131_072, 1_000_000, 1_048_576, 2_500_000,
        ] {
            assert_eq!(parse_tokens(&exact_tokens(n)), Some(n), "{n}");
        }
        assert_eq!(exact_tokens(32_000), "32k");
        assert_eq!(exact_tokens(1_500_000), "1.5m");
        assert_eq!(short_tokens(1_048_576), "1.05m");
        assert_eq!(short_tokens(131_072), "131k");
        assert_eq!(short_tokens(1_234), "1.2k");
        let agent = AgentConfig {
            auto_context: Some(180_000),
            ..AgentConfig::default()
        };
        assert_eq!(agent.context_budget(), 180_000);
        assert_eq!(AgentConfig::default().context_budget(), FALLBACK_CONTEXT);
    }

    #[test]
    fn partial_file_keeps_defaults() {
        let config: Config = toml::from_str("[model]\nmodel = \"x/y\"\n").unwrap();
        assert_eq!(config.model.model, "x/y");
        assert_eq!(config.model.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.agent.max_steps, 25);
    }

    #[test]
    fn the_model_must_be_chosen() {
        let config = Config::default();
        assert!(config.model.model.is_empty());
        assert!(
            config
                .model_id()
                .unwrap_err()
                .to_string()
                .contains("model.model")
        );
        let config: Config = toml::from_str("[model]\nmodel = \" x/y \"\n").unwrap();
        assert_eq!(config.model_id().unwrap(), "x/y");
    }
}
