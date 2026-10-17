//! `runo init`: guided first-time setup. It asks only what a first
//! run needs (language, OpenRouter key, main model, an image model when the
//! main one cannot see images), checks the key and offers models from
//! OpenRouter's catalog, then saves config.toml. Everything else keeps its
//! default and stays in `runo config`.
//!
//! `chat`, `ask` and `serve` start it on their own when run in a terminal
//! with no model or key, so nobody has to edit a file before the first reply.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value as Json;
use toml_edit::{DocumentMut, Value};

use crate::config::Config;
use crate::i18n::{Lang, Tr, tr};
use crate::setup::{self, Picked, Term};

/// Models shown per search.
const SHOWN: usize = 12;

/// A model from the catalog, with what the agent needs to know about it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context: u64,
    /// US dollars per million tokens, in and out; `None` when not listed.
    pub price: Option<(f64, f64)>,
    pub images: bool,
    /// Takes audio input, so it can transcribe voice messages.
    pub audio: bool,
    /// Supports tool calls, without which the agent cannot use its tools.
    pub tools: bool,
    /// Longest answer the provider allows, when listed.
    pub max_output: Option<u64>,
    /// The thinking levels it can be asked for (`reasoning.supported_efforts`
    /// in the catalog); empty when it lists none.
    pub efforts: Vec<String>,
}

/// What checking an API key found.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyCheck {
    /// Accepted; the detail says how much credit is used or left, when known.
    Valid(Option<String>),
    /// The server refused it; the text says why.
    Invalid(String),
    /// It could not be checked (offline, another server).
    Unknown(String),
}

/// Where models and key checks come from; OpenRouter in real use.
#[async_trait]
pub trait Catalog: Send + Sync {
    async fn check_key(&self, key: &str) -> KeyCheck;
    async fn models(&self) -> Result<Vec<ModelInfo>>;
}

/// OpenRouter's (or a compatible server's) `/key` and `/models`.
pub struct OpenRouter {
    base_url: String,
}

impl OpenRouter {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// Built per request, so a proxy set during setup applies.
    fn http(&self) -> reqwest::Client {
        crate::net::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Catalog for OpenRouter {
    async fn check_key(&self, key: &str) -> KeyCheck {
        let response = self
            .http()
            .get(format!("{}/key", self.base_url))
            .bearer_auth(key)
            .send()
            .await;
        match response {
            Ok(r) if r.status().is_success() => {
                let body: Json = r.json().await.unwrap_or_default();
                KeyCheck::Valid(credit(&body))
            }
            Ok(r) if matches!(r.status().as_u16(), 401 | 403) => {
                let status = r.status();
                let body: Json = r.json().await.unwrap_or_default();
                let message = body
                    .pointer("/error/message")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                KeyCheck::Invalid(format!("HTTP {status} {message}").trim().to_owned())
            }
            Ok(r) => KeyCheck::Unknown(format!("HTTP {}", r.status())),
            Err(err) => KeyCheck::Unknown(err.without_url().to_string()),
        }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        let body: Json = self
            .http()
            .get(format!("{}/models", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_models(&body))
    }
}

/// Credit left on a key (or spent, for keys without a limit), from `/key`.
fn credit(body: &Json) -> Option<String> {
    let number = |k: &str| body.pointer(&format!("/data/{k}")).and_then(Json::as_f64);
    match (number("limit_remaining"), number("usage")) {
        (Some(left), _) => Some(format!("${left:.2} left")),
        (None, Some(used)) => Some(format!("${used:.2} used, no limit")),
        _ => None,
    }
}

fn parse_models(body: &Json) -> Vec<ModelInfo> {
    let price = |m: &Json, k: &str| {
        m.pointer(&format!("/pricing/{k}"))
            .and_then(Json::as_str)
            .and_then(|p| p.parse::<f64>().ok())
            .map(|p| p * 1_000_000.0)
    };
    body.get("data")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let has = |path: &str, what: &str| {
                m.pointer(path)
                    .and_then(Json::as_array)
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(what)))
            };
            Some(ModelInfo {
                name: m
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                context: m.get("context_length").and_then(Json::as_u64).unwrap_or(0),
                price: price(m, "prompt").zip(price(m, "completion")),
                images: has("/architecture/input_modalities", "image"),
                audio: has("/architecture/input_modalities", "audio"),
                tools: has("/supported_parameters", "tools"),
                efforts: m
                    .pointer("/reasoning/supported_efforts")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                    .map(str::to_owned)
                    .collect(),
                max_output: m
                    .pointer("/top_provider/max_completion_tokens")
                    .and_then(Json::as_u64),
                id,
            })
        })
        .collect()
}

const WELCOME: Tr = tr(
    "Welcome to Runo. A few questions and you are ready; everything else can be changed later with `runo config`.",
    "欢迎使用 Runo。回答几个问题就能开始用；其他设置以后可以用 `runo config` 修改。",
);
const STEP: Tr = tr("\n[{}/8] {}", "\n[{}/8] {}");
const STEP_LANGUAGE: Tr = tr("Language", "语言");
const STEP_KEY: Tr = tr("Model provider", "模型提供方");
const STEP_PERMISSIONS: Tr = tr("Permissions", "权限");
const STEP_TOOLS: Tr = tr("Tools and the web", "工具与网络");
const PROXY_ASK: Tr = tr(
    "Reach the model providers through a network proxy? [y/N]: ",
    "需要通过网络代理访问模型提供方吗？[y/N]：",
);
const PROXY_ASK_SET: Tr = tr(
    "Reach the model providers through a network proxy? [Y/n]: ",
    "需要通过网络代理访问模型提供方吗？[Y/n]：",
);
const PROXY_ADDRESS: Tr = tr(
    "Proxy address (http://host:port, e.g. http://127.0.0.1:7890): ",
    "代理地址（http://主机:端口，例如 http://127.0.0.1:7890）：",
);
const PROXY_ADDRESS_KEEP: Tr = tr(
    "Proxy address [{}] (Enter keeps it): ",
    "代理地址 [{}]（直接回车保留）：",
);
const PROXY_BAD: Tr = tr(
    "✗ {} Only http:// proxies work (Clash, v2rayN and the like have an HTTP port).",
    "✗ {} 只支持 http:// 代理（Clash、v2rayN 等都有 HTTP 端口）。",
);
const PROXY_SET: Tr = tr(
    "✓ Using the proxy {}; local addresses go direct.",
    "✓ 使用代理 {}；本机和局域网地址直连。",
);
const PROVIDER_TITLE: Tr = tr("Which models?", "用哪里的模型？");
const PROVIDER_CHATGPT: Tr = tr("My ChatGPT subscription", "我的 ChatGPT 订阅");
const PROVIDER_OPENROUTER: Tr = tr("OpenRouter (API key)", "OpenRouter（API key）");
const SIGN_IN_HOW: Tr = tr("Sign in how?", "怎么登录？");
const SIGN_IN_BROWSER: Tr = tr("In a browser", "在浏览器里登录");
const SIGN_IN_DEVICE: Tr = tr(
    "With a code (on a phone or another device)",
    "用验证码（在手机或其他设备上输入）",
);
const SEARCH_TITLE: Tr = tr("Web search", "网页搜索");
const SEARCH_AUTO: Tr = tr("With the main model's provider ({})", "跟随主模型（{}）");
const SEARCH_SEARXNG: Tr = tr("My own SearXNG", "自己的 SearXNG");
const SEARCH_OFF: Tr = tr("Off", "关闭");
const SEARXNG_URL: Tr = tr("SearXNG address [{}]: ", "SearXNG 地址 [{}]：");
const FETCH_ASK: Tr = tr(
    "Let it read web pages without a browser (web_fetch)? [Y/n]: ",
    "允许不开浏览器直接读取网页（web_fetch）？[Y/n]：",
);
const SHELL_TITLE: Tr = tr("Shell commands", "Shell 命令");
const WRITE_TITLE: Tr = tr("Writing and editing files", "写入和修改文件");
const PERMIT_ASK: Tr = tr("Ask me each time", "每次问我");
const PERMIT_ALLOW: Tr = tr("Allow", "直接允许");
const PERMIT_DENY: Tr = tr("Never", "不允许");
const KEEP_SIGN_IN: Tr = tr(
    "Keep the ChatGPT sign-in ({})? [Y/n]: ",
    "保留当前的 ChatGPT 登录（{}）？[Y/n]：",
);
const SIGN_IN_FAILED: Tr = tr("✗ Could not sign in: {}", "✗ 登录失败：{}");
const SIGN_IN_RETRY: Tr = tr("Try again? [Y/n]: ", "再试一次？[Y/n]：");
const SIGNED_IN_AS: Tr = tr("✓ Signed in to ChatGPT: {}", "✓ 已登录 ChatGPT：{}");
const CHATGPT_MODEL: Tr = tr(
    "Model your plan offers in Codex [{}]: ",
    "你的套餐在 Codex 里能用的模型 [{}]：",
);
const OWNERS_HOW: Tr = tr(
    "On QQ, Feishu and Zulip only owners may use every tool (memory, files, shell); everyone else is a guest. Owners are listed in access.owners (`runo config`, Access and spending › Access).",
    "在 QQ、飞书和 Zulip 上，只有所有者能使用全部工具（记忆、文件、shell），其他人都是访客。所有者记在 access.owners 里（`runo config` 的“权限与花费 › 权限”）。",
);
const OWNERS_NOW: Tr = tr("Owners now: {}", "当前所有者：{}");
const FIRST_CHAT_ASK: Tr = tr(
    "Let the first person to write to the bot in a private chat on an app without an owner become its owner? Write to it yourself first, then turn this off in `runo config`. [y/N]: ",
    "让每个还没有所有者的平台上，第一个私聊机器人的人自动成为所有者？请自己先去私聊机器人，之后再到 `runo config` 里关掉。[y/N]：",
);
const FIRST_CHAT_ON: Tr = tr(
    "✓ The first private chat on QQ, Feishu or Zulip claims that app. Until you have written, anyone who reaches the bot first gets every tool.",
    "✓ QQ、飞书或 Zulip 上第一个私聊的人会成为该平台的所有者。在你自己发消息之前，谁先找到机器人谁就拿到所有工具。",
);
const REVOKE_ASK: Tr = tr("Revoke one of them? [y/N]: ", "要撤销其中一个吗？[y/N]：");
const STEP_MODEL: Tr = tr("Main model", "主模型");
const STEP_IMAGES: Tr = tr("Images", "图片");
const STEP_GATEWAY: Tr = tr("Gateway", "Gateway");
const STEP_COMPANION: Tr = tr("Companion", "陪伴");
const COMPANION_HOW: Tr = tr(
    "It can be more than a chatbot: keep a diary and a mood of its own, remember what is going on in your life, say good morning and good night, and message you on its own now and then when it has something worth saying (never 23:00-08:00). That takes a few model calls a day and the Gateway running.",
    "它可以不只是聊天机器人：有自己的日记和心情，记住你生活里正在发生的事，早晚问候，偶尔在有话想说时主动找你（23:00–08:00 不打扰）。这每天需要几次模型调用，并且 Gateway 要在运行。",
);
const COMPANION_ASK: Tr = tr("Turn this on? [Y/n]: ", "要打开吗？[Y/n]：");
const COMPANION_CITY: Tr = tr(
    "Your city, for the weather in the morning (Enter to skip): ",
    "你所在的城市，早安时说说天气（回车跳过）：",
);
const COMPANION_BIRTHDAY: Tr = tr(
    "Your birthday as MM-DD, so it remembers (Enter to skip): ",
    "你的生日，格式 月-日，例如 03-14，它会记住（回车跳过）：",
);
const COMPANION_QUIET: Tr = tr(
    "Quiet hours, when it never messages you first [23:00-08:00]: ",
    "免打扰时段，这段时间它不会主动找你 [23:00-08:00]：",
);
const COMPANION_BAD: Tr = tr(
    "{} is not right; try again or press Enter.",
    "{} 格式不对；请重新输入，或直接回车。",
);
const COMPANION_LATER: Tr = tr(
    "Change it any time with `runo config` or /config in the chat window, section \"Companion\".",
    "以后随时可以用 `runo config` 或聊天窗口里的 /config 修改，在“陪伴”一节。",
);
const MY_BIRTHDAY: Tr = tr("my birthday", "我的生日");
const LANGUAGE_TITLE: Tr = tr("Language / 语言", "语言 / Language");
const PICK_MODEL: Tr = tr("Pick a model", "选择模型");
const SEARCH_AGAIN: Tr = tr("Search again", "重新搜索");
const TYPE_ID: Tr = tr("Type a model id", "输入模型 id");
const GATEWAY_HOW: Tr = tr(
    "The Gateway serves the Web UI at http://{} and connects QQ and email. Run in the background, it keeps going after this terminal closes.",
    "Gateway 在 http://{} 提供 Web UI，并连接 QQ 和邮件。在后台运行时，关掉终端也会继续运行。",
);
const GATEWAY_ASK: Tr = tr(
    "Start the Gateway now, and at boot where this system allows (may ask for your password)? [Y/n]: ",
    "现在启动 Gateway，并尽量设为开机自启（可能要求输入你的密码）？[Y/n]：",
);
const GATEWAY_LATER: Tr = tr(
    "Start it later with `runo gateway start`, or `runo serve` in the foreground.",
    "以后可以用 `runo gateway start` 启动，或用 `runo serve` 在前台运行。",
);
const GATEWAY_FAILED: Tr = tr(
    "! Could not start the Gateway: {}",
    "! 无法启动 Gateway：{}",
);
const PICK_LANGUAGE: Tr = tr("1) 中文  2) English [{}]: ", "1) 中文  2) English [{}]: ");
const KEY_FROM_ENV: Tr = tr(
    "Using the key in $OPENROUTER_API_KEY.",
    "使用环境变量 $OPENROUTER_API_KEY 里的 key。",
);
const KEY_HOW: Tr = tr(
    "The agent's models run on OpenRouter. Create a key at https://openrouter.ai/keys and paste it here (it is not shown while typing).",
    "智能体的模型通过 OpenRouter 调用。请在 https://openrouter.ai/keys 创建 key 并粘贴到这里（输入时不显示）。",
);
const KEY_PROMPT: Tr = tr("API key: ", "API key：");
const KEY_KEEP_PROMPT: Tr = tr(
    "API key (Enter keeps the one in config.toml): ",
    "API key（回车保留 config.toml 里已有的）：",
);
const KEY_CHECKING: Tr = tr("Checking the key...", "正在验证 key……");
const KEY_OK: Tr = tr("✓ The key works.", "✓ key 可用。");
const KEY_BAD: Tr = tr(
    "✗ OpenRouter refused this key ({}). Paste it again.",
    "✗ OpenRouter 拒绝了这个 key（{}），请重新输入。",
);
const KEY_UNCHECKED: Tr = tr(
    "! Could not check the key ({}); it is kept anyway.",
    "! 无法验证 key（{}），先保留它。",
);
const KEY_ENV_BAD: Tr = tr(
    "✗ OpenRouter refused the key in $OPENROUTER_API_KEY ({}). Fix the variable and run `runo init` again.",
    "✗ OpenRouter 拒绝了 $OPENROUTER_API_KEY 里的 key（{}）。请修正环境变量后重新运行 `runo init`。",
);
const KEY_NEEDED: Tr = tr("A key is required.", "必须填写 key。");
const LOADING_MODELS: Tr = tr("Loading the model list...", "正在获取模型列表……");
const NO_CATALOG: Tr = tr(
    "! Could not load the model list ({}); type a model id instead.",
    "! 无法获取模型列表（{}），请直接输入模型 id。",
);
const KEEP_MODEL: Tr = tr("Keep the model {}? [Y/n]: ", "保留模型 {}？[Y/n]：");
const MODEL_HOW: Tr = tr(
    "Search by name or maker (claude, gpt, gemini, qwen, deepseek...); Enter lists the newest. 🖼 = can see images.",
    "按名称或厂商搜索（claude、gpt、gemini、qwen、deepseek……）；直接回车列出最新的。🖼 = 能看图片。",
);
const ONLY_TOOLS: Tr = tr(
    "Only models that can use tools are shown.",
    "只显示能调用工具的模型。",
);
const ONLY_AUDIO: Tr = tr(
    "Only models that take audio are shown.",
    "只显示能输入音频的模型。",
);
const SEARCH_PROMPT: Tr = tr("Search: ", "搜索：");
const NO_MATCH: Tr = tr(
    "No model matches; try another word.",
    "没有匹配的模型，换个词试试。",
);
const PICK_PROMPT: Tr = tr(
    "Number, or a full model id; Enter searches again: ",
    "输入编号或完整模型 id；回车重新搜索：",
);
const ID_PROMPT: Tr = tr(
    "Model id (e.g. anthropic/claude-sonnet-4.5): ",
    "模型 id（例如 anthropic/claude-sonnet-4.5）：",
);
const UNKNOWN_ID: Tr = tr(
    "{} is not in the list. Use it anyway? [y/N]: ",
    "{} 不在列表里。仍然使用它？[y/N]：",
);
const NO_TOOLS: Tr = tr(
    "{} cannot use tools, so the agent could not run commands, read files or browse. Use it anyway? [y/N]: ",
    "{} 不能调用工具，智能体将无法执行命令、读文件或浏览网页。仍然使用它？[y/N]：",
);
const CHOSEN: Tr = tr("✓ Model: {}", "✓ 模型：{}");
const SEES_IMAGES: Tr = tr(
    "✓ {} can see images, so it also handles pictures people send and browser screenshots.",
    "✓ {} 能看图片，用户发来的图片和浏览器截图也交给它。",
);
const BLIND: Tr = tr(
    "{} cannot see images. Pick a model for pictures people send and for browser screenshots? Without one they are only listed by name. [Y/n]: ",
    "{} 不能看图片。要为用户发来的图片和浏览器截图选一个图片模型吗？不选的话图片只按文件名列出。[Y/n]：",
);
const IMAGES_UNKNOWN: Tr = tr(
    "Whether {} can see images is unknown; if not, set agent.vision_model later with `runo config`.",
    "不确定 {} 能否看图片；如果不能，以后可以用 `runo config` 设置 agent.vision_model。",
);
const VISION_CHOSEN: Tr = tr("✓ Image model: {}", "✓ 图片模型：{}");
const CONTEXT_AUTO: Tr = tr(
    "✓ Context: {} has a {} window, so each call may use up to {} (agent.context_tokens = \"auto\").",
    "✓ 上下文：{} 的窗口是 {}，每次调用最多使用 {}（agent.context_tokens = \"auto\"）。",
);
const BROWSER_FOUND: Tr = tr(
    "✓ Browser: {} (used when the agent needs a real web page).",
    "✓ 浏览器：{}（智能体需要打开真实网页时使用）。",
);
const BROWSER_MISSING: Tr = tr(
    "· No browser found, so the agent has no browser tool. Install Chromium to add one (apk add chromium / apt install chromium) and restart.",
    "· 没有找到浏览器，所以智能体没有浏览器工具。安装 Chromium 即可启用（apk add chromium / apt install chromium），然后重启。",
);
const BROWSER_INSTALL: Tr = tr(
    "No browser found; the agent needs one to open real web pages. Install Chromium now (may ask for your password)? [Y/n]: ",
    "没有找到浏览器；智能体打开真实网页需要它。现在安装 Chromium 吗（可能要求输入你的密码）？[Y/n]：",
);
const BROWSER_INSTALL_FAILED: Tr = tr(
    "! Could not install Chromium: {}",
    "! 无法安装 Chromium：{}",
);
const ABORTED: Tr = tr(
    "Setup ended; nothing was saved.",
    "设置已结束，没有保存任何内容。",
);
const DONE: Tr = tr(
    "Saved {}.\nNext: `runo chat` to talk in the terminal, `runo gateway start|stop|status` for the Web UI in the background, `runo config` for QQ, email and everything else.",
    "已保存 {}。\n接下来：`runo chat` 在终端里聊天，`runo gateway start|stop|status` 在后台运行 Web UI，`runo config` 设置 QQ、邮件和其他选项。",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[
    WELCOME,
    STEP,
    STEP_LANGUAGE,
    STEP_KEY,
    PROXY_ASK,
    PROXY_ASK_SET,
    PROXY_ADDRESS,
    PROXY_ADDRESS_KEEP,
    PROXY_BAD,
    PROXY_SET,
    PROVIDER_TITLE,
    PROVIDER_OPENROUTER,
    PROVIDER_CHATGPT,
    SIGN_IN_HOW,
    SIGN_IN_BROWSER,
    SIGN_IN_DEVICE,
    SEARCH_TITLE,
    SEARCH_AUTO,
    SEARCH_SEARXNG,
    SEARCH_OFF,
    SEARXNG_URL,
    FETCH_ASK,
    SHELL_TITLE,
    WRITE_TITLE,
    PERMIT_ASK,
    PERMIT_ALLOW,
    PERMIT_DENY,
    STEP_PERMISSIONS,
    STEP_TOOLS,
    KEEP_SIGN_IN,
    SIGN_IN_FAILED,
    SIGN_IN_RETRY,
    SIGNED_IN_AS,
    CHATGPT_MODEL,
    OWNERS_HOW,
    OWNERS_NOW,
    FIRST_CHAT_ASK,
    FIRST_CHAT_ON,
    REVOKE_ASK,
    STEP_MODEL,
    STEP_IMAGES,
    STEP_GATEWAY,
    STEP_COMPANION,
    COMPANION_HOW,
    COMPANION_ASK,
    COMPANION_CITY,
    COMPANION_BIRTHDAY,
    COMPANION_QUIET,
    COMPANION_BAD,
    COMPANION_LATER,
    MY_BIRTHDAY,
    LANGUAGE_TITLE,
    PICK_MODEL,
    SEARCH_AGAIN,
    TYPE_ID,
    GATEWAY_HOW,
    GATEWAY_ASK,
    GATEWAY_LATER,
    GATEWAY_FAILED,
    PICK_LANGUAGE,
    KEY_FROM_ENV,
    KEY_HOW,
    KEY_PROMPT,
    KEY_KEEP_PROMPT,
    KEY_CHECKING,
    KEY_OK,
    KEY_BAD,
    KEY_UNCHECKED,
    KEY_ENV_BAD,
    KEY_NEEDED,
    LOADING_MODELS,
    NO_CATALOG,
    KEEP_MODEL,
    MODEL_HOW,
    ONLY_TOOLS,
    ONLY_AUDIO,
    SEARCH_PROMPT,
    NO_MATCH,
    PICK_PROMPT,
    ID_PROMPT,
    UNKNOWN_ID,
    NO_TOOLS,
    CHOSEN,
    SEES_IMAGES,
    BLIND,
    IMAGES_UNKNOWN,
    VISION_CHOSEN,
    CONTEXT_AUTO,
    BROWSER_FOUND,
    BROWSER_MISSING,
    BROWSER_INSTALL,
    BROWSER_INSTALL_FAILED,
    ABORTED,
    DONE,
];

/// Room left for the answer: what the provider says an answer may take,
/// else an eighth of the window; at least 4k, at most a quarter.
pub fn budget_for(window: usize, max_output: Option<u64>) -> usize {
    let wanted = max_output.map_or(window / 8, |m| m as usize);
    let reserve = wanted.max(4096).min(window / 4);
    window - reserve
}

/// Model windows seen in the catalog, cached in the state directory.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WindowCache {
    fetched_at: i64,
    /// Model id → (window, longest answer).
    models: std::collections::BTreeMap<String, (u64, Option<u64>)>,
    /// Model id → the thinking levels it takes, for models that list any;
    /// `None` in caches from before it was kept.
    #[serde(default)]
    efforts: Option<std::collections::BTreeMap<String, Vec<String>>>,
}

const WINDOW_CACHE: &str = "model-windows.json";
const WINDOW_CACHE_SECS: i64 = 7 * 86_400;

impl WindowCache {
    fn find(&self, id: &str) -> Option<(u64, Option<u64>)> {
        // Variants such as ":free" or ":online" share the base model's window.
        self.models
            .get(id)
            .or_else(|| self.models.get(id.split(':').next().unwrap_or(id)))
            .copied()
            .filter(|(window, _)| *window > 0)
    }
}

/// What `agent.context_tokens = "auto"` stands for: the smallest window of
/// the models a turn may call (main and image model), less room for the
/// answer. The catalog is read at most once a week; `None` when no window
/// is known, so the caller falls back.
pub async fn auto_context(config: &Config, state: &Path) -> Option<usize> {
    // OpenRouter's catalog knows ChatGPT's models as `openai/<id>`.
    let mut ids = vec![config.openrouter_model().model.trim().to_owned()];
    if let Some(vision) = config
        .agent
        .vision_model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
    {
        // A ChatGPT image model is `openai/<id>` there.
        let vision = vision.trim();
        ids.push(if vision.contains('/') {
            vision.to_owned()
        } else {
            format!("openai/{vision}")
        });
    }
    let cache = catalog_cache(config, state, &ids).await;
    ids.iter()
        .filter_map(|id| cache.find(id))
        .map(|(window, max_output)| budget_for(window as usize, max_output))
        .min()
}

/// The thinking levels OpenRouter's catalog lists for the main model
/// (ChatGPT's as `openai/<id>`): empty when it lists none, `None` when the
/// catalog cannot be read or does not know the model.
pub async fn efforts(config: &Config, state: &Path) -> Option<Vec<String>> {
    let id = config.openrouter_model().model.trim().to_owned();
    let cache = catalog_cache(config, state, std::slice::from_ref(&id)).await;
    cache.find(&id)?;
    let base = id.split(':').next().unwrap_or(&id);
    let efforts = cache.efforts.as_ref()?;
    Some(
        efforts
            .get(&id)
            .or_else(|| efforts.get(base))
            .cloned()
            .unwrap_or_default(),
    )
}

/// The cached catalog, read again from OpenRouter when a week old or when
/// it does not know one of `ids`.
async fn catalog_cache(config: &Config, state: &Path, ids: &[String]) -> WindowCache {
    let path = state.join(WINDOW_CACHE);
    let mut cache: WindowCache = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let fresh = crate::store::now() - cache.fetched_at < WINDOW_CACHE_SECS;
    let complete = cache.efforts.is_some() && ids.iter().all(|id| cache.find(id).is_some());
    if !fresh || !complete {
        let fetched = match OpenRouter::new(&config.openrouter_model().base_url) {
            Ok(catalog) => tokio::time::timeout(Duration::from_secs(10), catalog.models())
                .await
                .ok()
                .and_then(Result::ok),
            Err(_) => None,
        };
        if let Some(models) = fetched.filter(|m| !m.is_empty()) {
            cache = WindowCache {
                fetched_at: crate::store::now(),
                efforts: Some(
                    models
                        .iter()
                        .filter(|m| !m.efforts.is_empty())
                        .map(|m| (m.id.clone(), m.efforts.clone()))
                        .collect(),
                ),
                models: models
                    .into_iter()
                    .map(|m| (m.id, (m.context, m.max_output)))
                    .collect(),
            };
            if let Ok(json) = serde_json::to_vec(&cache) {
                let _ = std::fs::create_dir_all(state);
                let _ = std::fs::write(&path, json);
            }
        }
    }
    cache
}

/// Whether `config` lacks what a model call needs, so setup should run.
pub fn needed(config: &Config) -> bool {
    config.model_id().is_err() || config.model_key().is_err()
}

/// Whether both ends of the process are a terminal someone can answer in.
pub fn interactive() -> bool {
    // SAFETY: isatty has no preconditions.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Runs setup on the process's terminal. Returns the language picked, or
/// `None` when it ended without saving. `offer_gateway` adds the last step,
/// starting the Gateway in the background (not when about to `serve`).
pub async fn run(path: &Path, lang: Lang, offer_gateway: bool) -> Result<Option<Lang>> {
    let config = Config::load(path).unwrap_or_default();
    let catalog = OpenRouter::new(&config.model.base_url)?;
    let mut term = setup::terminal(lang);
    let mut start = || crate::service::install(None, Some(path));
    let mut install = || {
        crate::elevate::install_packages(&["chromium"])?;
        Ok(crate::browser::find_installed())
    };
    let (signed_in, sign_in) = setup::chatgpt_sign_in();
    let host = Host {
        browser: crate::browser::find_installed().filter(|_| config.browser.enabled),
        install_browser: (config.browser.enabled && crate::elevate::can_ask())
            .then_some(&mut install as &mut dyn FnMut() -> Result<Option<PathBuf>>),
        gateway: offer_gateway.then_some(&mut start as &mut dyn FnMut() -> Result<()>),
        sign_in: Some(sign_in),
        signed_in,
        chatgpt_models: Some(setup::chatgpt_models(path)),
    };
    // Typed answers come from a blocking terminal; that is all this process does now.
    wizard(path, &mut term, &catalog, host).await
}

/// What setup can do on this machine besides writing config.toml.
#[derive(Default)]
pub struct Host<'a> {
    /// The browser found, if any.
    pub browser: Option<PathBuf>,
    /// Installs a browser with the package manager (asking for root) and
    /// says where it is now.
    pub install_browser: Option<&'a mut dyn FnMut() -> Result<Option<PathBuf>>>,
    /// Starts the Gateway in the background once the config is saved.
    pub gateway: Option<&'a mut dyn FnMut() -> Result<()>>,
    /// Signs in to ChatGPT; without it, only OpenRouter is offered.
    pub sign_in: Option<setup::SignIn<'a>>,
    /// Who is signed in to ChatGPT already.
    pub signed_in: Option<String>,
    /// Reads the models the ChatGPT plan offers.
    pub chatgpt_models: Option<setup::Loader<'a>>,
}

/// The setup itself, on any terminal and catalog.
pub async fn wizard<R: BufRead, W: Write>(
    path: &Path,
    term: &mut Term<R, W>,
    catalog: &dyn Catalog,
    host: Host<'_>,
) -> Result<Option<Lang>> {
    let Host {
        mut browser,
        install_browser,
        gateway,
        sign_in,
        signed_in,
        mut chatgpt_models,
    } = host;
    let mut doc = setup::load(path)?;
    let config = setup::parse(&doc).unwrap_or_default();
    term.tell(WELCOME, &[])?;

    // 1. Language.
    step(term, 1, STEP_LANGUAGE)?;
    if term.arrows {
        let items = ["中文".to_owned(), "English".to_owned()];
        let start = usize::from(term.lang == Lang::En);
        let title = LANGUAGE_TITLE.get(term.lang);
        match term.select(title, &items, start, &|_| None)? {
            Picked::Item(i) => term.lang = if i == 0 { Lang::Zh } else { Lang::En },
            Picked::Back => {}
            Picked::End => return aborted(term),
        }
    } else {
        let current = match term.lang {
            Lang::Zh => "1",
            Lang::En => "2",
        };
        let Some(answer) = term.ask(&PICK_LANGUAGE.fill(term.lang, &[current]))? else {
            return aborted(term);
        };
        term.lang = match answer.as_str() {
            "1" | "zh" | "中文" => Lang::Zh,
            "2" | "en" | "English" | "english" => Lang::En,
            _ => term.lang,
        };
    }
    let lang_name = match term.lang {
        Lang::Zh => "zh",
        Lang::En => "en",
    };
    setup::set(&mut doc, &["language"], Value::from(lang_name));

    // 2. Where the models run: OpenRouter with a key, or a ChatGPT plan.
    step(term, 2, STEP_KEY)?;
    proxy(&mut doc, term, &config)?;
    let chatgpt = match sign_in {
        Some(sign_in) => match provider(term, &config, sign_in, signed_in)? {
            Some(chatgpt) => chatgpt,
            None => return aborted(term),
        },
        None => false,
    };
    if chatgpt {
        // The model is picked from ChatGPT's list in the next step.
        crate::chatgpt::use_in_doc(&mut doc, || None);
    } else {
        if config.chatgpt() {
            setup::set(&mut doc, &["model", "provider"], Value::from("openrouter"));
        }
        match std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        {
            Some(key) => {
                term.tell(KEY_FROM_ENV, &[])?;
                term.tell(KEY_CHECKING, &[])?;
                match catalog.check_key(&key).await {
                    KeyCheck::Valid(_) => term.tell(KEY_OK, &[])?,
                    KeyCheck::Invalid(why) => {
                        term.tell(KEY_ENV_BAD, &[&why])?;
                        return aborted(term);
                    }
                    KeyCheck::Unknown(why) => term.tell(KEY_UNCHECKED, &[&why])?,
                }
            }
            None => {
                let existing = config
                    .model
                    .api_key
                    .clone()
                    .filter(|k| !k.trim().is_empty());
                if existing.is_none() {
                    term.tell(KEY_HOW, &[])?;
                }
                loop {
                    let prompt = if existing.is_some() {
                        KEY_KEEP_PROMPT
                    } else {
                        KEY_PROMPT
                    };
                    let Some(typed) = term.secret(prompt.get(term.lang))? else {
                        return aborted(term);
                    };
                    let key = match (typed.is_empty(), &existing) {
                        (false, _) => typed,
                        (true, Some(key)) => key.clone(),
                        (true, None) => {
                            term.tell(KEY_NEEDED, &[])?;
                            continue;
                        }
                    };
                    term.tell(KEY_CHECKING, &[])?;
                    match catalog.check_key(&key).await {
                        KeyCheck::Valid(_) => term.tell(KEY_OK, &[])?,
                        KeyCheck::Invalid(why) => {
                            term.tell(KEY_BAD, &[&why])?;
                            continue;
                        }
                        KeyCheck::Unknown(why) => term.tell(KEY_UNCHECKED, &[&why])?,
                    }
                    setup::set(&mut doc, &["model", "api_key"], Value::from(key.as_str()));
                    break;
                }
            }
        }
    }

    // 3. Main model.
    step(term, 3, STEP_MODEL)?;
    if chatgpt {
        // ChatGPT's own list of what the plan offers, not OpenRouter's.
        let list = match chatgpt_models.as_mut() {
            Some(load) => {
                term.tell(LOADING_MODELS, &[])?;
                load().or_else(|err| {
                    term.tell(NO_CATALOG, &[&format!("{err:#}")])?;
                    anyhow::Ok(Vec::new())
                })?
            }
            None => Vec::new(),
        };
        let current = setup::parse(&doc)
            .map(|c| c.model.model.trim().to_owned())
            .unwrap_or_default();
        let listed = list.iter().any(|m| m.id == current);
        let mut model = None;
        if listed {
            let Some(keep) = term.confirm(&KEEP_MODEL.fill(term.lang, &[&current]), true)? else {
                return aborted(term);
            };
            model = keep.then(|| current.clone());
        }
        let model = match model {
            Some(model) => model,
            None if !list.is_empty() => match pick(term, &list, Need::Tools)? {
                Some(model) => model,
                None => return aborted(term),
            },
            None => loop {
                let Some(typed) = term.ask(&CHATGPT_MODEL.fill(term.lang, &[&current]))? else {
                    return aborted(term);
                };
                let typed = if typed.is_empty() {
                    current.clone()
                } else {
                    typed
                };
                if !typed.is_empty() {
                    break typed;
                }
            },
        };
        setup::set(&mut doc, &["model", "model"], Value::from(model.as_str()));
        term.tell(CHOSEN, &[&model])?;
    }
    let models = if chatgpt {
        Vec::new()
    } else {
        term.tell(LOADING_MODELS, &[])?;
        match catalog.models().await {
            Ok(models) => models,
            Err(err) => {
                term.tell(NO_CATALOG, &[&format!("{err:#}")])?;
                Vec::new()
            }
        }
    };
    let current = config.model.model.trim().to_owned();
    let mut model = None;
    if chatgpt {
        model = setup::parse(&doc).ok().map(|c| c.model.model);
    } else if !current.is_empty() {
        let Some(keep) = term.confirm(&KEEP_MODEL.fill(term.lang, &[&current]), true)? else {
            return aborted(term);
        };
        if keep {
            model = Some(current);
        }
    }
    let model = match model {
        Some(model) => model,
        None => match pick(term, &models, Need::Tools)? {
            Some(model) => model,
            None => return aborted(term),
        },
    };
    if !chatgpt {
        setup::set(&mut doc, &["model", "model"], Value::from(model.as_str()));
        term.tell(CHOSEN, &[&model])?;
    }

    // 4. Images, and the browser that needs no setting.
    step(term, 4, STEP_IMAGES)?;
    match models.iter().find(|m| m.id == model) {
        _ if chatgpt => term.tell(SEES_IMAGES, &[&model])?,
        Some(info) if info.images => term.tell(SEES_IMAGES, &[&model])?,
        Some(_) => {
            let Some(wanted) = term.confirm(&BLIND.fill(term.lang, &[&model]), true)? else {
                return aborted(term);
            };
            if wanted {
                let Some(vision) = pick(term, &models, Need::Images)? else {
                    return aborted(term);
                };
                setup::set(
                    &mut doc,
                    &["agent", "vision_model"],
                    Value::from(vision.as_str()),
                );
                term.tell(VISION_CHOSEN, &[&vision])?;
            }
        }
        None => term.tell(IMAGES_UNKNOWN, &[&model])?,
    }
    let config_now = setup::parse(&doc).unwrap_or_default();
    if config_now.agent.context_tokens == crate::config::ContextBudget::Auto
        && let Some(info) = models.iter().find(|m| m.id == model && m.context > 0)
    {
        let window = info.context as usize;
        term.tell(
            CONTEXT_AUTO,
            &[
                &model,
                &crate::config::short_tokens(window),
                &crate::config::short_tokens(budget_for(window, info.max_output)),
            ],
        )?;
    }

    // 5. Tools and the web: search, the browser, reading pages.
    step(term, 5, STEP_TOOLS)?;
    tools_and_web(&mut doc, term)?;
    if browser.is_none()
        && let Some(install) = install_browser
    {
        let Some(yes) = term.confirm(BROWSER_INSTALL.get(term.lang), true)? else {
            return aborted(term);
        };
        if yes {
            term.out.flush()?;
            match install() {
                Ok(found) => browser = found,
                Err(err) => term.tell(BROWSER_INSTALL_FAILED, &[&format!("{err:#}")])?,
            }
        }
    }
    match &browser {
        Some(path) => term.tell(BROWSER_FOUND, &[&path.display().to_string()])?,
        None => term.tell(BROWSER_MISSING, &[])?,
    }

    // 6. Permissions: what runs without asking, and owners on chat apps.
    step(term, 6, STEP_PERMISSIONS)?;
    permissions(&mut doc, term)?;
    owners(&mut doc, term)?;

    // 7. Companion: the heartbeat and an inner life. Optional, so running
    // out of answers here skips it rather than the whole setup.
    step(term, 7, STEP_COMPANION)?;
    companion(&mut doc, term)?;

    // 8. The Gateway, in the background.
    let mut start = None;
    if let Some(gateway) = gateway {
        step(term, 8, STEP_GATEWAY)?;
        let bind = setup::parse(&doc)
            .map(|c| c.gateway.bind)
            .unwrap_or_else(|_| crate::config::GatewayConfig::default().bind);
        term.tell(GATEWAY_HOW, &[&bind])?;
        let Some(yes) = term.confirm(GATEWAY_ASK.get(term.lang), true)? else {
            return aborted(term);
        };
        if yes {
            start = Some(gateway);
        }
    }

    save(path, &doc)?;
    term.say("")?;
    term.tell(DONE, &[&path.display().to_string()])?;
    match start {
        Some(gateway) => {
            term.say("")?;
            term.out.flush()?;
            if let Err(err) = gateway() {
                term.tell(GATEWAY_FAILED, &[&format!("{err:#}")])?;
                term.tell(GATEWAY_LATER, &[])?;
            }
        }
        None => term.tell(GATEWAY_LATER, &[])?,
    }
    Ok(Some(term.lang))
}

/// A menu on a terminal with arrows, numbered lines otherwise: the index
/// picked, `start` for Enter or Esc, `None` when input ended.
fn choose<R: BufRead, W: Write>(
    term: &mut Term<R, W>,
    title: &str,
    items: &[String],
    start: usize,
) -> Result<Option<usize>> {
    if term.arrows {
        return Ok(match term.select(title, items, start, &|_| None)? {
            Picked::Item(i) => Some(i),
            Picked::Back => Some(start),
            Picked::End => None,
        });
    }
    term.say(title)?;
    for (i, item) in items.iter().enumerate() {
        term.say(&format!("  {}) {item}", i + 1))?;
    }
    let Some(answer) = term.ask(&format!("[{}]: ", start + 1))? else {
        return Ok(None);
    };
    Ok(Some(
        answer
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=items.len()).contains(n))
            .map_or(start, |n| n - 1),
    ))
}

/// Asks whether to go through a proxy, before anything signs in or loads
/// a list, and uses the answer right away. Running out of answers keeps
/// what is set.
fn proxy<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    term: &mut Term<R, W>,
    config: &Config,
) -> Result<()> {
    let current = config
        .network
        .proxy
        .clone()
        .filter(|p| !p.trim().is_empty());
    let ask = if current.is_some() {
        PROXY_ASK_SET
    } else {
        PROXY_ASK
    };
    let Some(wanted) = term.confirm(ask.get(term.lang), current.is_some())? else {
        return Ok(());
    };
    if !wanted {
        setup::remove(doc, &["network", "proxy"]);
        crate::net::set(None);
        return Ok(());
    }
    let url = loop {
        let prompt = match &current {
            Some(current) => PROXY_ADDRESS_KEEP.fill(term.lang, &[current]),
            None => PROXY_ADDRESS.get(term.lang).to_owned(),
        };
        let Some(answer) = term.ask(&prompt)? else {
            return Ok(());
        };
        let text = match (answer.trim(), &current) {
            ("", Some(current)) => current.clone(),
            ("", None) => continue,
            (text, _) => text.to_owned(),
        };
        match crate::net::parse(&text) {
            Ok(url) => break setup::proxy_text(&url),
            Err(err) => term.tell(PROXY_BAD, &[&format!("{err:#}")])?,
        }
    };
    setup::set(doc, &["network", "proxy"], Value::from(url.as_str()));
    crate::net::set(Some(&url));
    term.tell(PROXY_SET, &[&url])?;
    Ok(())
}

/// Asks where the models run. `Some(true)` for a ChatGPT plan, signed in;
/// `None` when input ended.
fn provider<R: BufRead, W: Write>(
    term: &mut Term<R, W>,
    config: &Config,
    mut sign_in: setup::SignIn<'_>,
    signed_in: Option<String>,
) -> Result<Option<bool>> {
    let start = usize::from(!config.chatgpt() && config.api_key().is_ok());
    let items = [
        PROVIDER_CHATGPT.get(term.lang).to_owned(),
        PROVIDER_OPENROUTER.get(term.lang).to_owned(),
    ];
    let Some(picked) = choose(term, PROVIDER_TITLE.get(term.lang), &items, start)? else {
        return Ok(None);
    };
    if picked == 1 {
        return Ok(Some(false));
    }
    if let Some(who) = signed_in {
        let Some(keep) = term.confirm(&KEEP_SIGN_IN.fill(term.lang, &[&who]), true)? else {
            return Ok(None);
        };
        if keep {
            return Ok(Some(true));
        }
    }
    let items = [
        SIGN_IN_BROWSER.get(term.lang).to_owned(),
        SIGN_IN_DEVICE.get(term.lang).to_owned(),
    ];
    let Some(how) = choose(term, SIGN_IN_HOW.get(term.lang), &items, 0)? else {
        return Ok(None);
    };
    let device = how == 1;
    loop {
        match sign_in(term, device) {
            Ok(Some(who)) => {
                term.tell(SIGNED_IN_AS, &[&who])?;
                return Ok(Some(true));
            }
            Ok(None) => return Ok(None),
            Err(err) => {
                term.tell(SIGN_IN_FAILED, &[&format!("{err:#}")])?;
                match term.confirm(SIGN_IN_RETRY.get(term.lang), true)? {
                    Some(true) => {}
                    _ => return Ok(None),
                }
            }
        }
    }
}

/// Web search, reading pages without a browser. Running out of answers
/// skips the rest.
fn tools_and_web<R: BufRead, W: Write>(doc: &mut DocumentMut, term: &mut Term<R, W>) -> Result<()> {
    use crate::config::SearchProvider;
    let config = setup::parse(doc).unwrap_or_default();
    let auto = if config.chatgpt() {
        "ChatGPT"
    } else {
        "OpenRouter"
    };
    let items = [
        SEARCH_AUTO.fill(term.lang, &[auto]),
        SEARCH_SEARXNG.get(term.lang).to_owned(),
        SEARCH_OFF.get(term.lang).to_owned(),
    ];
    let start = match config.search.provider {
        SearchProvider::Searxng => 1,
        SearchProvider::Off => 2,
        _ => 0,
    };
    let Some(picked) = choose(term, SEARCH_TITLE.get(term.lang), &items, start)? else {
        return Ok(());
    };
    if picked != start {
        let value = ["auto", "searxng", "off"][picked];
        setup::set(doc, &["search", "provider"], Value::from(value));
    }
    if picked == 1 {
        let current = config.search.searxng_url.clone().unwrap_or_default();
        let Some(url) = term.ask(&SEARXNG_URL.fill(term.lang, &[&current]))? else {
            return Ok(());
        };
        if !url.trim().is_empty() {
            setup::set(doc, &["search", "searxng_url"], Value::from(url.trim()));
        }
    }
    let Some(fetch) = term.confirm(FETCH_ASK.get(term.lang), config.fetch.enabled)? else {
        return Ok(());
    };
    if fetch != config.fetch.enabled {
        setup::set(doc, &["fetch", "enabled"], Value::from(fetch));
    }
    Ok(())
}

/// What runs without asking: shell commands and file writes. Running out
/// of answers skips the rest.
fn permissions<R: BufRead, W: Write>(doc: &mut DocumentMut, term: &mut Term<R, W>) -> Result<()> {
    use crate::config::Permission;
    let config = setup::parse(doc).unwrap_or_default();
    let items = [
        PERMIT_ASK.get(term.lang).to_owned(),
        PERMIT_ALLOW.get(term.lang).to_owned(),
        PERMIT_DENY.get(term.lang).to_owned(),
    ];
    for (title, path, now) in [
        (SHELL_TITLE, ["tools", "shell"], config.tools.shell),
        (WRITE_TITLE, ["tools", "write"], config.tools.write),
    ] {
        let start = match now {
            Permission::Ask => 0,
            Permission::Allow => 1,
            Permission::Deny => 2,
        };
        let Some(picked) = choose(term, title.get(term.lang), &items, start)? else {
            return Ok(());
        };
        if picked != start {
            let value = ["ask", "allow", "deny"][picked];
            setup::set(doc, &path, Value::from(value));
        }
    }
    Ok(())
}

/// Shows the owners on chat apps, offers to let the first private chat
/// claim an app, and to revoke an owner. Running out of answers skips it.
fn owners<R: BufRead, W: Write>(doc: &mut DocumentMut, term: &mut Term<R, W>) -> Result<()> {
    term.tell(OWNERS_HOW, &[])?;
    let listed = setup::chat_owners(doc);
    if !listed.is_empty() {
        term.tell(OWNERS_NOW, &[&listed.join(", ")])?;
    }
    let on = setup::parse(doc).is_ok_and(|c| c.access.first_chat_owner);
    let Some(claim) = term.confirm(FIRST_CHAT_ASK.get(term.lang), on)? else {
        return Ok(());
    };
    if claim != on {
        setup::set(doc, &["access", "first_chat_owner"], Value::from(claim));
    }
    if claim {
        term.tell(FIRST_CHAT_ON, &[])?;
    }
    if !listed.is_empty() && term.confirm(REVOKE_ASK.get(term.lang), false)? == Some(true) {
        setup::revoke_owner(doc, term)?;
    }
    Ok(())
}

/// Asks whether to turn on the heartbeat and the inner life, and with them
/// the city, a birthday and quiet hours.
fn companion<R: BufRead, W: Write>(doc: &mut DocumentMut, term: &mut Term<R, W>) -> Result<()> {
    term.tell(COMPANION_HOW, &[])?;
    let Some(true) = term.confirm(COMPANION_ASK.get(term.lang), true)? else {
        return term.tell(COMPANION_LATER, &[]);
    };
    setup::set(doc, &["heartbeat", "enabled"], Value::from(true));
    setup::set(doc, &["life", "enabled"], Value::from(true));
    if let Some(city) = term.ask(COMPANION_CITY.get(term.lang))?
        && !city.is_empty()
    {
        setup::set(doc, &["life", "city"], Value::from(city.as_str()));
    }
    loop {
        let Some(day) = term.ask(COMPANION_BIRTHDAY.get(term.lang))? else {
            break;
        };
        if day.is_empty() {
            break;
        }
        let valid = day
            .split_once('-')
            .and_then(|(m, d)| Some((m.parse().ok()?, d.parse().ok()?)))
            .is_some_and(|(m, d)| chrono::NaiveDate::from_ymd_opt(2024, m, d).is_some());
        if !valid {
            term.tell(COMPANION_BAD, &[&day])?;
            continue;
        }
        let mut date = toml_edit::InlineTable::new();
        date.insert("date", day.as_str().into());
        date.insert("name", MY_BIRTHDAY.get(term.lang).into());
        let mut dates = toml_edit::Array::new();
        dates.push(date);
        setup::set(doc, &["life", "dates"], Value::Array(dates));
        break;
    }
    loop {
        let Some(quiet) = term.ask(COMPANION_QUIET.get(term.lang))? else {
            break;
        };
        if quiet.is_empty() {
            break;
        }
        if crate::heartbeat::Quiet::parse(&quiet).is_err() {
            term.tell(COMPANION_BAD, &[&quiet])?;
            continue;
        }
        setup::set(
            doc,
            &["heartbeat", "quiet_hours"],
            Value::from(quiet.as_str()),
        );
        break;
    }
    term.tell(COMPANION_LATER, &[])
}

fn step<R: BufRead, W: Write>(term: &mut Term<R, W>, n: usize, title: Tr) -> Result<()> {
    let title = title.get(term.lang);
    term.tell(STEP, &[&n.to_string(), title])
}

fn aborted<R: BufRead, W: Write>(term: &mut Term<R, W>) -> Result<Option<Lang>> {
    term.say("")?;
    term.tell(ABORTED, &[])?;
    Ok(None)
}

fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    let text = doc.to_string();
    toml::from_str::<Config>(&text).context("the new settings do not load")?;
    setup::write_private(path, &text)
}

/// What a model is picked for, which decides what it must be able to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Runs the agent: must call tools.
    Tools,
    /// Runs the agent on messages with images: tools and image input.
    Images,
    /// Transcribes voice messages: audio input.
    Audio,
    /// Writes text, such as summaries: anything goes.
    Text,
}

impl Need {
    fn fits(self, m: &ModelInfo) -> bool {
        match self {
            Self::Tools => m.tools,
            Self::Images => m.tools && m.images,
            Self::Audio => m.audio,
            Self::Text => true,
        }
    }
}

/// Lets the person search the catalog and pick a model for `need`. `None`
/// when input ended.
pub(crate) fn pick<R: BufRead, W: Write>(
    term: &mut Term<R, W>,
    models: &[ModelInfo],
    need: Need,
) -> Result<Option<String>> {
    if models.is_empty() {
        loop {
            let Some(id) = term.ask(ID_PROMPT.get(term.lang))? else {
                return Ok(None);
            };
            if !id.is_empty() {
                return Ok(Some(id));
            }
        }
    }
    term.tell(MODEL_HOW, &[])?;
    match need {
        Need::Tools | Need::Images => term.tell(ONLY_TOOLS, &[])?,
        Need::Audio => term.tell(ONLY_AUDIO, &[])?,
        Need::Text => {}
    }
    loop {
        let Some(query) = term.ask(SEARCH_PROMPT.get(term.lang))? else {
            return Ok(None);
        };
        let found = search(models, &query, need);
        if found.is_empty() {
            term.tell(NO_MATCH, &[])?;
            continue;
        }
        let answer = if term.arrows {
            let mut items: Vec<String> = found.iter().map(|m| describe(m, term.lang)).collect();
            items.push(SEARCH_AGAIN.get(term.lang).into());
            items.push(TYPE_ID.get(term.lang).into());
            match term.select(PICK_MODEL.get(term.lang), &items, 0, &|_| None)? {
                Picked::Item(i) if i < found.len() => (i + 1).to_string(),
                Picked::Item(i) if i == found.len() => continue,
                Picked::Item(_) => {
                    let Some(id) = term.ask(ID_PROMPT.get(term.lang))? else {
                        return Ok(None);
                    };
                    id
                }
                Picked::Back => continue,
                Picked::End => return Ok(None),
            }
        } else {
            for (i, m) in found.iter().enumerate() {
                term.say(&format!("  {:>2}) {}", i + 1, describe(m, term.lang)))?;
            }
            let Some(answer) = term.ask(PICK_PROMPT.get(term.lang))? else {
                return Ok(None);
            };
            answer
        };
        if answer.is_empty() {
            continue;
        }
        let chosen = match answer.parse::<usize>() {
            Ok(n) => match found.get(n.wrapping_sub(1)) {
                Some(m) => (*m).clone(),
                None => continue,
            },
            Err(_) => match models.iter().find(|m| m.id == answer) {
                Some(m) => m.clone(),
                None => {
                    let Some(yes) = term.confirm(&UNKNOWN_ID.fill(term.lang, &[&answer]), false)?
                    else {
                        return Ok(None);
                    };
                    if yes {
                        return Ok(Some(answer));
                    }
                    continue;
                }
            },
        };
        if matches!(need, Need::Tools | Need::Images) && !chosen.tools {
            let Some(yes) = term.confirm(&NO_TOOLS.fill(term.lang, &[&chosen.id]), false)? else {
                return Ok(None);
            };
            if !yes {
                continue;
            }
        }
        return Ok(Some(chosen.id));
    }
}

/// Models matching every word of `query` in their id or name, newest first
/// as the catalog lists them. Models unfit for `need`, and `:batch` variants
/// (asynchronous batch jobs, not chat), are only taken when typed by id.
fn search<'a>(models: &'a [ModelInfo], query: &str, need: Need) -> Vec<&'a ModelInfo> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    models
        .iter()
        .filter(|m| need.fits(m) && !m.id.ends_with(":batch"))
        .filter(|m| {
            let text = format!("{} {}", m.id, m.name).to_lowercase();
            words.iter().all(|w| text.contains(w.as_str()))
        })
        .take(SHOWN)
        .collect()
}

fn describe(m: &ModelInfo, lang: Lang) -> String {
    let mut parts = vec![m.name.clone()];
    if m.context > 0 {
        let context = if m.context >= 1_000_000 {
            format!("{}M", m.context / 1_000_000)
        } else {
            format!("{}K", m.context / 1000)
        };
        parts.push(match lang {
            Lang::En => format!("{context} context"),
            Lang::Zh => format!("{context} 上下文"),
        });
    }
    if let Some((input, output)) = m.price {
        parts.push(if input == 0.0 && output == 0.0 {
            match lang {
                Lang::En => "free".into(),
                Lang::Zh => "免费".into(),
            }
        } else {
            let unit = match lang {
                Lang::En => "per M tokens in/out",
                Lang::Zh => "每百万 token 输入/输出",
            };
            format!("${} / ${} {unit}", money(input), money(output))
        });
    }
    let eye = if m.images { " 🖼" } else { "" };
    format!("{}{eye}  ({})", m.id, parts.join(" · "))
}

fn money(dollars: f64) -> String {
    let text = if dollars > 0.0 && dollars < 0.1 {
        format!("{dollars:.3}")
    } else {
        format!("{dollars:.2}")
    };
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Fake {
        valid: &'static str,
        models: Vec<ModelInfo>,
        checked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Catalog for Fake {
        async fn check_key(&self, key: &str) -> KeyCheck {
            self.checked.lock().unwrap().push(key.to_owned());
            if key == self.valid {
                KeyCheck::Valid(None)
            } else {
                KeyCheck::Invalid("HTTP 401".into())
            }
        }
        async fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(self.models.clone())
        }
    }

    fn model(id: &str, images: bool, tools: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            context: 200_000,
            price: Some((3.0, 15.0)),
            images,
            audio: false,
            tools,
            max_output: None,
            efforts: Vec::new(),
        }
    }

    fn fake() -> Fake {
        Fake {
            valid: "sk-good",
            models: vec![
                model("maker/text-only", false, true),
                model("maker/text-only:batch", false, true),
                model("maker/no-tools", true, false),
                model("other/sees", true, true),
            ],
            checked: Mutex::new(Vec::new()),
        }
    }

    async fn drive(path: &Path, catalog: &Fake, script: &str) -> (Option<Lang>, String) {
        // SAFETY: tests that read OPENROUTER_API_KEY do not run in parallel with this.
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            arrows: false,
            raw: false,
            lang: Lang::En,
        };
        let done = wizard(path, &mut term, catalog, Host::default())
            .await
            .unwrap();
        (done, String::from_utf8(term.out).unwrap())
    }

    #[tokio::test]
    async fn first_run_sets_language_key_model_and_an_image_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let catalog = fake();
        // Chinese; a refused key, then a good one; search "maker", pick the
        // text-only model; yes to an image model, which only lists "other/sees".
        let script = "1\n\nsk-bad\nsk-good\nmaker\n1\n\n\n1\n";
        let (done, out) = drive(&path, &catalog, script).await;
        assert_eq!(done, Some(Lang::Zh), "{out}");
        assert!(
            out.contains("OpenRouter 拒绝了这个 key（HTTP 401）"),
            "{out}"
        );
        assert!(out.contains("maker/text-only"), "{out}");
        assert!(!out.contains(":batch"), "{out}");
        // Models without tools are not offered.
        assert!(
            !out.contains("1) maker/no-tools") && !out.contains("2) maker/no-tools"),
            "{out}"
        );
        assert!(out.contains("不能看图片"), "{out}");
        assert!(out.contains("没有找到浏览器"), "{out}");
        assert_eq!(*catalog.checked.lock().unwrap(), ["sk-bad", "sk-good"]);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.language, Some(Lang::Zh));
        assert_eq!(config.model.api_key.as_deref(), Some("sk-good"));
        assert_eq!(config.model.model, "maker/text-only");
        assert_eq!(config.agent.vision_model.as_deref(), Some("other/sees"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_model_that_sees_needs_no_image_model_and_settings_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# mine\n[model]\napi_key = \"sk-good\"\n[qq]\nenabled = false\n",
        )
        .unwrap();
        let catalog = fake();
        // English; keep the key; pick by full id.
        let script = "2\n\n\nsees\nother/sees\n";
        let (done, out) = drive(&path, &catalog, script).await;
        assert_eq!(done, Some(Lang::En), "{out}");
        assert!(out.contains("other/sees can see images"), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine") && text.contains("[qq]"), "{text}");
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.model.model, "other/sees");
        assert_eq!(config.agent.vision_model, None);
    }

    #[tokio::test]
    async fn ending_early_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (done, out) = drive(&path, &fake(), "1\n\nsk-good\n").await;
        assert_eq!(done, None);
        assert!(out.contains("没有保存"), "{out}");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn arrow_menus_and_starting_the_gateway() {
        // SAFETY: as in drive.
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let catalog = fake();
        // English (already highlighted); down to OpenRouter and a key;
        // search "sees", pick the first; search and web_fetch as they are;
        // yes (highlighted) to installing a browser; shell and file writes
        // as they are; no (highlighted) to the first chat claiming
        // ownership; yes to the companion, a city, a bad then a good
        // birthday, default quiet hours; yes to the Gateway.
        let script = "\r\r\x1b[B\rsk-good\nsees\n\r\r\r\r\r\r\r\rShanghai\n31-02\n03-14\n\n\r";
        let unused = || -> Option<setup::SignIn<'static>> {
            Some(Box::new(|_: &mut dyn setup::Prompt, _| unreachable!()))
        };
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            arrows: true,
            raw: false,
            lang: Lang::En,
        };
        let mut started = 0;
        let mut start = || {
            started += 1;
            Ok(())
        };
        let mut install = || Ok(Some(PathBuf::from("/usr/bin/chromium")));
        let host = Host {
            browser: None,
            install_browser: Some(&mut install),
            gateway: Some(&mut start),
            sign_in: unused(),
            signed_in: None,
            chatgpt_models: None,
        };
        let done = wizard(&path, &mut term, &catalog, host).await.unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert_eq!(done, Some(Lang::En), "{out}");
        assert_eq!(started, 1);
        assert!(out.contains("Install Chromium"), "{out}");
        assert!(out.contains("Browser: /usr/bin/chromium"), "{out}");
        assert!(out.contains("[8/8] Gateway"), "{out}");
        assert!(out.contains("http://127.0.0.1:18789"), "{out}");
        assert!(out.contains("Pick a model › other/sees"), "{out}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.model.model, "other/sees");
        assert!(config.heartbeat.enabled && config.life.enabled);
        assert_eq!(config.life.city.as_deref(), Some("Shanghai"));
        assert_eq!(config.life.dates[0].date, "03-14");
        assert_eq!(config.life.dates[0].name, "my birthday");
        assert!(out.contains("31-02 is not right"), "{out}");
        assert_eq!(config.heartbeat.quiet_hours, "23:00-08:00");
        assert!(!config.access.first_chat_owner);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("first_chat_owner")
        );

        // A Gateway that does not start leaves the saved config and says how.
        let path = dir.path().join("second.toml");
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            arrows: true,
            raw: false,
            lang: Lang::En,
        };
        let mut fail = || anyhow::bail!("port busy");
        let mut install = || anyhow::bail!("apk failed");
        let host = Host {
            browser: None,
            install_browser: Some(&mut install),
            gateway: Some(&mut fail),
            sign_in: unused(),
            signed_in: None,
            chatgpt_models: None,
        };
        wizard(&path, &mut term, &catalog, host).await.unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(
            out.contains("Could not start the Gateway: port busy"),
            "{out}"
        );
        assert!(out.contains("runo gateway start"), "{out}");
        assert!(
            out.contains("Could not install Chromium: apk failed"),
            "{out}"
        );
        assert!(path.exists());
    }

    async fn drive_with(path: &Path, script: &str, host: Host<'_>) -> (Option<Lang>, String) {
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            arrows: false,
            raw: false,
            lang: Lang::En,
        };
        let done = wizard(path, &mut term, &fake(), host).await.unwrap();
        (done, String::from_utf8(term.out).unwrap())
    }

    #[tokio::test]
    async fn signs_in_to_chatgpt_and_sets_up_owners() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut signed = 0;
        let host = Host {
            sign_in: Some(Box::new(|term: &mut dyn setup::Prompt, device| {
                assert!(device);
                signed += 1;
                term.say("open this")?;
                Ok(Some("me@example.org".into()))
            })),
            ..Host::default()
        };
        // English; ChatGPT, with a code; a model;
        // SearXNG and its address; web_fetch as it is; shell commands
        // allowed, file writes as they are; the first chat claims; no
        // companion.
        let script = "2\n\n1\n2\ngpt-5-codex\n2\nhttp://127.0.0.1:8888\n\n2\n\ny\nn\n";
        let (done, out) = drive_with(&path, script, host).await;
        assert_eq!(done, Some(Lang::En), "{out}");
        assert_eq!(signed, 1);
        assert!(
            out.contains("Signed in to ChatGPT: me@example.org"),
            "{out}"
        );
        assert!(!out.contains("API key: "), "{out}");
        assert!(out.contains("claims that app"), "{out}");
        let config = Config::load(&path).unwrap();
        assert!(config.chatgpt());
        assert_eq!(config.model.model, "gpt-5-codex");
        assert_eq!(config.model.api_key, None);
        assert_eq!(
            config.search.provider,
            crate::config::SearchProvider::Searxng
        );
        assert_eq!(
            config.search.searxng_url.as_deref(),
            Some("http://127.0.0.1:8888")
        );
        assert_eq!(config.tools.shell, crate::config::Permission::Allow);
        assert_eq!(config.tools.write, crate::config::Permission::Ask);
        assert!(config.access.first_chat_owner);

        // Signed in already: keep it and the model; revoke the QQ owner.
        std::fs::write(
            &path,
            "[model]\nprovider = \"chatgpt\"\nmodel = \"gpt-5\"\n[access]\nowners = [\"qq:ME\", \"feishu:ou_x\"]\nfirst_chat_owner = true\n",
        )
        .unwrap();
        let host = Host {
            sign_in: Some(Box::new(|_: &mut dyn setup::Prompt, _| unreachable!())),
            signed_in: Some("me@example.org".into()),
            ..Host::default()
        };
        // English; a proxy, typed without its scheme; ChatGPT (the
        // default); keep the sign-in and model; tools and permissions as
        // they are; keep claiming on; revoke the first; no companion.
        let script = "2\ny\nsocks5://x:1\nlocalhost:7890\n\n\n\n\n\n\n\n\ny\n1\nn\n";
        let (done, out) = drive_with(&path, script, host).await;
        assert_eq!(done, Some(Lang::En), "{out}");
        assert!(out.contains("Owners now: qq:ME, feishu:ou_x"), "{out}");
        assert!(out.contains("Revoked qq:ME"), "{out}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.model.model, "gpt-5");
        assert_eq!(config.access.owners, ["feishu:ou_x"]);
        assert!(config.access.first_chat_owner);
        assert!(out.contains("Only http:// proxies work"), "{out}");
        assert!(
            out.contains("Using the proxy http://localhost:7890"),
            "{out}"
        );
        assert_eq!(
            config.network.proxy.as_deref(),
            Some("http://localhost:7890")
        );
        crate::net::set(None);

        // A model the plan no longer offers: picked from ChatGPT's list.
        std::fs::write(&path, "[model]\nprovider = \"chatgpt\"\nmodel = \"gone\"\n").unwrap();
        let host = Host {
            sign_in: Some(Box::new(|_: &mut dyn setup::Prompt, _| unreachable!())),
            signed_in: Some("me@example.org".into()),
            chatgpt_models: Some(Box::new(|| {
                Ok(vec![model("gpt-a", true, true), model("gpt-b", true, true)])
            })),
            ..Host::default()
        };
        // English; ChatGPT; keep the sign-in; list all, take the second;
        // tools and permissions as they are; no claiming; no companion.
        let script = "2\n\n\n\n\n2\n\n\n\n\nn\nn\n";
        let (done, out) = drive_with(&path, script, host).await;
        assert_eq!(done, Some(Lang::En), "{out}");
        assert!(
            out.contains("gpt-a") && !out.contains("Keep the model"),
            "{out}"
        );
        assert_eq!(Config::load(&path).unwrap().model.model, "gpt-b");
    }

    #[test]
    fn leaves_room_for_the_answer() {
        assert_eq!(budget_for(200_000, Some(64_000)), 150_000);
        assert_eq!(budget_for(200_000, None), 175_000);
        assert_eq!(budget_for(128_000, Some(16_384)), 111_616);
        assert_eq!(budget_for(8_192, None), 6_144);
        let cache = WindowCache {
            fetched_at: 0,
            models: [("a/b".to_owned(), (131_072, None))].into(),
            efforts: Some([("a/b".to_owned(), vec!["low".to_owned()])].into()),
        };
        assert_eq!(cache.find("a/b:free"), Some((131_072, None)));
        assert_eq!(cache.find("c/d"), None);
    }

    #[test]
    fn every_text_has_both_languages() {
        crate::i18n::assert_complete(ALL);
    }

    #[test]
    fn catalog_entries_parse_with_price_images_and_tools() {
        let body = serde_json::json!({"data": [
            {"id": "a/b", "name": "A: B", "context_length": 1_000_000,
             "architecture": {"input_modalities": ["text", "image"]},
             "pricing": {"prompt": "0.000003", "completion": "0.000015"},
             "supported_parameters": ["tools", "temperature", "reasoning"],
             "reasoning": {"mandatory": false, "supported_efforts": ["high", "medium", "low"]}},
            {"id": "c/d"}
        ]});
        let models = parse_models(&body);
        assert_eq!(models[0].id, "a/b");
        assert!(models[0].images && models[0].tools);
        assert_eq!(models[0].efforts, ["high", "medium", "low"]);
        let (input, output) = models[0].price.unwrap();
        assert!((input - 3.0).abs() < 1e-9 && (output - 15.0).abs() < 1e-9);
        assert_eq!(
            describe(&models[0], Lang::En),
            "a/b 🖼  (A: B · 1M context · $3 / $15 per M tokens in/out)"
        );
        assert!(!models[1].tools && models[1].efforts.is_empty() && models[1].price.is_none());
        assert_eq!(money(0.004), "0.004");
        let key = serde_json::json!({"data": {"limit_remaining": 4.5, "usage": 5.5}});
        assert_eq!(credit(&key).as_deref(), Some("$4.50 left"));
        let key = serde_json::json!({"data": {"limit_remaining": null, "usage": 1.25}});
        assert_eq!(credit(&key).as_deref(), Some("$1.25 used, no limit"));
        assert_eq!(money(2.5), "2.5");
        assert_eq!(search(&models, "", Need::Tools).len(), 1);
    }
}
