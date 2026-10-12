//! `openclaw-rs config`: an interactive editor for config.toml, in English
//! or Chinese.
//!
//! Edits the file in place with `toml_edit`, so comments and keys it does not
//! know survive. Every change is checked by parsing the whole file as a
//! `Config`; a change that does not parse is undone on the spot.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::config::{Config, ReviewProvider};
pub use crate::i18n::Lang;
use crate::i18n::{Tr, tr};
use crate::onboard::{ModelInfo, Need};

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Int,
    /// US dollars; 0 means no limit.
    Money,
    Bool,
    Choice(&'static [&'static str]),
    /// Comma-separated strings.
    List,
    /// Comma-separated `MM-DD name` pairs, kept as `{ date, name }` tables.
    Dates,
    /// `auto`, or a token count written like 128k or 1m.
    Tokens,
    /// A model id, picked from OpenRouter's list when it can be loaded.
    Model(Need),
    /// Which decision model to use; shown with the model it runs, whose id
    /// is at `model` (a different provider resets it).
    Decider {
        model: &'static [&'static str],
        base_url: &'static [&'static str],
    },
    /// Read without echo; `env` overrides the file when set.
    Secret {
        env: &'static str,
        generate: bool,
    },
    /// Signs in to ChatGPT, in a browser or with a device code.
    SignIn,
    /// An `http://host:port` proxy address.
    Proxy,
    /// Where `web_search` looks: `auto` and what this setup can use.
    Search,
    /// Removes one chat app's owner from the list at `path`.
    Revoke,
    /// How hard the main model thinks: `auto`, `off`, or one of the levels
    /// the model's list says it takes.
    Effort,
}

/// Every value `model.reasoning_effort` takes, for typed answers.
const SEARCHES: &[&str] = &["auto", "chatgpt", "openrouter", "searxng", "off"];

/// Whether `field` means anything with `config`; the rest stay hidden.
fn relevant(field: &Field, config: &Config) -> bool {
    use crate::config::SearchProvider;
    match field.path {
        ["model", "fallbacks"] | ["model", "prompt_cache"] => !config.chatgpt(),
        ["qq" | "feishu" | "zulip", "proxy"] => config
            .network
            .proxy
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty()),
        ["search", "searxng_url"] => config.search.provider == SearchProvider::Searxng,
        ["search", "model"] => config.search_provider() == SearchProvider::Openrouter,
        ["tools", "review", "model"] => config.tools.review.provider != ReviewProvider::Off,
        _ => true,
    }
}

const EFFORTS: &[&str] = &[
    "auto", "off", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

const BROKEN: Tr = tr(
    "warning: the current file does not load ({}); fix the field it names below",
    "警告：当前配置文件无法加载（{}）；请在下面修改它指出的字段",
);
const EDITING: Tr = tr("Editing {}", "正在编辑 {}");
const HOW: Tr = tr(
    "In each field: Enter keeps the value, - resets it to the default, ? explains it.",
    "每个字段：回车保持不变，- 恢复默认值，? 查看说明。",
);
const SAVE: Tr = tr("Save and exit", "保存并退出");
const QUIT: Tr = tr("Quit without saving", "不保存退出");
const PICK_MENU: Tr = tr("Pick a number, s or q.", "请输入编号、s 或 q。");
const BACK_PROMPT: Tr = tr("Number, Enter goes back: ", "输入编号，回车返回：");
const NO_CHANGES: Tr = tr("No changes.", "没有改动。");
const NOT_SAVED: Tr = tr("Quit without saving.", "已退出，未保存。");
const SAVED: Tr = tr("Saved {}.", "已保存 {}。");
const RESTART: Tr = tr(
    "Restart a running Gateway (openclaw-rs service restart) to apply it.",
    "如果 Gateway 正在运行，重启后生效（openclaw-rs service restart）。",
);
const RESTARTED: Tr = tr(
    "Restarted the Gateway so the change applies now.",
    "已重启 Gateway，修改立即生效。",
);
const RESULT_INVALID: Tr = tr(
    "not saved: the result does not load: {}",
    "未保存：修改后的配置无法加载：{}",
);
const CANNOT_RESET: Tr = tr("cannot reset: {}", "无法恢复默认：{}");
const NOT_ACCEPTED: Tr = tr("not accepted: {}", "不接受这个值：{}");
const NOT_NUMBER: Tr = tr("{} is not a whole number", "{} 不是整数");
const NOT_TOKENS: Tr = tr(
    "{} is not a token count: type auto, or e.g. 128k or 1m",
    "{} 不是 token 数：输入 auto，或者如 128k、1m",
);
const OTHER: Tr = tr("Other…", "其他…");
const SEARCH_MODELS: Tr = tr("Search OpenRouter's models…", "搜索 OpenRouter 上的模型…");
const LOADING_MODELS: Tr = tr("Loading the model list...", "正在获取模型列表……");
const NO_MODEL_LIST: Tr = tr(
    "Could not load the model list ({}); type the id.",
    "无法获取模型列表（{}），请直接输入 id。",
);
/// Quick picks for a token count in the arrow menu.
const TOKEN_PICKS: &[&str] = &["auto", "32k", "64k", "128k", "200k", "1m"];
const YES_NO: Tr = tr("answer y or n", "请回答 y 或 n");
const NOT_A_DATE: Tr = tr(
    "{} is not MM-DD and a name, e.g. 03-14 my birthday",
    "{} 不是“月-日 名字”的格式，例如 03-14 我的生日",
);
const PICK_ONE: Tr = tr("pick one of: {}", "请从这些中选一个：{}");
const DEFAULT: Tr = tr("default", "默认");
const EMPTY: Tr = tr("empty", "空");
const NONE: Tr = tr("none", "无");
const FROM_ENV: Tr = tr("from ${}", "来自 ${}");
const IN_FILE: Tr = tr("set in this file", "已在本文件中设置");
const NOT_SET: Tr = tr("not set", "未设置");
const NOT_SIGNED_IN: Tr = tr("not signed in", "未登录");
const NO_SIGN_IN: Tr = tr(
    "Signing in is not possible here; run `openclaw-rs login`.",
    "这里无法登录，请运行 `openclaw-rs login`。",
);
const SIGN_IN_FAILED: Tr = tr("Could not sign in: {}", "登录失败：{}");
const SIGN_IN_BROWSER: Tr = tr("Sign in in a browser", "在浏览器里登录");
const SIGN_IN_DEVICE: Tr = tr(
    "Sign in with a code (on a phone or another device)",
    "用验证码登录（在手机或其他设备上输入）",
);
const SIGNED_IN_PICK: Tr = tr(
    "Signed in. ChatGPT's models are now in the model lists; the main model stays as it is until you pick one.",
    "已登录。ChatGPT 的模型已经出现在模型列表里；主模型保持不变，需要时在「模型」里选。",
);
const ON_CHATGPT: Tr = tr(
    "The model runs on your ChatGPT subscription once saved: model.provider = \"chatgpt\", model.model = \"{}\".",
    "保存后模型使用你的 ChatGPT 订阅：model.provider = \"chatgpt\"，model.model = \"{}\"。",
);
const ON_CHATGPT_PICK: Tr = tr(
    "The model runs on your ChatGPT subscription once saved (model.provider = \"chatgpt\"); ChatGPT's model list could not be read, so pick the model under Model.",
    "保存后模型使用你的 ChatGPT 订阅（model.provider = \"chatgpt\"）；读不到 ChatGPT 的模型列表，请在「模型」里选择模型。",
);
const AUTO_EFFORT: Tr = tr(
    "auto (the model's lowest from low up: {})",
    "auto（模型从 low 起最低的一档：{}）",
);
const OFF_EFFORT: Tr = tr(
    "off (send nothing; the model decides)",
    "off（不发送，由模型自己决定）",
);
const NO_CHAT_OWNERS: Tr = tr(
    "No chat app (QQ, Feishu, Zulip) has an owner.",
    "QQ、飞书、Zulip 都还没有所有者。",
);
const REVOKED: Tr = tr("Revoked {}.", "已撤销 {}。");
const REVOKE_TITLE: Tr = tr("Revoke which owner?", "撤销哪个所有者？");
const REVOKE_WHICH: Tr = tr(
    "Revoke which? A number or id, Enter for none: ",
    "撤销哪一个？输入编号或 id，回车跳过：",
);

struct Field {
    path: &'static [&'static str],
    label: Tr,
    help: Tr,
    kind: Kind,
}

struct Section {
    title: Tr,
    fields: &'static [Field],
}

/// A first-level menu entry: related sections, e.g. the chat apps.
struct Group {
    title: Tr,
    sections: &'static [Section],
}

/// Every section, in menu order.
fn sections() -> impl Iterator<Item = &'static Section> {
    GROUPS.iter().flat_map(|g| g.sections)
}

const PERMISSION: &[&str] = &["allow", "ask", "deny"];
const DECIDERS: &[&str] = &["off", "openrouter", "openrouter-chat", "typesafe"];
const SECURITY: &[&str] = &["tls", "starttls", "none"];

const GROUPS: &[Group] = &[
    Group {
        title: tr("Language", "语言"),
        sections: &[Section {
            title: tr("Language", "语言"),
            fields: &[Field {
                path: &["language"],
                label: tr("Language", "语言"),
                help: tr(
                    "Language of CLI output, this editor and the program's own chat replies (/compact, /identity, errors). Reset (-) to follow the system locale. The Web UI follows the browser and has its own switch.",
                    "CLI 输出、本编辑器和程序自己的聊天回复（/compact、/identity、错误提示）的语言。输入 - 恢复为跟随系统语言。Web UI 跟随浏览器语言，并有自己的切换按钮。",
                ),
                kind: Kind::Choice(&["en", "zh"]),
            }],
        }],
    },
    Group {
        title: tr("Model", "模型"),
        sections: &[
            Section {
                title: tr("Provider and sign-in", "模型提供方与登录"),
                fields: &[
                    Field {
                        path: &["network", "proxy"],
                        label: tr("Network proxy", "网络代理"),
                        help: tr(
                            "An HTTP proxy (http://host:port, e.g. Clash's 7890) for the model providers, sign-in, search, voice and updates. Chat apps use it only where \"Use the network proxy\" is on in their section. Local addresses, web_fetch and email go direct. Reset (-) to go direct.",
                            "HTTP 代理（http://主机:端口，例如 Clash 的 7890），模型提供方、登录、搜索、语音和更新都走它。聊天应用只有在各自设置里打开“使用网络代理”才走它。本机和局域网地址、web_fetch 和邮件直连。输入 - 取消代理。",
                        ),
                        kind: Kind::Proxy,
                    },
                    Field {
                        path: &["model", "provider"],
                        label: tr("ChatGPT account", "ChatGPT 账号"),
                        help: tr(
                            "Sign in with your ChatGPT plan: in a browser, or with a code entered on any device (a phone too). Its models then appear in the model lists.",
                            "用你的 ChatGPT 订阅登录：在浏览器里登录，或者在任意设备（手机也行）上输入一个码。登录后它的模型会出现在模型列表里。",
                        ),
                        kind: Kind::SignIn,
                    },
                    Field {
                        path: &["model", "api_key"],
                        label: tr("OpenRouter API key", "OpenRouter API key"),
                        help: tr(
                            "For OpenRouter's models (ids with a /). Stored in config.toml (mode 0600); OPENROUTER_API_KEY wins when set.",
                            "用 OpenRouter 的模型（id 里带 /）时需要。存在 config.toml（权限 0600）；设了 OPENROUTER_API_KEY 时以它为准。",
                        ),
                        kind: Kind::Secret {
                            env: "OPENROUTER_API_KEY",
                            generate: false,
                        },
                    },
                    Field {
                        path: &["model", "model"],
                        label: tr("Model", "模型"),
                        help: tr(
                            "Picked from your ChatGPT plan's models and OpenRouter's; the pick decides which one runs it.",
                            "从你的 ChatGPT 订阅和 OpenRouter 的模型里选；选了哪家的模型就用哪家。",
                        ),
                        kind: Kind::Model(Need::Tools),
                    },
                    Field {
                        path: &["model", "reasoning_effort"],
                        label: tr("Thinking level", "思考档位"),
                        help: tr(
                            "How hard the model thinks before answering. auto (the default) reads the levels the model supports and takes the lowest from low up; off sends nothing and leaves it to the model; or pick one of the model's levels.",
                            "模型回答前思考的程度。auto（默认）读取模型支持的档位，自动选从 low 起最低的一档；off 不发送，交给模型自己决定；也可以选模型支持的某一档。",
                        ),
                        kind: Kind::Effort,
                    },
                    Field {
                        path: &["model", "fallbacks"],
                        label: tr("Fallback models", "备用模型"),
                        help: tr(
                            "Comma-separated; OpenRouter tries them in order when the model fails.",
                            "用逗号分隔；主模型失败时 OpenRouter 按顺序尝试。",
                        ),
                        kind: Kind::List,
                    },
                ],
            },
            Section {
                title: tr("Context and model calls", "上下文与模型调用"),
                fields: &[
                    Field {
                        path: &["model", "max_retries"],
                        label: tr("Retries on transient errors", "临时错误重试次数"),
                        help: tr(
                            "Connection errors, HTTP 408/429/5xx, and streams that fail before any text.",
                            "连接错误、HTTP 408/429/5xx，以及还没输出文字就失败的流。",
                        ),
                        kind: Kind::Int,
                    },
                    Field {
                        path: &["model", "prompt_cache"],
                        label: tr("Prompt cache breakpoints", "Prompt 缓存标记"),
                        help: tr(
                            "auto: for anthropic/ and google/ models; on: always (e.g. openrouter/auto routing to Claude); off: never.",
                            "auto：只对 anthropic/ 和 google/ 模型加；on：总是加（例如 openrouter/auto 会路由到 Claude 时）；off：不加。",
                        ),
                        kind: Kind::Choice(&["auto", "on", "off"]),
                    },
                    Field {
                        path: &["agent", "context_tokens"],
                        label: tr("Context budget", "上下文预算"),
                        help: tr(
                            "Per model call: system prompt, tools, summary and history. auto takes it from the model's window on OpenRouter (checked weekly), leaving room for the answer; or a count such as 128k or 1m, below the model's window.",
                            "每次调用模型的总预算：系统提示、工具、摘要和历史。auto 从 OpenRouter 上模型的窗口自动获取（每周检查一次）并给回答留出空间；也可以写 128k、1m 这样的数，要小于模型的窗口。",
                        ),
                        kind: Kind::Tokens,
                    },
                    Field {
                        path: &["agent", "summary_model"],
                        label: tr("Summary model", "摘要模型"),
                        help: tr(
                            "Model that writes the context summary, e.g. a cheaper one. Reset (-) to use the main model.",
                            "写上下文摘要的模型，例如更便宜的模型。输入 - 恢复为主模型。",
                        ),
                        kind: Kind::Model(Need::Text),
                    },
                    Field {
                        path: &["agent", "vision_model"],
                        label: tr("Image model", "图片模型"),
                        help: tr(
                            "Model for messages with images (files people send, browser screenshots). Reset (-) to use the main model; set it when the main model has no image input.",
                            "处理带图片消息（用户发来的图片、浏览器截图）的模型。输入 - 恢复为主模型；主模型不支持图片输入时需要设置。",
                        ),
                        kind: Kind::Model(Need::Images),
                    },
                    Field {
                        path: &["agent", "audio_model"],
                        label: tr("Audio model", "音频模型"),
                        help: tr(
                            "Model that transcribes voice messages (email, Web UI, QQ when QQ sends no transcript). Reset (-) to use the main model; set it when the main model has no audio input.",
                            "把语音消息转成文字的模型（邮件、Web UI，以及 QQ 没给识别结果时）。输入 - 恢复为主模型；主模型不支持音频输入时需要设置。",
                        ),
                        kind: Kind::Model(Need::Audio),
                    },
                    Field {
                        path: &["agent", "recall_limit"],
                        label: tr("Memories recalled per message", "每条消息自动召回的记忆数"),
                        help: tr(
                            "Saved memories that share words with a message are shown to the model with it. 0 turns this off.",
                            "与消息有相同词语的已保存记忆，会随消息一起交给模型。设为 0 关闭。",
                        ),
                        kind: Kind::Int,
                    },
                    Field {
                        path: &["agent", "max_steps"],
                        label: tr("Max model calls per turn", "每轮最多调用模型次数"),
                        help: tr("Stops a runaway tool loop.", "防止工具调用无限循环。"),
                        kind: Kind::Int,
                    },
                ],
            },
            Section {
                title: tr("Messages during a reply", "回复中插入消息"),
                fields: &[
                    Field {
                        path: &["guide", "enabled"],
                        label: tr("Add messages to a running turn", "对话中插入补充消息"),
                        help: tr(
                            "Messages sent while a turn runs join it at a good moment instead of waiting; see README \"Guided conversation\".",
                            "回复进行中发来的消息会在合适的时机加入当前这一轮，而不是排队等待；见 README 的 \"Guided conversation\"。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["guide", "provider"],
                        label: tr("Model that picks the moment", "判断插入时机的模型"),
                        help: tr(
                            "openrouter: TypeSafe's Jev decision model with your OpenRouter key; off: insert at the next step.",
                            "openrouter：通过 OpenRouter 密钥使用 TypeSafe 的 Jev 决策模型；off：在下一步直接插入。",
                        ),
                        kind: Kind::Decider {
                            model: &["guide", "model"],
                            base_url: &["guide", "base_url"],
                        },
                    },
                    Field {
                        path: &["guide", "model"],
                        label: tr("Its model id", "它的模型 id"),
                        help: tr(
                            "Reset (-) for the provider's default, shown on the line above.",
                            "输入 - 使用该服务的默认模型，即上一行显示的那个。",
                        ),
                        kind: Kind::Text,
                    },
                ],
            },
        ],
    },
    Group {
        title: tr("Chat apps and email", "聊天平台与邮件"),
        sections: &[
            Section {
                title: tr("QQ bot", "QQ 机器人"),
                fields: &[
                    Field {
                        path: &["qq", "enabled"],
                        label: tr("Enabled", "启用"),
                        help: tr(
                            "Connects to QQ over WebSocket when `serve` runs.",
                            "运行 `serve` 时通过 WebSocket 连接 QQ。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["qq", "proxy"],
                        label: tr("Use the network proxy", "使用网络代理"),
                        help: tr(
                            "Reach QQ through network.proxy (Model › Provider and sign-in). Off connects directly; most need no proxy.",
                            "通过网络代理（模型 › 模型提供方与登录）连接QQ。关闭则直连；大多数情况不需要代理。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["qq", "app_id"],
                        label: tr("AppID", "AppID"),
                        help: tr(
                            "From the QQ Open Platform (q.qq.com).",
                            "在 QQ 开放平台（q.qq.com）获取。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["qq", "app_secret"],
                        label: tr("AppSecret", "AppSecret"),
                        help: tr(
                            "QQ_APP_SECRET wins when set.",
                            "设了 QQ_APP_SECRET 时以环境变量为准。",
                        ),
                        kind: Kind::Secret {
                            env: "QQ_APP_SECRET",
                            generate: false,
                        },
                    },
                    Field {
                        path: &["qq", "allow"],
                        label: tr("Allowed openids", "允许的 openid"),
                        help: tr(
                            "Comma-separated user or group openids; empty lets everyone chat. The log prints each sender's openid.",
                            "用逗号分隔的用户或群 openid；留空则所有人都能聊天。日志里会打印每个发送者的 openid。",
                        ),
                        kind: Kind::List,
                    },
                    Field {
                        path: &["qq", "split_replies"],
                        label: tr("Split chatty replies", "闲聊回复分条发送"),
                        help: tr(
                            "A short reply of a few paragraphs goes out as up to three messages, a moment apart, like texting.",
                            "几段话的简短回复拆成最多三条消息，间隔片刻发出，像真人发消息。",
                        ),
                        kind: Kind::Bool,
                    },
                ],
            },
            Section {
                title: tr("Feishu / Lark bot", "飞书机器人"),
                fields: &[
                    Field {
                        path: &["feishu", "enabled"],
                        label: tr("Enabled", "启用"),
                        help: tr(
                            "Connects to Feishu over the long connection when `serve` runs; no public address needed. In the developer console: add the bot capability, the im:message permissions, and subscribe to im.message.receive_v1 and the card.action.trigger callback with the long connection.",
                            "运行 `serve` 时通过长连接接入飞书，不需要公网地址。在开发者后台：添加机器人能力和 im:message 相关权限，事件与回调都选“长连接”，订阅 im.message.receive_v1 事件和 card.action.trigger 回调。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["feishu", "proxy"],
                        label: tr("Use the network proxy", "使用网络代理"),
                        help: tr(
                            "Reach Feishu through network.proxy (Model › Provider and sign-in). Off connects directly; most need no proxy.",
                            "通过网络代理（模型 › 模型提供方与登录）连接飞书。关闭则直连；大多数情况不需要代理。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["feishu", "app_id"],
                        label: tr("App ID", "App ID"),
                        help: tr(
                            "cli_… from the app's credentials page (open.feishu.cn).",
                            "应用凭证页里的 cli_…（open.feishu.cn）。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["feishu", "app_secret"],
                        label: tr("App Secret", "App Secret"),
                        help: tr(
                            "FEISHU_APP_SECRET wins when set.",
                            "设了 FEISHU_APP_SECRET 时以环境变量为准。",
                        ),
                        kind: Kind::Secret {
                            env: "FEISHU_APP_SECRET",
                            generate: false,
                        },
                    },
                    Field {
                        path: &["feishu", "allow"],
                        label: tr("Allowed senders or groups", "允许的用户或群"),
                        help: tr(
                            "Comma-separated open_ids (ou_…) or group chat_ids (oc_…); empty lets everyone in the tenant chat. The log prints each sender.",
                            "用逗号分隔的 open_id（ou_…）或群 chat_id（oc_…）；留空则企业里所有人都能聊天。日志里会打印每个发送者。",
                        ),
                        kind: Kind::List,
                    },
                    Field {
                        path: &["feishu", "api_base"],
                        label: tr("Server", "服务器"),
                        help: tr(
                            "https://open.feishu.cn, or https://open.larksuite.com for Lark.",
                            "https://open.feishu.cn；国际版 Lark 用 https://open.larksuite.com。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["feishu", "split_replies"],
                        label: tr("Split chatty replies", "闲聊回复分条发送"),
                        help: tr(
                            "A short reply of a few paragraphs goes out as up to three messages, a moment apart, like texting.",
                            "几段话的简短回复拆成最多三条消息，间隔片刻发出，像真人发消息。",
                        ),
                        kind: Kind::Bool,
                    },
                ],
            },
            Section {
                title: tr("Zulip bot", "Zulip 机器人"),
                fields: &[
                    Field {
                        path: &["zulip", "enabled"],
                        label: tr("Enabled", "启用"),
                        help: tr(
                            "Polls Zulip for messages when `serve` runs. Create a Generic bot under Personal settings → Bots.",
                            "运行 `serve` 时从 Zulip 拉取消息。在 个人设置 → 机器人 里创建一个 Generic 机器人。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["zulip", "proxy"],
                        label: tr("Use the network proxy", "使用网络代理"),
                        help: tr(
                            "Reach Zulip through network.proxy (Model › Provider and sign-in). Off connects directly; most need no proxy.",
                            "通过网络代理（模型 › 模型提供方与登录）连接Zulip。关闭则直连；大多数情况不需要代理。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["zulip", "site"],
                        label: tr("Organization URL", "组织地址"),
                        help: tr(
                            "e.g. https://example.zulipchat.com",
                            "例如 https://example.zulipchat.com",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["zulip", "email"],
                        label: tr("Bot email", "机器人邮箱"),
                        help: tr(
                            "Shown on the bot's settings, e.g. claw-bot@example.zulipchat.com.",
                            "机器人设置里显示的邮箱，例如 claw-bot@example.zulipchat.com。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["zulip", "api_key"],
                        label: tr("API key", "API key"),
                        help: tr(
                            "ZULIP_API_KEY wins when set.",
                            "设了 ZULIP_API_KEY 时以环境变量为准。",
                        ),
                        kind: Kind::Secret {
                            env: "ZULIP_API_KEY",
                            generate: false,
                        },
                    },
                    Field {
                        path: &["zulip", "allow"],
                        label: tr("Allowed senders or channels", "允许的用户或频道"),
                        help: tr(
                            "Comma-separated sender emails or channel names; empty lets everyone in the organization chat. In channels the bot answers only when @-mentioned.",
                            "用逗号分隔的发送者邮箱或频道名；留空则组织里所有人都能聊天。在频道里只有 @ 机器人才会回复。",
                        ),
                        kind: Kind::List,
                    },
                    Field {
                        path: &["zulip", "split_replies"],
                        label: tr("Split chatty replies", "闲聊回复分条发送"),
                        help: tr(
                            "A short reply of a few paragraphs goes out as up to three messages, a moment apart, like texting.",
                            "几段话的简短回复拆成最多三条消息，间隔片刻发出，像真人发消息。",
                        ),
                        kind: Kind::Bool,
                    },
                ],
            },
            Section {
                title: tr("Email", "邮件"),
                fields: &[
                    Field {
                        path: &["mail", "enabled"],
                        label: tr("Enabled", "启用"),
                        help: tr(
                            "Polls IMAP and answers by SMTP when `serve` runs.",
                            "运行 `serve` 时轮询 IMAP，并通过 SMTP 回复。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["mail", "mode"],
                        label: tr("Mode", "模式"),
                        help: tr(
                            "allow: read and answer only allowed senders. receive: read mail from anyone, send only to allowed addresses. open: read from and answer anyone (anyone can spend your model credits).",
                            "allow：只读取并回复允许的发件人。receive：读取任何人的邮件，只给允许的地址发信。open：读取并回复任何人（任何人都能花你的模型额度）。",
                        ),
                        kind: Kind::Choice(&["allow", "receive", "open"]),
                    },
                    Field {
                        path: &["mail", "imap_host"],
                        label: tr("IMAP host", "IMAP 服务器"),
                        help: tr("e.g. imap.qq.com", "例如 imap.qq.com"),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["mail", "imap_port"],
                        label: tr("IMAP port", "IMAP 端口"),
                        help: tr("993 for TLS.", "TLS 用 993。"),
                        kind: Kind::Int,
                    },
                    Field {
                        path: &["mail", "imap_security"],
                        label: tr("IMAP security", "IMAP 加密方式"),
                        help: tr(
                            "none is only accepted for loopback hosts.",
                            "none 只能用于本机地址。",
                        ),
                        kind: Kind::Choice(SECURITY),
                    },
                    Field {
                        path: &["mail", "smtp_host"],
                        label: tr("SMTP host", "SMTP 服务器"),
                        help: tr("e.g. smtp.qq.com", "例如 smtp.qq.com"),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["mail", "smtp_port"],
                        label: tr("SMTP port", "SMTP 端口"),
                        help: tr(
                            "465 for TLS, 587 for STARTTLS.",
                            "TLS 用 465，STARTTLS 用 587。",
                        ),
                        kind: Kind::Int,
                    },
                    Field {
                        path: &["mail", "smtp_security"],
                        label: tr("SMTP security", "SMTP 加密方式"),
                        help: tr(
                            "none is only accepted for loopback hosts.",
                            "none 只能用于本机地址。",
                        ),
                        kind: Kind::Choice(SECURITY),
                    },
                    Field {
                        path: &["mail", "username"],
                        label: tr("Username", "用户名"),
                        help: tr("Usually the full address.", "通常是完整的邮箱地址。"),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["mail", "password"],
                        label: tr("Password", "密码"),
                        help: tr(
                            "Many providers (QQ Mail, 163) need an app authorization code. MAIL_PASSWORD wins when set.",
                            "很多邮箱（QQ 邮箱、163）要填授权码。设了 MAIL_PASSWORD 时以环境变量为准。",
                        ),
                        kind: Kind::Secret {
                            env: "MAIL_PASSWORD",
                            generate: false,
                        },
                    },
                    Field {
                        path: &["mail", "from"],
                        label: tr("Reply-from address", "回复的发件地址"),
                        help: tr("Reset (-) to use the username.", "输入 - 恢复为用户名。"),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["mail", "allow"],
                        label: tr("Allowed senders", "允许的发件人"),
                        help: tr(
                            "Comma-separated addresses or @domain. Mode allow: who may email the bot (required). Mode receive: who it may send to.",
                            "用逗号分隔的地址或 @域名。allow 模式：谁可以给机器人发邮件（必填）。receive 模式：机器人可以给谁发信。",
                        ),
                        kind: Kind::List,
                    },
                ],
            },
        ],
    },
    Group {
        title: tr("Tools and the web", "工具与网络"),
        sections: &[
            Section {
                title: tr("Tools", "工具"),
                fields: &[
                    Field {
                        path: &["tools", "shell"],
                        label: tr("Shell commands", "Shell 命令"),
                        help: tr(
                            "allow: run without asking; ask: a person approves each one; deny: no shell tool.",
                            "allow：直接运行；ask：每条都要人批准；deny：不提供 shell 工具。",
                        ),
                        kind: Kind::Choice(PERMISSION),
                    },
                    Field {
                        path: &["tools", "write"],
                        label: tr("File writes and edits", "写入和修改文件"),
                        help: tr(
                            "Reads are always allowed inside the workspace.",
                            "工作目录内的读取总是允许。",
                        ),
                        kind: Kind::Choice(PERMISSION),
                    },
                    Field {
                        path: &["tools", "identity"],
                        label: tr("Identity changes", "修改身份"),
                        help: tr(
                            "deny removes identity_set; otherwise every draft needs a person's approval.",
                            "deny 会去掉 identity_set；否则每份草稿都要人批准。",
                        ),
                        kind: Kind::Choice(PERMISSION),
                    },
                    Field {
                        path: &["tools", "mail"],
                        label: tr("Sending email", "发送邮件"),
                        help: tr(
                            "send_mail, when email is set up. Recipients also follow the email mode; deny removes the tool.",
                            "send_mail，需要先设置邮件。收件人还要符合邮件模式；deny 会去掉这个工具。",
                        ),
                        kind: Kind::Choice(PERMISSION),
                    },
                    Field {
                        path: &["tools", "workspace"],
                        label: tr("Workspace directory", "工作目录"),
                        help: tr(
                            "Where files and shell commands run. Reset (-) for <state dir>/workspace.",
                            "文件和 shell 命令所在目录。输入 - 恢复为 <状态目录>/workspace。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["tools", "review", "provider"],
                        label: tr("Shell command auto-review", "Shell 命令自动审查"),
                        help: tr(
                            "A model rates each `ask` command; see README \"Command auto-review\".",
                            "由模型给每条需要批准的命令打分；见 README 的 \"Command auto-review\"。",
                        ),
                        kind: Kind::Decider {
                            model: &["tools", "review", "model"],
                            base_url: &["tools", "review", "base_url"],
                        },
                    },
                    Field {
                        path: &["tools", "review", "model"],
                        label: tr("Its model id", "它的模型 id"),
                        help: tr(
                            "Reset (-) for the provider's default, shown on the line above.",
                            "输入 - 使用该服务的默认模型，即上一行显示的那个。",
                        ),
                        kind: Kind::Text,
                    },
                ],
            },
            Section {
                title: tr("Web search", "网页搜索"),
                fields: &[
                    Field {
                        path: &["search", "provider"],
                        label: tr("Provider", "搜索服务"),
                        help: tr(
                            "auto uses what the main model runs on: ChatGPT's own search, or OpenRouter's (billed per search). searxng: your own instance; off: no web search.",
                            "auto 跟随主模型：用 ChatGPT 自带的搜索，或 OpenRouter 的搜索（按次计费）。searxng：自己的实例；off：不搜索。",
                        ),
                        kind: Kind::Search,
                    },
                    Field {
                        path: &["search", "searxng_url"],
                        label: tr("SearXNG URL", "SearXNG 地址"),
                        help: tr(
                            "e.g. http://127.0.0.1:8888, with the JSON format enabled.",
                            "例如 http://127.0.0.1:8888，需要开启 JSON 格式。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["search", "model"],
                        label: tr("Search model", "搜索模型"),
                        help: tr(
                            "Model that runs OpenRouter searches. Reset (-) to use the main model.",
                            "执行 OpenRouter 搜索的模型。输入 - 恢复为主模型。",
                        ),
                        kind: Kind::Model(Need::Text),
                    },
                ],
            },
            Section {
                title: tr("Browser", "浏览器"),
                fields: &[
                    Field {
                        path: &["browser", "enabled"],
                        label: tr("Browser tool", "浏览器工具"),
                        help: tr(
                            "Uses a Chromium, Chrome, Edge or Brave already installed on this host; none is bundled. Without one there is no browser tool.",
                            "使用本机已安装的 Chromium、Chrome、Edge 或 Brave，不自带浏览器；本机没有时就不提供浏览器工具。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["browser", "executable"],
                        label: tr("Browser path", "浏览器路径"),
                        help: tr(
                            "Reset (-) to use the first of chromium, chromium-browser, google-chrome, microsoft-edge, brave found on PATH.",
                            "输入 - 恢复为自动查找 PATH 中的 chromium、chromium-browser、google-chrome、microsoft-edge、brave。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["browser", "cdp_url"],
                        label: tr("Running browser", "已运行的浏览器"),
                        help: tr(
                            "DevTools address of a browser started with --remote-debugging-port, e.g. http://127.0.0.1:9222; empty starts one when needed.",
                            "用 --remote-debugging-port 启动的浏览器的调试地址，例如 http://127.0.0.1:9222；留空则在需要时自动启动。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["browser", "headless"],
                        label: tr("Headless", "无窗口运行"),
                        help: tr(
                            "n shows a window, which needs a display.",
                            "选 n 会显示窗口，需要图形界面。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["browser", "idle_secs"],
                        label: tr("Close when idle (s)", "空闲关闭（秒）"),
                        help: tr(
                            "The browser is closed after this long without use, freeing its memory.",
                            "这么久不用就关闭浏览器，释放内存。",
                        ),
                        kind: Kind::Int,
                    },
                ],
            },
            Section {
                title: tr("Reading web pages", "读取网页"),
                fields: &[
                    Field {
                        path: &["fetch", "enabled"],
                        label: tr("web_fetch tool", "web_fetch 工具"),
                        help: tr(
                            "Downloads a page and reads its text without a browser: fast and light. Pages that need JavaScript still need the browser.",
                            "不用浏览器，直接下载网页读取文字，又快又省资源。需要 JavaScript 的页面仍然要用浏览器。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["fetch", "private_network"],
                        label: tr("Local network", "局域网"),
                        help: tr(
                            "y lets owners read pages on this host and the local network (router, NAS). Guests never can.",
                            "选 y 允许 owner 读取本机和局域网里的页面（路由器、NAS）。访客始终不行。",
                        ),
                        kind: Kind::Bool,
                    },
                ],
            },
            Section {
                title: tr("Skills", "Skills"),
                fields: &[Field {
                    path: &["skills", "enabled"],
                    label: tr("Skills", "Skills"),
                    help: tr(
                        "Task instructions in workspace/skills/<name>/SKILL.md that the agent loads when a request needs them. Manage them with openclaw-rs skills.",
                        "放在 workspace/skills/<名字>/SKILL.md 里的任务说明，需要时 agent 会自动加载。用 openclaw-rs skills 管理。",
                    ),
                    kind: Kind::Bool,
                }],
            },
        ],
    },
    Group {
        title: tr("Access and spending", "权限与花费"),
        sections: &[
            Section {
                title: tr("Access", "权限"),
                fields: &[
                    Field {
                        path: &["access", "owners"],
                        label: tr("Owners", "Owner"),
                        help: tr(
                            "Comma-separated qq:<openid>, feishu:<open_id>, zulip:<email> or mail:<address> treated like the terminal: every tool, /compact, identity approval.",
                            "用逗号分隔的 qq:<openid>、feishu:<open_id>、zulip:<邮箱> 或 mail:<地址>，和终端同等权限：所有工具、/compact、批准身份。",
                        ),
                        kind: Kind::List,
                    },
                    Field {
                        path: &["access", "first_chat_owner"],
                        label: tr("First chat becomes owner", "第一个私聊的人成为所有者"),
                        help: tr(
                            "On QQ, Feishu or Zulip with no owner yet, the first person to write to the bot in a private chat becomes its owner and is added to the list above. Turn it on, write to the bot yourself, then turn it off again: until then, anyone who reaches the bot first gets every tool.",
                            "QQ、飞书或 Zulip 上还没有所有者时，第一个私聊机器人的人会成为该平台的所有者，并加进上面的列表。建议打开后自己先给机器人发消息，然后再关掉：在那之前，谁先找到机器人谁就拿到所有工具。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["access", "owners"],
                        label: tr("Revoke a chat owner", "撤销聊天平台的所有者"),
                        help: tr(
                            "Removes the owner you pick (QQ, Feishu or Zulip). With \"first chat becomes owner\" on, the next person to write privately on that app becomes its owner.",
                            "移除你选中的所有者（QQ、飞书或 Zulip）。如果开着“第一个私聊的人成为所有者”，之后第一个私聊该平台机器人的人会成为新的所有者。",
                        ),
                        kind: Kind::Revoke,
                    },
                ],
            },
            Section {
                title: tr("Spending limits", "花费上限"),
                fields: &[
                    Field {
                        path: &["limits", "daily_usd"],
                        label: tr(
                            "Daily limit, whole agent (USD)",
                            "整个 agent 每日上限（美元）",
                        ),
                        help: tr(
                            "Model calls stop for everyone, owners included, once this much was spent today. 0: no limit.",
                            "今天花到这个数后，所有人（包括 owner）的模型调用都会停止。0 表示不限。",
                        ),
                        kind: Kind::Money,
                    },
                    Field {
                        path: &["limits", "monthly_usd"],
                        label: tr(
                            "Monthly limit, whole agent (USD)",
                            "整个 agent 每月上限（美元）",
                        ),
                        help: tr(
                            "The same per calendar month. 0: no limit.",
                            "同上，按自然月计算。0 表示不限。",
                        ),
                        kind: Kind::Money,
                    },
                    Field {
                        path: &["limits", "guest_daily_usd"],
                        label: tr("Daily limit per guest (USD)", "每个访客每日上限（美元）"),
                        help: tr(
                            "What each QQ or email sender who is not an owner may spend a day. 0: no limit.",
                            "每个不是 owner 的 QQ 或邮件发送者每天最多能花多少。0 表示不限。",
                        ),
                        kind: Kind::Money,
                    },
                    Field {
                        path: &["limits", "turn_usd"],
                        label: tr("Limit per turn (USD)", "每轮上限（美元）"),
                        help: tr(
                            "Stops one reply that keeps calling tools once it cost this much. 0: no limit.",
                            "一次回复不停调用工具、花到这个数时就停下。0 表示不限。",
                        ),
                        kind: Kind::Money,
                    },
                ],
            },
        ],
    },
    Group {
        title: tr("Companion", "陪伴"),
        sections: &[Section {
            title: tr(
                "Companion: time, heartbeat, inner life",
                "陪伴：时间感、心跳、内心生活",
            ),
            fields: &[
                Field {
                    path: &["agent", "time_sense"],
                    label: tr("Time sense", "时间感"),
                    help: tr(
                        "Each message tells the model the local time, and how long ago you last wrote when it has been a while.",
                        "每条消息都告诉模型当前本地时间，隔了一段时间再聊时还会告诉它你上次说话是多久以前。",
                    ),
                    kind: Kind::Bool,
                },
                Field {
                    path: &["heartbeat", "enabled"],
                    label: tr("Heartbeat: reach out on its own", "心跳：主动找你"),
                    help: tr(
                        "Every so often it may message you on its own when it has something worth saying. Needs the Gateway (service) running.",
                        "每隔一段时间，如果它觉得有值得说的话，会主动给你发消息。需要 Gateway（后台服务）在运行。",
                    ),
                    kind: Kind::Bool,
                },
                Field {
                    path: &["heartbeat", "every_minutes"],
                    label: tr("Heartbeat interval (minutes)", "心跳间隔（分钟）"),
                    help: tr(
                        "How often it considers reaching out.",
                        "多久考虑一次要不要主动找你。",
                    ),
                    kind: Kind::Int,
                },
                Field {
                    path: &["heartbeat", "quiet_hours"],
                    label: tr("Quiet hours", "免打扰时段"),
                    help: tr(
                        "Local time HH:MM-HH:MM with no heartbeat; may cross midnight. Empty: none.",
                        "本地时间 HH:MM-HH:MM，期间不主动找你，可以跨午夜。留空表示没有。",
                    ),
                    kind: Kind::Text,
                },
                Field {
                    path: &["heartbeat", "max_per_day"],
                    label: tr("Messages it starts per day", "每天最多主动消息数"),
                    help: tr(
                        "At most this many a day; one more when you answered the last one quickly.",
                        "一天里最多主动发几条；你很快回复了上一条时，当天可以多一条。",
                    ),
                    kind: Kind::Int,
                },
                Field {
                    path: &["heartbeat", "prompt"],
                    label: tr("What to keep an eye on", "让它多留意什么"),
                    help: tr(
                        "Optional words added to each heartbeat, e.g. \"Ask how my running is going now and then.\"",
                        "可选，每次心跳都会带上的话，例如“偶尔问问我跑步的情况”。",
                    ),
                    kind: Kind::Text,
                },
                Field {
                    path: &["life", "enabled"],
                    label: tr("Inner life", "内心生活"),
                    help: tr(
                        "A nightly diary, mood and notes about you, carried into your conversations, plus good-morning and good-night messages. Needs the Gateway running.",
                        "每晚写日记、记下心情和关于你的事，并带进你们的对话；还有早安和晚安消息。需要 Gateway 在运行。",
                    ),
                    kind: Kind::Bool,
                },
                Field {
                    path: &["life", "reflect_at"],
                    label: tr("Nightly reflection at", "每晚反思时间"),
                    help: tr("Local time HH:MM.", "本地时间 HH:MM。"),
                    kind: Kind::Text,
                },
                Field {
                    path: &["life", "morning"],
                    label: tr("Good-morning at", "早安时间"),
                    help: tr(
                        "Local time HH:MM; empty for none.",
                        "本地时间 HH:MM；留空表示不发。",
                    ),
                    kind: Kind::Text,
                },
                Field {
                    path: &["life", "evening"],
                    label: tr("Good-night at", "晚安时间"),
                    help: tr(
                        "Local time HH:MM; empty for none.",
                        "本地时间 HH:MM；留空表示不发。",
                    ),
                    kind: Kind::Text,
                },
                Field {
                    path: &["life", "city"],
                    label: tr("City for the weather", "天气所在城市"),
                    help: tr(
                        "Named in the good-morning message; it looks the weather up with its web tools.",
                        "早安消息里会提到这里的天气，由它用网页工具去查。",
                    ),
                    kind: Kind::Text,
                },
                Field {
                    path: &["life", "dates"],
                    label: tr("Days that matter", "重要的日子"),
                    help: tr(
                        "Comma-separated MM-DD and a name, e.g. \"03-14 my birthday, 05-20 our anniversary\".",
                        "用逗号分隔，每项是 月-日 加名字，例如“03-14 我的生日, 05-20 纪念日”。",
                    ),
                    kind: Kind::Dates,
                },
                Field {
                    path: &["heartbeat", "session"],
                    label: tr("Where it reaches out", "主动消息发到哪里"),
                    help: tr(
                        "A session such as qq:c2c:<openid>, feishu:dm:<open_id> or zulip:dm:<email>. Empty: your private chat with the first qq:, feishu: or zulip: owner under Access, else the local main session.",
                        "会话名，例如 qq:c2c:<openid>、feishu:dm:<open_id> 或 zulip:dm:<邮箱>。留空：发到权限里第一个 qq:、feishu: 或 zulip: owner 的私聊，没有则发到本地 main 会话。",
                    ),
                    kind: Kind::Text,
                },
                Field {
                    path: &["life", "session"],
                    label: tr("Where morning and evening go", "早安晚安发到哪里"),
                    help: tr(
                        "As above; empty for the same default.",
                        "同上；留空使用同样的默认值。",
                    ),
                    kind: Kind::Text,
                },
            ],
        }],
    },
    Group {
        title: tr("Gateway and updates", "Gateway 与更新"),
        sections: &[
            Section {
                title: tr("Gateway and Web UI", "Gateway 与 Web UI"),
                fields: &[
                    Field {
                        path: &["gateway", "bind"],
                        label: tr("Listen address", "监听地址"),
                        help: tr(
                            "e.g. 127.0.0.1:18789, or 0.0.0.0:18789 for the LAN (needs a token).",
                            "例如 127.0.0.1:18789；局域网访问用 0.0.0.0:18789（需要 token）。",
                        ),
                        kind: Kind::Text,
                    },
                    Field {
                        path: &["gateway", "token"],
                        label: tr("Access token", "访问 token"),
                        help: tr(
                            "Required unless bound to loopback; OPENCLAW_RS_TOKEN wins when set. Type + to generate one.",
                            "不是只监听本机时必须设置；设了 OPENCLAW_RS_TOKEN 时以环境变量为准。输入 + 自动生成。",
                        ),
                        kind: Kind::Secret {
                            env: "OPENCLAW_RS_TOKEN",
                            generate: true,
                        },
                    },
                    Field {
                        path: &["gateway", "web"],
                        label: tr("Web UI", "Web UI"),
                        help: tr(
                            "ui: Web UI and WebSocket API; api: only the API (for other clients); off: no HTTP at all, QQ, email and scheduled jobs only. Chat in the terminal with openclaw-rs tui.",
                            "ui：Web UI 和 WebSocket API；api：只有 API（给其他客户端用）；off：完全不开 HTTP，只运行 QQ、邮件和定时任务。在终端里聊天用 openclaw-rs tui。",
                        ),
                        kind: Kind::Choice(&["ui", "api", "off"]),
                    },
                ],
            },
            Section {
                title: tr("Updates", "更新"),
                fields: &[
                    Field {
                        path: &["update", "check"],
                        label: tr("Look for new versions", "检查新版本"),
                        help: tr(
                            "The Gateway checks GitHub Releases once a day and logs a new version.",
                            "Gateway 每天检查一次 GitHub Releases，发现新版本就写进日志。",
                        ),
                        kind: Kind::Bool,
                    },
                    Field {
                        path: &["update", "auto"],
                        label: tr("Install updates on their own", "自动安装更新"),
                        help: tr(
                            "The Gateway installs a new version (checksum checked, old binary kept) and restarts when no turn is running. The service account must be able to write the binary.",
                            "Gateway 自动安装新版本（会校验 checksum，保留旧版本），并在没有对话进行时重启。服务账号需要能写入程序文件。",
                        ),
                        kind: Kind::Bool,
                    },
                ],
            },
        ],
    },
];

/// Terminal I/O, abstracted so the editor can be driven by tests.
pub struct Term<R, W> {
    pub(crate) input: R,
    pub(crate) out: W,
    /// Turn echo off while a secret is typed; only for a real terminal.
    pub(crate) hide_secrets: bool,
    /// Menus are picked with the arrow keys instead of typed numbers.
    pub(crate) arrows: bool,
    /// Input and output are the process's terminal, switched to raw mode
    /// while a menu is open; otherwise keys are read from `input` (tests).
    pub(crate) raw: bool,
    pub(crate) lang: Lang,
}

/// What a menu ended with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Picked {
    Item(usize),
    /// Esc or q: go back without picking.
    Back,
    /// End of input or Ctrl-C.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Key {
    Up,
    Down,
    Home,
    End,
    Enter,
    Back,
    Digit(usize),
    Other,
    Eof,
}

const KEYS_HINT: Tr = tr(
    "↑/↓ move · Enter picks · Esc goes back",
    "↑/↓ 移动 · 回车选择 · Esc 返回",
);
const YES: Tr = tr("Yes", "是");
const NO: Tr = tr("No", "否");
const BACK: Tr = tr("← Back", "← 返回");
const RESET: Tr = tr("↺ Default", "↺ 恢复默认");
const UNSAVED: Tr = tr("There are unsaved changes.", "有未保存的改动。");
const KEEP_EDITING: Tr = tr("Keep editing", "继续编辑");
const TYPE_HOW: Tr = tr(
    "Enter keeps the value, - resets it to the default.",
    "回车保持不变，- 恢复默认值。",
);

impl<R: BufRead, W: Write> Term<R, W> {
    /// One trimmed line, or `None` at end of input.
    pub(crate) fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim().to_owned()))
    }

    pub(crate) fn secret(&mut self, prompt: &str) -> Result<Option<String>> {
        if !self.hide_secrets {
            return self.ask(prompt);
        }
        let _echo = EchoOff::new();
        let line = self.ask(prompt);
        writeln!(self.out)?;
        line
    }

    pub(crate) fn say(&mut self, text: &str) -> Result<()> {
        writeln!(self.out, "{text}")?;
        Ok(())
    }

    pub(crate) fn tell(&mut self, text: Tr, args: &[&str]) -> Result<()> {
        let line = text.fill(self.lang, args);
        self.say(&line)
    }

    /// One byte of input. On the raw terminal, `follow_up` waits only
    /// briefly, to tell a lone Esc from the start of an arrow key.
    fn byte(&mut self, follow_up: bool) -> Result<Option<u8>> {
        if !self.raw {
            let buf = self.input.fill_buf()?;
            let Some(&b) = buf.first() else {
                return Ok(None);
            };
            self.input.consume(1);
            return Ok(Some(b));
        }
        if follow_up {
            let mut fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut fd, 1, 50) } <= 0 {
                return Ok(None);
            }
        }
        let mut b = 0u8;
        // SAFETY: reads at most one byte into `b`.
        let n = unsafe { libc::read(libc::STDIN_FILENO, (&raw mut b).cast(), 1) };
        Ok((n == 1).then_some(b))
    }

    fn key(&mut self) -> Result<Key> {
        let Some(b) = self.byte(false)? else {
            return Ok(Key::Eof);
        };
        Ok(match b {
            b'\r' | b'\n' => Key::Enter,
            // Ctrl-C, Ctrl-D: raw mode turns them into bytes.
            0x03 | 0x04 => Key::Eof,
            b'k' => Key::Up,
            b'j' => Key::Down,
            b'q' => Key::Back,
            b'1'..=b'9' => Key::Digit(usize::from(b - b'0')),
            0x1b => match self.byte(true)? {
                Some(b'[' | b'O') => match self.byte(true)? {
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    Some(b'H') => Key::Home,
                    Some(b'F') => Key::End,
                    // Home and End as ESC [ 1 ~ and ESC [ 4 ~.
                    Some(n @ (b'1' | b'4' | b'7' | b'8')) => {
                        self.byte(true)?;
                        if matches!(n, b'1' | b'7') {
                            Key::Home
                        } else {
                            Key::End
                        }
                    }
                    _ => Key::Other,
                },
                _ => Key::Back,
            },
            _ => Key::Other,
        })
    }

    /// A menu: ↑/↓ (or j/k, or a digit) move, Enter picks, Esc goes back.
    /// `help` gives a line shown under the highlighted item. The menu is
    /// erased when it ends, leaving `title › item` for a pick.
    pub(crate) fn select(
        &mut self,
        title: &str,
        items: &[String],
        start: usize,
        help: &dyn Fn(usize) -> Option<String>,
    ) -> Result<Picked> {
        if items.is_empty() {
            return Ok(Picked::Back);
        }
        let _raw = self.raw.then(RawMode::new);
        let (cols, rows) = if self.raw { terminal_size() } else { (80, 24) };
        let width = cols.saturating_sub(1).max(20);
        let height = items.len().min(rows.saturating_sub(6).max(5));
        let mut cur = start.min(items.len() - 1);
        let mut top = 0;
        let mut drawn = 0;
        write!(self.out, "\x1b[?25l")?;
        let picked = loop {
            top = top.min(cur).max((cur + 1).saturating_sub(height));
            let mut lines: Vec<String> = wrap(title, width)
                .into_iter()
                .map(|l| format!("\x1b[1m{l}\x1b[0m"))
                .collect();
            for (i, item) in items.iter().enumerate().skip(top).take(height) {
                let text = truncate(item, width.saturating_sub(4));
                lines.push(if i == cur {
                    format!("\x1b[36;1m❯ {text}\x1b[0m")
                } else {
                    format!("  {text}")
                });
            }
            if items.len() > height {
                lines.push(format!("\x1b[2m  ({}/{})\x1b[0m", cur + 1, items.len()));
            }
            if let Some(text) = help(cur) {
                for line in wrap(&text, width.saturating_sub(2)) {
                    lines.push(format!("\x1b[2m  {line}\x1b[0m"));
                }
            }
            lines.push(format!("\x1b[2m{}\x1b[0m", KEYS_HINT.get(self.lang)));
            self.erase(drawn)?;
            for line in &lines {
                write!(self.out, "{line}\r\n")?;
            }
            self.out.flush()?;
            drawn = lines.len();
            match self.key()? {
                Key::Up => cur = cur.checked_sub(1).unwrap_or(items.len() - 1),
                Key::Down => cur = (cur + 1) % items.len(),
                Key::Home => cur = 0,
                Key::End => cur = items.len() - 1,
                Key::Digit(n) if n <= items.len() => cur = n - 1,
                Key::Enter => break Picked::Item(cur),
                Key::Back => break Picked::Back,
                Key::Eof => break Picked::End,
                Key::Digit(_) | Key::Other => {}
            }
        };
        self.erase(drawn)?;
        if let Picked::Item(i) = picked {
            let line = truncate(&format!("{title} › {}", items[i]), width);
            write!(self.out, "{line}\r\n")?;
        }
        write!(self.out, "\x1b[?25h")?;
        self.out.flush()?;
        Ok(picked)
    }

    /// Clears the `lines` just written above the cursor.
    fn erase(&mut self, lines: usize) -> Result<()> {
        if lines > 0 {
            write!(self.out, "\x1b[{lines}A\r\x1b[J")?;
        }
        Ok(())
    }

    /// A yes/no question. `question` may end in a typed hint such as
    /// "[Y/n]: ", which the menu leaves out. `None` when input ended.
    pub(crate) fn confirm(&mut self, question: &str, default_yes: bool) -> Result<Option<bool>> {
        if !self.arrows {
            let Some(answer) = self.ask(question)? else {
                return Ok(None);
            };
            let answer = answer.to_lowercase();
            return Ok(Some(if default_yes {
                !matches!(answer.as_str(), "n" | "no" | "否" | "不")
            } else {
                matches!(answer.as_str(), "y" | "yes" | "是" | "要")
            }));
        }
        let title = question
            .rsplit_once('[')
            .map_or(question, |(q, _)| q)
            .trim_end();
        let items = [YES.get(self.lang).to_owned(), NO.get(self.lang).to_owned()];
        Ok(
            match self.select(title, &items, usize::from(!default_yes), &|_| None)? {
                Picked::Item(i) => Some(i == 0),
                Picked::Back => Some(false),
                Picked::End => None,
            },
        )
    }
}

/// Raw input (no line editing, echo or signals) on stdin until dropped.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn new() -> Self {
        // SAFETY: termios is plain data; tcgetattr fills it or fails.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Self(None);
            }
            let saved = term;
            term.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            term.c_cc[libc::VMIN] = 1;
            term.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Self(Some(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

/// Columns and rows of the terminal on stdout.
fn terminal_size() -> (usize, usize) {
    // SAFETY: winsize is plain data; the ioctl fills it or fails.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
            (usize::from(size.ws_col), usize::from(size.ws_row.max(10)))
        } else {
            (80, 24)
        }
    }
}

/// Columns a character takes: 2 for CJK and emoji, else 1.
fn char_width(c: char) -> usize {
    match u32::from(c) {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        0xFE00..=0xFE0F | 0x200B..=0x200F => 0,
        _ => 1,
    }
}

pub(crate) fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// `text` cut to `width` columns, ending in … when cut.
fn truncate(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// `text` broken into lines of at most `width` columns, at spaces when it can.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for c in paragraph.chars() {
            let w = char_width(c);
            if used + w > width {
                // Back to the last space, when the line has one.
                match line.rfind(' ').filter(|&i| i > 0) {
                    Some(i) => {
                        let rest = line.split_off(i + 1);
                        lines.push(line.trim_end().to_owned());
                        line = rest;
                    }
                    None => lines.push(std::mem::take(&mut line)),
                }
                used = text_width(&line);
            }
            line.push(c);
            used += w;
        }
        lines.push(line);
    }
    lines
}

/// Disables terminal echo on stdin until dropped.
struct EchoOff(Option<libc::termios>);

impl EchoOff {
    fn new() -> Self {
        // SAFETY: termios is plain data; tcgetattr fills it or fails.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Self(None);
            }
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Self(Some(saved))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

/// A terminal to talk on, for steps done outside this module (signing in).
pub trait Prompt {
    fn say(&mut self, text: &str) -> Result<()>;
    /// One trimmed line, or `None` at end of input.
    fn ask(&mut self, prompt: &str) -> Result<Option<String>>;
    fn lang(&self) -> Lang;
}

impl<R: BufRead, W: Write> Prompt for Term<R, W> {
    fn say(&mut self, text: &str) -> Result<()> {
        Term::say(self, text)?;
        self.out.flush()?;
        Ok(())
    }

    fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        Term::ask(self, prompt)
    }

    fn lang(&self) -> Lang {
        self.lang
    }
}

/// Signs in to ChatGPT on a terminal: who signed in, or `None` when input ended.
/// `true` signs in with a device code, `false` in a browser.
pub type SignIn<'a> = Box<dyn FnMut(&mut dyn Prompt, bool) -> Result<Option<String>> + 'a>;

/// Signing in for real, saved in the state directory.
pub fn chatgpt_sign_in() -> (Option<String>, SignIn<'static>) {
    match crate::config::state_dir() {
        Ok(state) => (
            crate::chatgpt::signed_in(&state),
            Box::new(move |term: &mut dyn Prompt, device| {
                crate::chatgpt::sign_in(term, &state, device)
            }),
        ),
        Err(err) => {
            let why = format!("{err:#}");
            (None, Box::new(move |_: &mut dyn Prompt, _| bail!("{why}")))
        }
    }
}

/// The process's own terminal.
pub fn terminal(lang: Lang) -> Term<std::io::StdinLock<'static>, std::io::Stdout> {
    // SAFETY: isatty has no preconditions.
    let hide = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    // SAFETY: as above.
    let tty = hide && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    Term {
        input: std::io::stdin().lock(),
        out: std::io::stdout(),
        hide_secrets: hide,
        arrows: tty,
        raw: tty,
        lang,
    }
}

/// Runs the editor on `path` with the process's terminal; model fields
/// offer OpenRouter's list, fetched the first time one is edited. A saved
/// change restarts a running Gateway so it applies at once. `None` when
/// nothing was saved, else whether the Gateway was restarted.
pub fn run(path: &Path, lang: Lang) -> Result<Option<bool>> {
    let mut term = terminal(lang);
    if !run_on(path, &mut term)? {
        return Ok(None);
    }
    term.out.flush()?;
    let restarted = crate::service::restart_if_running();
    term.tell(if restarted { RESTARTED } else { RESTART }, &[])?;
    Ok(Some(restarted))
}

/// The editor itself on `term`; `true` when it saved.
fn run_on<R: BufRead, W: Write>(path: &Path, term: &mut Term<R, W>) -> Result<bool> {
    let base_url = Config::load(path).unwrap_or_default().model.base_url;
    let mut models = Models::new(move || {
        let catalog = crate::onboard::OpenRouter::new(&base_url)?;
        // The editor runs on a runtime worker; let it wait for the request.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(crate::onboard::Catalog::models(&catalog))
        })
    });
    models.chatgpt = Some(chatgpt_models(path));
    let (signed_in, sign_in) = chatgpt_sign_in();
    models.signed_in = signed_in;
    models.sign_in = Some(sign_in);
    edit_with(path, term, &mut models)
}

/// Loads a model list.
pub type Loader<'a> = Box<dyn FnMut() -> Result<Vec<ModelInfo>> + 'a>;

/// ChatGPT's model list, read with the saved sign-in.
pub fn chatgpt_models(path: &Path) -> Loader<'static> {
    let config = Config::load(path).unwrap_or_default();
    Box::new(move || {
        let state = crate::config::state_dir()?;
        // The editor runs on a runtime worker; let it wait for the request.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(crate::chatgpt::model_list(&config, &state))
        })
    })
}

/// OpenRouter's model list for the model fields, loaded once, when first
/// needed; ChatGPT's, once read; and the ChatGPT sign-in.
#[derive(Default)]
pub struct Models<'a> {
    load: Option<Loader<'a>>,
    list: Option<Vec<ModelInfo>>,
    /// Reads the models the ChatGPT plan offers.
    pub chatgpt: Option<Loader<'a>>,
    chatgpt_list: Option<Vec<ModelInfo>>,
    /// Signs in to ChatGPT; without it, the sign-in field cannot be used.
    pub sign_in: Option<SignIn<'a>>,
    /// Who is signed in to ChatGPT now.
    pub signed_in: Option<String>,
}

impl<'a> Models<'a> {
    pub fn new(load: impl FnMut() -> Result<Vec<ModelInfo>> + 'a) -> Self {
        Self {
            load: Some(Box::new(load)),
            ..Self::default()
        }
    }

    /// The list; empty when there is none, which model fields take as
    /// "type the id".
    fn get<R: BufRead, W: Write>(&mut self, term: &mut Term<R, W>) -> Result<&[ModelInfo]> {
        if self.list.is_none() {
            let list = match self.load.as_mut() {
                None => Vec::new(),
                Some(load) => {
                    term.tell(LOADING_MODELS, &[])?;
                    load().or_else(|err| {
                        term.tell(NO_MODEL_LIST, &[&format!("{err:#}")])?;
                        anyhow::Ok(Vec::new())
                    })?
                }
            };
            self.list = Some(list);
        }
        Ok(self.list.as_deref().unwrap_or_default())
    }

    /// ChatGPT's list; empty when it cannot be read (not kept, so it is
    /// read again, e.g. after signing in).
    fn chatgpt<R: BufRead, W: Write>(&mut self, term: &mut Term<R, W>) -> Result<&[ModelInfo]> {
        if self.chatgpt_list.is_none()
            && let Some(load) = self.chatgpt.as_mut()
        {
            term.tell(LOADING_MODELS, &[])?;
            match load() {
                Ok(list) => self.chatgpt_list = Some(list),
                Err(err) => term.tell(NO_MODEL_LIST, &[&format!("{err:#}")])?,
            }
        }
        Ok(self.chatgpt_list.as_deref().unwrap_or_default())
    }

    /// Every model this setup can run: ChatGPT's (when signed in) first,
    /// then OpenRouter's.
    fn all<R: BufRead, W: Write>(
        &mut self,
        term: &mut Term<R, W>,
        config: &Config,
    ) -> Result<Vec<ModelInfo>> {
        let mut list = if config.chatgpt() || self.signed_in.is_some() {
            self.chatgpt(term)?.to_vec()
        } else {
            Vec::new()
        };
        list.extend(self.get(term)?.iter().cloned());
        Ok(list)
    }

    /// The thinking levels the main model's list gives it; for a ChatGPT
    /// model without any, OpenRouter's entry for it. Empty when unknown.
    fn efforts<R: BufRead, W: Write>(
        &mut self,
        term: &mut Term<R, W>,
        config: &Config,
    ) -> Result<Vec<String>> {
        let find = |list: &[ModelInfo], id: &str| {
            let base = id.split(':').next().unwrap_or(id);
            list.iter()
                .find(|m| m.id.eq_ignore_ascii_case(id) || m.id.eq_ignore_ascii_case(base))
                .map(|m| m.efforts.clone())
                .unwrap_or_default()
        };
        let mut levels = find(&self.all(term, config)?, config.model.model.trim());
        if levels.is_empty() && config.chatgpt() {
            levels = find(self.get(term)?, config.openrouter_model().model.trim());
        }
        Ok(levels)
    }
}

pub(crate) fn load(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("{} is not valid TOML; fix it by hand first", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub(crate) fn parse(doc: &DocumentMut) -> Result<Config> {
    Ok(toml::from_str(&doc.to_string())?)
}

/// The menu loop: edits `path` and saves it when asked. Model ids are typed.
#[cfg(test)]
pub fn edit<R: BufRead, W: Write>(path: &Path, term: &mut Term<R, W>) -> Result<bool> {
    edit_with(path, term, &mut Models::default())
}

/// The menu loop, with `models` for the model fields; `true` when it saved.
pub fn edit_with<R: BufRead, W: Write>(
    path: &Path,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    let mut doc = load(path)?;
    if let Err(err) = parse(&doc) {
        term.tell(BROKEN, &[&format!("{err:#}")])?;
    }
    let original = doc.to_string();
    if term.arrows {
        return edit_with_arrows(path, doc, &original, term, models);
    }
    term.tell(EDITING, &[&path.display().to_string()])?;
    term.tell(HOW, &[])?;
    loop {
        term.say("")?;
        for (i, group) in GROUPS.iter().enumerate() {
            term.say(&format!("  {}) {}", i + 1, group.title.get(term.lang)))?;
        }
        term.say(&format!("  s) {}", SAVE.get(term.lang)))?;
        term.say(&format!("  q) {}", QUIT.get(term.lang)))?;
        let Some(choice) = term.ask("> ")? else {
            return finish(path, &doc, &original, term, false);
        };
        match choice.as_str() {
            "s" | "S" => return finish(path, &doc, &original, term, true),
            "q" | "Q" => return finish(path, &doc, &original, term, false),
            other => match other
                .parse::<usize>()
                .ok()
                .and_then(|n| GROUPS.get(n.wrapping_sub(1)))
            {
                Some(group) => {
                    if !edit_group(&mut doc, group, term, models)? {
                        return finish(path, &doc, &original, term, false);
                    }
                }
                None => term.tell(PICK_MENU, &[])?,
            },
        }
    }
}

/// Picks one of a group's sections by number and walks it; a group of one
/// section goes straight to it. `false` when input ended.
fn edit_group<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    group: &Group,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    if let [section] = group.sections {
        return edit_section(doc, section, term, models);
    }
    term.say(&format!("\n[{}]", group.title.get(term.lang)))?;
    for (i, section) in group.sections.iter().enumerate() {
        term.say(&format!("  {}) {}", i + 1, section.title.get(term.lang)))?;
    }
    loop {
        let Some(choice) = term.ask(BACK_PROMPT.get(term.lang))? else {
            return Ok(false);
        };
        if choice.is_empty() {
            return Ok(true);
        }
        match choice
            .parse::<usize>()
            .ok()
            .and_then(|n| group.sections.get(n.wrapping_sub(1)))
        {
            Some(section) => return edit_section(doc, section, term, models),
            None => term.tell(PICK_MENU, &[])?,
        }
    }
}

/// The menus picked with the arrow keys: groups, sections, then fields.
fn edit_with_arrows<R: BufRead, W: Write>(
    path: &Path,
    mut doc: DocumentMut,
    original: &str,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    let title = EDITING.fill(term.lang, &[&path.display().to_string()]);
    let mut cur = 0;
    loop {
        let mut items: Vec<String> = GROUPS
            .iter()
            .map(|g| g.title.get(term.lang).to_owned())
            .collect();
        items.push(SAVE.get(term.lang).into());
        items.push(QUIT.get(term.lang).into());
        let changed = doc.to_string() != original;
        let shown = if changed {
            format!("{title} · {}", UNSAVED.get(term.lang))
        } else {
            title.clone()
        };
        match term.select(&shown, &items, cur, &|_| None)? {
            Picked::Item(i) if i < GROUPS.len() => {
                cur = i;
                if !group_with_arrows(&mut doc, &GROUPS[i], term, models)? {
                    return finish(path, &doc, original, term, false);
                }
            }
            Picked::Item(i) => {
                return finish(path, &doc, original, term, i == GROUPS.len());
            }
            Picked::Back if changed => {
                let choices = [
                    SAVE.get(term.lang).to_owned(),
                    QUIT.get(term.lang).to_owned(),
                    KEEP_EDITING.get(term.lang).to_owned(),
                ];
                match term.select(UNSAVED.get(term.lang), &choices, 0, &|_| None)? {
                    Picked::Item(0) => return finish(path, &doc, original, term, true),
                    Picked::Item(1) | Picked::End => {
                        return finish(path, &doc, original, term, false);
                    }
                    _ => {}
                }
            }
            Picked::Back | Picked::End => return finish(path, &doc, original, term, false),
        }
    }
}

/// A group's sections; Enter opens one. A group of one section opens it
/// at once. `false` when input ended.
fn group_with_arrows<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    group: &'static Group,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    let lang = term.lang;
    let title = group.title.get(lang);
    if let [section] = group.sections {
        return section_with_arrows(doc, section, &format!("[{title}]"), term, models);
    }
    let mut cur = 0;
    loop {
        let mut items: Vec<String> = group
            .sections
            .iter()
            .map(|s| s.title.get(lang).to_owned())
            .collect();
        items.push(BACK.get(lang).into());
        match term.select(&format!("[{title}]"), &items, cur, &|_| None)? {
            Picked::Item(i) if i < group.sections.len() => {
                cur = i;
                let section = &group.sections[i];
                let trail = format!("[{title} › {}]", section.title.get(lang));
                if !section_with_arrows(doc, section, &trail, term, models)? {
                    return Ok(false);
                }
            }
            Picked::Item(_) | Picked::Back => return Ok(true),
            Picked::End => return Ok(false),
        }
    }
}

/// A section's fields with their values; Enter edits one. `false` when
/// input ended.
fn section_with_arrows<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    section: &'static Section,
    title: &str,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    let lang = term.lang;
    let label_width = section
        .fields
        .iter()
        .map(|f| text_width(f.label.get(lang)))
        .max()
        .unwrap_or(0);
    let mut cur = 0;
    loop {
        let config = parse(doc).unwrap_or_default();
        let fields: Vec<&Field> = section
            .fields
            .iter()
            .filter(|f| relevant(f, &config))
            .collect();
        let mut items: Vec<String> = fields
            .iter()
            .map(|f| {
                let label = f.label.get(lang);
                let pad = " ".repeat(label_width - text_width(label));
                format!("{label}{pad}  {}", shown(doc, f, lang, models))
            })
            .collect();
        items.push(BACK.get(lang).into());
        let help = |i: usize| fields.get(i).map(|f| f.help.get(lang).to_owned());
        match term.select(title, &items, cur, &help)? {
            Picked::Item(i) if i < fields.len() => {
                if !field_with_arrows(doc, fields[i], term, models)? {
                    return Ok(false);
                }
                cur = i;
            }
            Picked::Item(_) | Picked::Back => return Ok(true),
            Picked::End => return Ok(false),
        }
    }
}

/// Edits one field: a menu for choices and yes/no, typed text otherwise.
fn field_with_arrows<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    field: &Field,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    let lang = term.lang;
    let label = field.label.get(lang);
    if let Kind::Revoke = field.kind {
        return revoke_owner(doc, term);
    }
    if let Kind::SignIn = field.kind {
        let title = match &models.signed_in {
            Some(who) => format!("{label} [{who}]"),
            None => format!("{label} [{}]", NOT_SIGNED_IN.get(lang)),
        };
        let items = [
            SIGN_IN_BROWSER.get(lang).to_owned(),
            SIGN_IN_DEVICE.get(lang).to_owned(),
        ];
        let help = |_| Some(field.help.get(lang).to_owned());
        return match term.select(&title, &items, 0, &help)? {
            Picked::Item(i) => sign_in(doc, term, models, i == 1),
            Picked::Back => Ok(true),
            Picked::End => Ok(false),
        };
    }
    let current = describe(doc, field, lang);
    if let Kind::Model(need) = field.kind {
        // OpenRouter's search runs on OpenRouter; the rest may run on either.
        let list = if field.path == ["search", "model"] {
            models.get(term)?.to_vec()
        } else {
            models.all(term, &parse(doc).unwrap_or_default())?
        };
        if !list.is_empty() {
            let items = [
                SEARCH_MODELS.get(lang).to_owned(),
                RESET.get(lang).to_owned(),
            ];
            let help = |_| Some(field.help.get(lang).to_owned());
            match term.select(&format!("{label} [{current}]"), &items, 0, &help)? {
                Picked::Item(0) => {
                    let Some(id) = crate::onboard::pick(term, &list, need)? else {
                        return Ok(false);
                    };
                    if field.path == ["model", "model"] {
                        set_provider_for(doc, &id);
                    }
                    change(doc, field.path, Some(Value::from(id)), term)?;
                }
                Picked::Item(_) => {
                    change(doc, field.path, None, term)?;
                }
                Picked::Back => {}
                Picked::End => return Ok(false),
            }
            return Ok(true);
        }
    }
    if let Kind::Decider { model, base_url } = field.kind {
        let config = parse(doc).unwrap_or_default();
        let now = describe_path(doc, field.path);
        let items: Vec<String> = DECIDERS
            .iter()
            .map(|p| {
                // Another provider's own model would not fit this one.
                let same = now.as_deref() == Some(*p);
                let model = same.then(|| describe_path(doc, model)).flatten();
                let base_url = describe_path(doc, base_url);
                match decider_label(
                    p,
                    model.as_deref(),
                    base_url.as_deref(),
                    &config.model.model,
                ) {
                    Some(label) if label.starts_with(p) => label,
                    Some(label) => format!("{p} · {label}"),
                    None => (*p).to_owned(),
                }
            })
            .chain([RESET.get(lang).to_owned()])
            .collect();
        let start = DECIDERS
            .iter()
            .position(|p| now.as_deref() == Some(*p))
            .unwrap_or(0);
        let help = |_| Some(field.help.get(lang).to_owned());
        match term.select(&format!("{label} [{current}]"), &items, start, &help)? {
            Picked::Item(i) => {
                let picked = DECIDERS.get(i).copied();
                let before = doc.clone();
                if picked != now.as_deref() {
                    remove(doc, model);
                }
                if !change(doc, field.path, picked.map(Value::from), term)? {
                    *doc = before;
                }
            }
            Picked::Back => {}
            Picked::End => return Ok(false),
        }
        return Ok(true);
    }
    let menu: Option<(Vec<String>, Vec<Value>)> = match field.kind {
        Kind::Effort => {
            let config = parse(doc).unwrap_or_default();
            let levels = models.efforts(term, &config)?;
            let auto = crate::reasoning::lowest(&levels).unwrap_or_else(|| "-".into());
            let levels = if levels.is_empty() {
                // Not listed: every level, for the model to accept or not.
                crate::reasoning::LEVELS
                    .iter()
                    .map(|l| (*l).to_owned())
                    .collect()
            } else {
                levels
            };
            let values: Vec<String> = ["auto", "off"]
                .iter()
                .map(|v| (*v).to_owned())
                .chain(levels)
                .collect();
            Some((
                values
                    .iter()
                    .map(|v| match v.as_str() {
                        "auto" => AUTO_EFFORT.fill(lang, &[&auto]),
                        "off" => OFF_EFFORT.get(lang).to_owned(),
                        level => level.to_owned(),
                    })
                    .collect(),
                values.iter().map(|v| Value::from(v.as_str())).collect(),
            ))
        }
        Kind::Search => {
            let config = parse(doc).unwrap_or_default();
            let auto = format!("{:?}", config.search_provider()).to_lowercase();
            let values: Vec<&str> = SEARCHES
                .iter()
                .copied()
                .filter(|v| match *v {
                    "chatgpt" => config.chatgpt(),
                    "openrouter" => config.api_key().is_ok(),
                    _ => true,
                })
                .collect();
            Some((
                values
                    .iter()
                    .map(|v| match *v {
                        "auto" => format!("auto ({auto})"),
                        v => v.to_owned(),
                    })
                    .collect(),
                values.iter().map(|v| Value::from(*v)).collect(),
            ))
        }
        Kind::Choice(choices) => Some((
            choices.iter().map(|c| (*c).to_owned()).collect(),
            choices.iter().map(|c| Value::from(*c)).collect(),
        )),
        Kind::Bool => Some((
            vec![YES.get(lang).into(), NO.get(lang).into()],
            vec![Value::from(true), Value::from(false)],
        )),
        _ => None,
    };
    if let Kind::Tokens = field.kind {
        let mut items: Vec<String> = TOKEN_PICKS.iter().map(|p| (*p).to_owned()).collect();
        items.push(OTHER.get(lang).into());
        items.push(RESET.get(lang).into());
        let start = TOKEN_PICKS.iter().position(|p| *p == current).unwrap_or(0);
        let help = |_| Some(field.help.get(lang).to_owned());
        match term.select(&format!("{label} [{current}]"), &items, start, &help)? {
            Picked::Item(i) if i < TOKEN_PICKS.len() => {
                change(doc, field.path, Some(Value::from(TOKEN_PICKS[i])), term)?;
                return Ok(true);
            }
            Picked::Item(i) if i > TOKEN_PICKS.len() => {
                change(doc, field.path, None, term)?;
                return Ok(true);
            }
            Picked::Item(_) => {}
            Picked::Back => return Ok(true),
            Picked::End => return Ok(false),
        }
    }
    if let Some((mut items, values)) = menu {
        let start = values
            .iter()
            .position(|v| v.to_string().trim().trim_matches('"') == current)
            .unwrap_or(0);
        items.push(RESET.get(lang).into());
        let help = |_| Some(field.help.get(lang).to_owned());
        match term.select(&format!("{label} [{current}]"), &items, start, &help)? {
            Picked::Item(i) => {
                change(doc, field.path, values.get(i).cloned(), term)?;
            }
            Picked::Back => {}
            Picked::End => return Ok(false),
        }
        return Ok(true);
    }
    term.say(&format!("  {}", field.help.get(lang)))?;
    term.say(&format!("  {}", TYPE_HOW.get(lang)))?;
    let prompt = format!("{label} [{current}]: ");
    loop {
        let input = match field.kind {
            Kind::Secret { .. } => term.secret(&prompt)?,
            _ => term.ask(&prompt)?,
        };
        let Some(input) = input else {
            return Ok(false);
        };
        let value = match input.as_str() {
            "" => return Ok(true),
            "-" => None,
            text => match value_for(field.kind, text, lang) {
                Ok(value) => Some(value),
                Err(err) => {
                    term.say(&format!("  {err:#}"))?;
                    continue;
                }
            },
        };
        if change(doc, field.path, value, term)? {
            return Ok(true);
        }
    }
}

/// Sets (or with `None` resets) a value, undoing it when the file would no
/// longer load. `false`, with the reason said, when it was undone.
fn change<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    path: &[&str],
    value: Option<Value>,
    term: &mut Term<R, W>,
) -> Result<bool> {
    let before = doc.clone();
    let reset = value.is_none();
    match value {
        Some(value) => set(doc, path, value),
        None => remove(doc, path),
    }
    match parse(doc) {
        Err(err) => {
            *doc = before;
            term.tell(
                if reset { CANNOT_RESET } else { NOT_ACCEPTED },
                &[&format!("{err:#}")],
            )?;
            return Ok(false);
        }
        // Sign-in and the model lists use the proxy right away.
        Ok(config) if path == ["network", "proxy"] => {
            crate::net::set(config.network.proxy.as_deref());
        }
        Ok(_) => {}
    }
    Ok(true)
}

/// A proxy address as stored: without the `/` the URL ends with.
pub fn proxy_text(url: &reqwest::Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

fn finish<R: BufRead, W: Write>(
    path: &Path,
    doc: &DocumentMut,
    original: &str,
    term: &mut Term<R, W>,
    save: bool,
) -> Result<bool> {
    let text = doc.to_string();
    if !save || text == original {
        term.tell(
            if text == original {
                NO_CHANGES
            } else {
                NOT_SAVED
            },
            &[],
        )?;
        return Ok(false);
    }
    if let Err(err) = toml::from_str::<Config>(&text) {
        bail!(RESULT_INVALID.fill(term.lang, &[&format!("{err:#}")]));
    }
    write_private(path, &text)?;
    term.tell(SAVED, &[&path.display().to_string()])?;
    Ok(true)
}

/// Writes atomically with mode 0600, since the file may hold secrets.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml")
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Walks one section's fields. `false` when input ended.
fn edit_section<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    section: &Section,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<bool> {
    term.say(&format!("\n[{}]", section.title.get(term.lang)))?;
    let mut index = 0;
    while let Some(field) = section.fields.get(index) {
        if !relevant(field, &parse(doc).unwrap_or_default()) {
            index += 1;
            continue;
        }
        if let Kind::Revoke = field.kind {
            if !revoke_owner(doc, term)? {
                return Ok(false);
            }
            index += 1;
            continue;
        }
        let current = shown(doc, field, term.lang, models);
        let kind = match field.kind {
            Kind::Decider { .. } => Kind::Choice(DECIDERS),
            Kind::Effort => Kind::Choice(EFFORTS),
            Kind::Search => Kind::Choice(SEARCHES),
            kind => kind,
        };
        let options = match kind {
            Kind::Choice(choices) => format!(
                " ({})",
                choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{}={c}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Kind::Bool => match term.lang {
                Lang::En => " (y/n)".into(),
                Lang::Zh => " (y/n，是/否)".into(),
            },
            Kind::SignIn => match term.lang {
                Lang::En => " (1=browser 2=device code)".into(),
                Lang::Zh => " (1=浏览器 2=设备码)".into(),
            },
            Kind::Tokens => " (auto, 128k, 1m)".into(),
            _ => String::new(),
        };
        let prompt = format!("{}{options} [{current}]: ", field.label.get(term.lang));
        let input = match field.kind {
            Kind::Secret { .. } => term.secret(&prompt)?,
            _ => term.ask(&prompt)?,
        };
        let Some(input) = input else {
            return Ok(false);
        };
        match input.as_str() {
            "" => {}
            "?" => {
                term.say(&format!("  {}", field.help.get(term.lang)))?;
                continue;
            }
            text if let Kind::SignIn = field.kind => {
                let device = match text {
                    "1" => false,
                    "2" => true,
                    _ => {
                        term.say(&format!("  {}", PICK_ONE.fill(term.lang, &["1, 2"])))?;
                        continue;
                    }
                };
                if !sign_in(doc, term, models, device)? {
                    return Ok(false);
                }
            }
            "-" => {
                if !change(doc, field.path, None, term)? {
                    continue;
                }
            }
            text => {
                let value = match value_for(field.kind, text, term.lang) {
                    Ok(value) => value,
                    Err(err) => {
                        term.say(&format!("  {err:#}"))?;
                        continue;
                    }
                };
                if field.path == ["model", "model"] {
                    set_provider_for(doc, text);
                }
                if !change(doc, field.path, Some(value), term)? {
                    continue;
                }
            }
        }
        index += 1;
    }
    Ok(true)
}

/// The settings a person may change by asking in chat (the `settings`
/// tool): the companion section, chat reply splitting and the language.
/// Keys, permissions and access stay with the editor.
fn chat_settable() -> impl Iterator<Item = &'static Field> {
    let companion = sections()
        .find(|s| s.fields.iter().any(|f| f.path == ["heartbeat", "enabled"]))
        .map_or(&[][..], |s| s.fields);
    let elsewhere = sections()
        .flat_map(|s| s.fields)
        .filter(|f| matches!(f.path, [_, "split_replies"] | ["language"]));
    companion.iter().chain(elsewhere)
}

fn key_of(field: &Field) -> String {
    field.path.join(".")
}

/// The chat-settable settings in `path`, one per line: key, value, what it is.
pub fn chat_settings(path: &Path, lang: Lang) -> Result<String> {
    let doc = load(path)?;
    Ok(chat_settable()
        .map(|f| {
            format!(
                "{} = {} ({}: {})",
                key_of(f),
                describe(&doc, f, lang),
                f.label.get(lang),
                f.help.get(lang)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Changes one chat-settable setting in `path` to `text`, as typed in the
/// editor (`-` resets it), after checking that the result still loads and
/// that the heartbeat and inner life accept it. The value before and after.
pub fn change_setting(path: &Path, key: &str, text: &str, lang: Lang) -> Result<(String, String)> {
    let field = chat_settable()
        .find(|f| key_of(f) == key.trim())
        .with_context(|| {
            let keys: Vec<String> = chat_settable().map(key_of).collect();
            format!(
                "{key:?} cannot be changed here; use one of {}",
                keys.join(", ")
            )
        })?;
    let mut doc = load(path)?;
    let before = describe(&doc, field, lang);
    match text.trim() {
        "-" => remove(&mut doc, field.path),
        text => set(&mut doc, field.path, value_for(field.kind, text, lang)?),
    }
    let config = parse(&doc).context("the result does not load")?;
    crate::heartbeat::Heartbeat::from_config(&config.heartbeat, &config.access)?;
    crate::life::Life::from_config(&config.life, &config.access)?;
    write_private(path, &doc.to_string())?;
    Ok((before, describe(&doc, field, lang)))
}

fn value_for(kind: Kind, text: &str, lang: Lang) -> Result<Value> {
    Ok(match kind {
        Kind::Text | Kind::Model(_) => Value::from(text),
        Kind::Proxy => Value::from(proxy_text(&crate::net::parse(text)?)),
        Kind::Decider { .. } => return value_for(Kind::Choice(DECIDERS), text, lang),
        Kind::Effort => return value_for(Kind::Choice(EFFORTS), text, lang),
        Kind::Search => return value_for(Kind::Choice(SEARCHES), text, lang),
        Kind::Int => {
            let n: u32 = text
                .replace('_', "")
                .parse()
                .with_context(|| NOT_NUMBER.fill(lang, &[&format!("{text:?}")]))?;
            Value::from(i64::from(n))
        }
        Kind::Money => {
            let n: f64 = text
                .trim_start_matches('$')
                .parse()
                .ok()
                .filter(|n: &f64| n.is_finite() && *n >= 0.0)
                .with_context(|| NOT_NUMBER.fill(lang, &[&format!("{text:?}")]))?;
            Value::from(n)
        }
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "on" | "1" | "是" => Value::from(true),
            "n" | "no" | "false" | "off" | "0" | "否" => Value::from(false),
            _ => bail!(YES_NO.get(lang)),
        },
        Kind::Choice(choices) => {
            let pick = text
                .parse::<usize>()
                .ok()
                .and_then(|n| choices.get(n.wrapping_sub(1)))
                .or_else(|| choices.iter().find(|c| c.eq_ignore_ascii_case(text)));
            match pick {
                Some(choice) => Value::from(*choice),
                None => bail!(PICK_ONE.fill(lang, &[&choices.join(", ")])),
            }
        }
        Kind::Tokens => match crate::config::ContextBudget::parse(text) {
            Some(crate::config::ContextBudget::Auto) => Value::from("auto"),
            Some(crate::config::ContextBudget::Tokens(n)) => {
                Value::from(crate::config::exact_tokens(n))
            }
            None => bail!(NOT_TOKENS.fill(lang, &[&format!("{text:?}")])),
        },
        Kind::Dates => {
            let mut array = Array::new();
            for item in text
                .split([',', '，'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let (date, name) = item
                    .split_once(char::is_whitespace)
                    .map(|(d, n)| (d.trim(), n.trim()))
                    .filter(|(_, n)| !n.is_empty())
                    .with_context(|| NOT_A_DATE.fill(lang, &[&format!("{item:?}")]))?;
                let mut table = toml_edit::InlineTable::new();
                table.insert("date", date.into());
                table.insert("name", name.into());
                array.push(table);
            }
            Value::Array(array)
        }
        Kind::List => {
            let mut array = Array::new();
            for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                array.push(item);
            }
            Value::Array(array)
        }
        Kind::SignIn | Kind::Revoke => bail!("{text:?} cannot be typed here"),
        Kind::Secret { generate, .. } => {
            if generate && text == "+" {
                Value::from(random_token()?)
            } else {
                Value::from(text)
            }
        }
    })
}

/// 24 random bytes, hex-encoded.
fn random_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("cannot read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sets the value at `path`, keeping the old value's surrounding comments.
pub(crate) fn set(doc: &mut DocumentMut, path: &[&str], mut value: Value) {
    let (key, tables) = path.split_last().expect("paths are never empty");
    let mut item = doc.as_item_mut();
    for name in tables {
        if !item.get(*name).is_some_and(Item::is_table_like) {
            item[*name] = toml_edit::table();
        }
        item = &mut item[*name];
    }
    if let Some(old) = item.get(*key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    } else if tables.is_empty() {
        // A new root key sits above the tables; keep a blank line between.
        value.decor_mut().set_suffix("\n");
    }
    item[*key] = Item::Value(value);
}

pub(crate) fn remove(doc: &mut DocumentMut, path: &[&str]) {
    let (key, tables) = path.split_last().expect("paths are never empty");
    let mut item = doc.as_item_mut();
    for name in tables {
        match item.get_mut(*name) {
            Some(next) => item = next,
            None => return,
        }
    }
    if let Some(table) = item.as_table_like_mut() {
        table.remove(key);
    }
}

/// What the menus show for `field`: who is signed in for the sign-in.
fn shown(doc: &DocumentMut, field: &Field, lang: Lang, models: &Models<'_>) -> String {
    match field.kind {
        Kind::SignIn => models
            .signed_in
            .clone()
            .unwrap_or_else(|| NOT_SIGNED_IN.get(lang).into()),
        _ => describe(doc, field, lang),
    }
}

/// Runs the main model where `id` lives: OpenRouter's ids have a `/`,
/// ChatGPT's do not (a server of one's own keeps its provider).
fn set_provider_for(doc: &mut DocumentMut, id: &str) {
    let config = parse(doc).unwrap_or_default();
    if config.model.base_url.trim_end_matches('/') != crate::config::DEFAULT_BASE_URL {
        return;
    }
    let chatgpt = !id.contains('/');
    if chatgpt != config.chatgpt() {
        let provider = if chatgpt { "chatgpt" } else { "openrouter" };
        set(doc, &["model", "provider"], Value::from(provider));
    }
}

/// Points the model at ChatGPT; an OpenRouter model gives way to ChatGPT's
/// default (the first in its list).
fn on_chatgpt<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
) -> Result<()> {
    let chosen = crate::chatgpt::use_in_doc(doc, || {
        models
            .chatgpt(term)
            .ok()
            .and_then(|l| l.first())
            .map(|m| m.id.clone())
    });
    match chosen {
        Some(model) => term.tell(ON_CHATGPT, &[&model]),
        None => term.tell(ON_CHATGPT_PICK, &[]),
    }
}

/// Signs in to ChatGPT and points the model at it. `false` when input ended.
fn sign_in<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    term: &mut Term<R, W>,
    models: &mut Models<'_>,
    device: bool,
) -> Result<bool> {
    let Some(sign_in) = models.sign_in.as_mut() else {
        term.tell(NO_SIGN_IN, &[])?;
        return Ok(true);
    };
    match sign_in(term, device) {
        Ok(Some(who)) => {
            models.signed_in = Some(who);
            // Read again now that it can be.
            models.chatgpt_list = None;
            // Moved to ChatGPT only when nothing else runs the model yet.
            let config = parse(doc).unwrap_or_default();
            if config.chatgpt() || config.model.model.trim().is_empty() || config.api_key().is_err()
            {
                on_chatgpt(doc, term, models)?;
            } else {
                term.tell(SIGNED_IN_PICK, &[])?;
            }
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(err) => {
            term.tell(SIGN_IN_FAILED, &[&format!("{err:#}")])?;
            Ok(true)
        }
    }
}

fn owners_array(doc: &DocumentMut) -> Option<&Array> {
    doc.get("access")?.get("owners")?.as_array()
}

/// The owners on a chat app (`qq:`, `feishu:`, `zulip:`) listed in `doc`.
pub(crate) fn chat_owners(doc: &DocumentMut) -> Vec<String> {
    owners_array(doc)
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|o| crate::chat::private_chat(o).is_some())
        .map(str::to_owned)
        .collect()
}

/// Removes `owner` from `access.owners`.
fn revoke(doc: &mut DocumentMut, owner: &str) {
    if let Some(array) = doc
        .get_mut("access")
        .and_then(|a| a.get_mut("owners"))
        .and_then(Item::as_array_mut)
    {
        array.retain(|v| v.as_str().map(str::trim) != Some(owner));
    }
}

/// Lets the person pick a chat app's owner to revoke. `false` when input ended.
pub(crate) fn revoke_owner<R: BufRead, W: Write>(
    doc: &mut DocumentMut,
    term: &mut Term<R, W>,
) -> Result<bool> {
    let owners = chat_owners(doc);
    if owners.is_empty() {
        term.tell(NO_CHAT_OWNERS, &[])?;
        return Ok(true);
    }
    let picked = if term.arrows {
        let mut items = owners.clone();
        items.push(BACK.get(term.lang).into());
        let title = REVOKE_TITLE.get(term.lang);
        match term.select(title, &items, 0, &|_| None)? {
            Picked::Item(i) => owners.get(i).cloned(),
            Picked::Back => None,
            Picked::End => return Ok(false),
        }
    } else {
        for (i, owner) in owners.iter().enumerate() {
            term.say(&format!("  {}) {owner}", i + 1))?;
        }
        loop {
            let Some(answer) = term.ask(REVOKE_WHICH.get(term.lang))? else {
                return Ok(false);
            };
            if answer.is_empty() {
                break None;
            }
            let found = answer
                .parse::<usize>()
                .ok()
                .and_then(|n| owners.get(n.wrapping_sub(1)))
                .or_else(|| owners.iter().find(|o| o.eq_ignore_ascii_case(&answer)));
            match found {
                Some(owner) => break Some(owner.clone()),
                None => term.tell(PICK_ONE, &[&owners.join(", ")])?,
            }
        }
    };
    if let Some(owner) = picked {
        revoke(doc, &owner);
        term.tell(REVOKED, &[&owner])?;
    }
    Ok(true)
}

/// Adds `id` to `access.owners` in the file at `path`, unless it is there.
pub(crate) fn add_owner(path: &Path, id: &str) -> Result<()> {
    let mut doc = load(path)?;
    let listed = owners_array(&doc).is_some_and(|a| {
        a.iter()
            .filter_map(|v| v.as_str())
            .any(|o| o.trim().eq_ignore_ascii_case(id))
    });
    if listed {
        return Ok(());
    }
    match doc
        .get_mut("access")
        .and_then(|a| a.get_mut("owners"))
        .and_then(Item::as_array_mut)
    {
        Some(array) => array.push(id),
        None => {
            let mut array = Array::new();
            array.push(id);
            set(&mut doc, &["access", "owners"], Value::Array(array));
        }
    }
    let text = doc.to_string();
    toml::from_str::<Config>(&text).context("the new settings do not load")?;
    write_private(path, &text)
}

/// The value in effect for `field`, defaults included; secrets are never shown.
/// A string set at `path`, as the config loads it.
fn describe_path(doc: &DocumentMut, path: &[&str]) -> Option<String> {
    let config = toml::Value::try_from(parse(doc).ok()?).ok()?;
    let mut value = Some(&config);
    for name in path {
        value = value.and_then(|v| v.get(*name));
    }
    value.and_then(toml::Value::as_str).map(str::to_owned)
}

/// What a decision provider runs, e.g. "openrouter/typesafe/jev-1.13".
fn decider_label(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    chat: &str,
) -> Option<String> {
    let provider = match provider {
        "openrouter" => ReviewProvider::Openrouter,
        "openrouter-chat" => ReviewProvider::OpenrouterChat,
        "typesafe" => ReviewProvider::Typesafe,
        _ => ReviewProvider::Off,
    };
    crate::review::decider_label(provider, model, base_url, chat)
}

fn describe(doc: &DocumentMut, field: &Field, lang: Lang) -> String {
    let effective = parse(doc)
        .ok()
        .and_then(|config| toml::Value::try_from(&config).ok());
    let mut value = effective.as_ref();
    for name in field.path {
        value = value.and_then(|v| v.get(*name));
    }
    if let Kind::Secret { env, .. } = field.kind {
        let from_env = std::env::var(env).is_ok_and(|v| !v.trim().is_empty());
        let in_file = value
            .and_then(toml::Value::as_str)
            .is_some_and(|v| !v.is_empty());
        return match (from_env, in_file) {
            (true, _) => FROM_ENV.fill(lang, &[env]),
            (false, true) => IN_FILE.get(lang).into(),
            (false, false) => NOT_SET.get(lang).into(),
        };
    }
    if let Kind::Revoke = field.kind {
        let owners = chat_owners(doc);
        return if owners.is_empty() {
            NONE.get(lang).into()
        } else {
            owners.join(", ")
        };
    }
    if let Kind::Decider { model, base_url } = field.kind {
        let provider = value.and_then(toml::Value::as_str).unwrap_or("off");
        let chat = effective
            .as_ref()
            .and_then(|v| v.get("model"))
            .and_then(|v| v.get("model"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        let model = describe_path(doc, model);
        let base_url = describe_path(doc, base_url);
        return decider_label(provider, model.as_deref(), base_url.as_deref(), chat)
            .unwrap_or_else(|| provider.to_owned());
    }
    match value {
        None => DEFAULT.get(lang).into(),
        Some(toml::Value::String(s)) if s.is_empty() => EMPTY.get(lang).into(),
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Array(items)) if items.is_empty() => NONE.get(lang).into(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|i| match (i.get("date"), i.get("name")) {
                (Some(toml::Value::String(date)), Some(toml::Value::String(name))) => {
                    format!("{date} {name}")
                }
                _ => i.as_str().map_or_else(|| i.to_string(), str::to_owned),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(path: &Path, script: &str) -> String {
        drive_in(Lang::En, path, script)
    }

    fn drive_in(lang: Lang, path: &Path, script: &str) -> String {
        let mut term = Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            arrows: false,
            raw: false,
            lang,
        };
        edit(path, &mut term).unwrap();
        String::from_utf8(term.out).unwrap()
    }

    #[test]
    fn edits_keep_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my notes\n[model]\nmodel = \"x/old\"  # keep me\n\n[custom]\nthing = 1\n",
        )
        .unwrap();
        // Model › provider: skip the sign-in, keep the key, new id, keep
        // the thinking level, two fallbacks. Model › context: keep
        // retries, cache by number, a bad then a good budget, reset summary
        // model, keep image and audio models, recall and max steps.
        let script = "2\n1\n\n\n\na/new\n\nb/one, c/two ,\n2\n2\n\n3\nlots\n32000\n-\n\n\n\n\ns\n";
        let out = drive(&path, script);
        assert!(out.contains("\"lots\" is not a token count"), "{out}");
        assert!(out.contains("Saved"), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes"));
        assert!(text.contains("model = \"a/new\"  # keep me"), "{text}");
        assert!(text.contains("[custom]\nthing = 1"));
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.model.model, "a/new");
        assert_eq!(config.model.fallbacks, ["b/one", "c/two"]);
        assert_eq!(config.model.prompt_cache, crate::config::PromptCache::Off);
        assert_eq!(
            config.agent.context_tokens,
            crate::config::ContextBudget::Tokens(32_000)
        );
        assert!(text.contains("context_tokens = \"32k\""), "{text}");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn sets_up_the_companion_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let section = "6";
        // Time sense kept; heartbeat on, every 90 min, keep quiet hours and
        // the daily limit, a prompt; inner life on, keep the times, a city,
        // a bad then a good list of dates; keep both sessions.
        let script = format!(
            "{section}\n\ny\n90\n\n\n偶尔问问我跑步\ny\n\n\n\n上海\n生日\n03-14 我的生日，05-20 纪念日\n\n\ns\n"
        );
        let out = drive_in(Lang::Zh, &path, &script);
        assert!(out.contains("不是“月-日 名字”的格式"), "{out}");
        assert!(out.contains("已保存"), "{out}");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.heartbeat.enabled && config.life.enabled);
        assert_eq!(config.heartbeat.every_minutes, 90);
        assert_eq!(config.heartbeat.prompt.as_deref(), Some("偶尔问问我跑步"));
        assert_eq!(config.life.city.as_deref(), Some("上海"));
        assert_eq!(
            config
                .life
                .dates
                .iter()
                .map(|d| (d.date.as_str(), d.name.as_str()))
                .collect::<Vec<_>>(),
            [("03-14", "我的生日"), ("05-20", "纪念日")]
        );
        crate::life::Life::from_config(&config.life, &config.access)
            .unwrap()
            .unwrap();
        // Shown back as typed.
        // Through the section's fields, past the dates, to the menu.
        let enter = "\n".repeat(14);
        let again = drive_in(Lang::Zh, &path, &format!("{section}\n{enter}q\n"));
        assert!(again.contains("03-14 我的生日, 05-20 纪念日"), "{again}");
    }

    #[test]
    fn chat_changes_only_what_it_may_and_only_to_what_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[access]\nowners = [\"qq:ME\"]\n").unwrap();
        let lang = Lang::En;
        let shown = chat_settings(&path, lang).unwrap();
        assert!(
            shown.contains("life.morning = 08:30 (Good-morning at"),
            "{shown}"
        );
        assert!(shown.contains("qq.split_replies = true"), "{shown}");
        assert!(!shown.contains("access.owners") && !shown.contains("api_key"));

        assert_eq!(
            change_setting(&path, "life.morning", "09:00", lang).unwrap(),
            ("08:30".to_owned(), "09:00".to_owned())
        );
        change_setting(&path, "heartbeat.enabled", "是", lang).unwrap();
        change_setting(&path, "life.dates", "03-14 my birthday", lang).unwrap();
        for (key, value) in [
            ("access.owners", "qq:EVE"),
            ("model.api_key", "sk-x"),
            ("tools.shell", "allow"),
            ("heartbeat.enabled", "maybe"),
            ("heartbeat.quiet_hours", "late"),
            ("life.morning", "9"),
            ("heartbeat.session", "qq:c2c:STRANGER"),
        ] {
            assert!(
                change_setting(&path, key, value, lang).is_err(),
                "{key} = {value}"
            );
        }
        // A reset goes back to the default.
        assert_eq!(
            change_setting(&path, "life.morning", "-", lang).unwrap().1,
            "08:30"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine"), "{text}");
        let config: Config = toml::from_str(&text).unwrap();
        assert!(config.heartbeat.enabled);
        assert_eq!(config.life.dates[0].name, "my birthday");
        assert_eq!(config.access.owners, ["qq:ME"]);
        assert_eq!(config.heartbeat.quiet_hours, "23:00-08:00");
    }

    #[test]
    fn shows_current_values_and_never_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[qq]\napp_secret = \"hunter2\"\nallow = [\"A\", \"B\"]\n",
        )
        .unwrap();
        let out = drive(&path, "3\n1\n?\n\n\n\n\nq\n");
        assert!(out.contains("Enabled (y/n) [false]"), "{out}");
        assert!(out.contains("Connects to QQ over WebSocket"), "{out}");
        assert!(out.contains("Allowed openids [A, B]"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("No changes."));
    }

    #[test]
    fn quitting_or_running_out_of_input_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive(&path, "7\n1\n0.0.0.0:1\n+\nq\n");
        assert!(out.contains("Quit without saving."), "{out}");
        assert!(!path.exists());
        drive(&path, "3\n1\ny\n");
        assert!(!path.exists());
    }

    #[test]
    fn generates_a_token_and_sets_nested_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        drive(&path, "7\n1\n\n+\n\n4\n1\n\n\n\n\n\nopenrouter\n\ns\n");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let token = config.gateway.token.unwrap();
        assert_eq!(token.len(), 48);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            config.tools.review.provider,
            crate::config::ReviewProvider::Openrouter
        );
    }

    #[test]
    fn a_new_root_key_goes_above_the_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[model]\nmodel = \"x/y\"\n").unwrap();
        drive(&path, "1\nzh\ns\n");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("language = \"zh\"\n\n[model]"), "{text}");
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.language, Some(Lang::Zh));
    }

    #[test]
    fn speaks_chinese() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive_in(
            Lang::Zh,
            &path,
            "3\n1\n?\n是\n\n\n\n\n99\n3\n4\n\n\n\nabc\n\n\n\n\n\n\n\n\n\ns\n",
        );
        assert!(out.contains("2) 模型"), "{out}");
        assert!(out.contains("1) QQ 机器人"), "{out}");
        assert!(out.contains("s) 保存并退出"), "{out}");
        assert!(out.contains("启用 (y/n，是/否) [false]"), "{out}");
        assert!(
            out.contains("运行 `serve` 时通过 WebSocket 连接 QQ。"),
            "{out}"
        );
        assert!(out.contains("AppSecret [未设置]"), "{out}");
        assert!(out.contains("请输入编号、s 或 q。"), "{out}");
        assert!(out.contains("\"abc\" 不是整数"), "{out}");
        assert!(out.contains("已保存"), "{out}");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.qq.enabled);
    }

    #[test]
    fn every_text_has_both_languages() {
        let mut texts = vec![
            BROKEN,
            EDITING,
            HOW,
            SAVE,
            QUIT,
            PICK_MENU,
            NO_CHANGES,
            NOT_SAVED,
            SAVED,
            RESTART,
            RESTARTED,
            RESULT_INVALID,
            CANNOT_RESET,
            NOT_ACCEPTED,
            NOT_NUMBER,
            NOT_TOKENS,
            OTHER,
            SEARCH_MODELS,
            LOADING_MODELS,
            NO_MODEL_LIST,
            YES_NO,
            NOT_A_DATE,
            PICK_ONE,
            DEFAULT,
            EMPTY,
            NONE,
            FROM_ENV,
            IN_FILE,
            NOT_SET,
            KEYS_HINT,
            YES,
            NO,
            BACK,
            RESET,
            UNSAVED,
            KEEP_EDITING,
            TYPE_HOW,
            NOT_SIGNED_IN,
            NO_SIGN_IN,
            SIGN_IN_FAILED,
            SIGN_IN_BROWSER,
            SIGN_IN_DEVICE,
            SIGNED_IN_PICK,
            ON_CHATGPT,
            ON_CHATGPT_PICK,
            AUTO_EFFORT,
            OFF_EFFORT,
            NO_CHAT_OWNERS,
            REVOKED,
            REVOKE_TITLE,
            REVOKE_WHICH,
            BACK_PROMPT,
        ];
        for group in GROUPS {
            texts.push(group.title);
        }
        for section in sections() {
            texts.push(section.title);
            for field in section.fields {
                texts.extend([field.label, field.help]);
            }
        }
        crate::i18n::assert_complete(&texts);
        assert_eq!(SAVED.fill(Lang::Zh, &["a.toml"]), "已保存 a.toml。");
    }

    /// The typed menu answers that open the section titled `title`.
    fn open(title: &str) -> String {
        for (g, group) in GROUPS.iter().enumerate() {
            for (i, section) in group.sections.iter().enumerate() {
                if section.title.get(Lang::En) == title {
                    return match group.sections.len() {
                        1 => format!("{}\n", g + 1),
                        _ => format!("{}\n{}\n", g + 1, i + 1),
                    };
                }
            }
        }
        panic!("no section {title}")
    }

    #[test]
    fn lets_the_first_chat_claim_ownership_and_revokes_a_chat_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[access]\nowners = [\"mail:me@example.org\", \"qq:ME\", \"zulip:z@example.org\"]\n",
        )
        .unwrap();
        // Keep the owners, turn claiming on, revoke the second chat owner.
        let script = format!("{}\ny\n2\ns\n", open("Access"));
        let out = drive(&path, &script);
        assert!(
            out.contains("1) qq:ME") && out.contains("2) zulip:z@example.org"),
            "{out}"
        );
        assert!(out.contains("Revoked zulip:z@example.org"), "{out}");
        let config = Config::load(&path).unwrap();
        assert!(config.access.first_chat_owner);
        assert_eq!(config.access.owners, ["mail:me@example.org", "qq:ME"]);

        // With arrows: Access and spending › Access, the revoke field
        // (third), the only chat owner; back out and save.
        let mut term = keys("5\r1\r\x1b[B\x1b[B\r\rqqq\r");
        edit_with(&path, &mut term, &mut Models::default()).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.access.owners, ["mail:me@example.org"]);
        // Nothing left to revoke.
        let out = drive(&path, &format!("{}\n\nq\n", open("Access")));
        assert!(out.contains("No chat app"), "{out}");
    }

    #[test]
    fn adds_a_claimed_owner_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[model]\nmodel = \"a/b\"\n").unwrap();
        add_owner(&path, "qq:ALICE").unwrap();
        add_owner(&path, "feishu:ou_b").unwrap();
        add_owner(&path, "qq:alice").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine"), "{text}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.access.owners, ["qq:ALICE", "feishu:ou_b"]);
    }

    fn listed(id: &str, efforts: &[&str]) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            context: 0,
            price: None,
            images: true,
            audio: false,
            tools: true,
            max_output: None,
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    #[test]
    fn chat_app_proxy_switches_show_only_with_a_proxy() {
        let field = Field {
            path: &["qq", "proxy"],
            label: tr("", ""),
            help: tr("", ""),
            kind: Kind::Bool,
        };
        let mut config = Config::default();
        assert!(!relevant(&field, &config));
        config.network.proxy = Some("http://127.0.0.1:7890".into());
        assert!(relevant(&field, &config));
        assert!(!config.qq.proxy && !config.feishu.proxy && !config.zulip.proxy);
    }

    #[test]
    fn proxy_addresses_are_checked_and_tidied() {
        let value = value_for(Kind::Proxy, "127.0.0.1:7890", Lang::En).unwrap();
        assert_eq!(value.as_str(), Some("http://127.0.0.1:7890"));
        assert!(value_for(Kind::Proxy, "socks5://127.0.0.1:1080", Lang::En).is_err());
    }

    #[test]
    fn offers_the_thinking_levels_the_model_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[model]\nprovider = \"chatgpt\"\nmodel = \"gpt-x\"\n",
        )
        .unwrap();
        let mut models = Models {
            chatgpt: Some(Box::new(|| {
                Ok(vec![
                    listed("gpt-y", &["minimal"]),
                    listed("gpt-x", &["low", "medium", "high"]),
                ])
            })),
            ..Models::default()
        };
        // Model › provider › thinking level (5th; no fallbacks on ChatGPT):
        // auto, off, low, medium.
        let mut term = keys("2\r1\r5\r4\rqqq\r");
        edit_with(&path, &mut term, &mut models).unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(
            out.contains("auto (the model's lowest from low up: low)"),
            "{out}"
        );
        assert!(!out.contains("minimal"), "{out}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.model.reasoning_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn signs_in_to_chatgpt_from_the_model_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[model]\nmodel = \"x/y\"\n").unwrap();
        let mut asked = Vec::new();
        let mut models = Models {
            chatgpt: Some(Box::new(|| Ok(vec![listed("gpt-x", &[])]))),
            sign_in: Some(Box::new(|term: &mut dyn Prompt, device| {
                asked.push(device);
                let code = term.ask("code: ")?;
                assert_eq!(code.as_deref(), Some("abc"));
                Ok(Some("me@example.org".into()))
            })),
            ..Models::default()
        };
        // Model › provider › ChatGPT account, in a browser; then again
        // with a code; back out and save.
        let script = "2\r1\r2\r\rabc\n2\r\x1b[B\rabc\nqqq\r";
        let mut term = keys(script);
        edit_with(&path, &mut term, &mut models).unwrap();
        assert_eq!(models.signed_in.as_deref(), Some("me@example.org"));
        drop(models);
        assert_eq!(asked, [false, true]);
        let config = Config::load(&path).unwrap();
        // Nothing else ran the model (no OpenRouter key): ChatGPT's default.
        assert!(config.chatgpt());
        assert_eq!(config.model.model, "gpt-x");

        // With an OpenRouter key the main model stays until one is picked;
        // picking ChatGPT's moves it there.
        std::fs::write(&path, "[model]\napi_key = \"sk\"\nmodel = \"x/y\"\n").unwrap();
        let mut models = Models::new(|| Ok(vec![listed("x/y", &[])]));
        models.chatgpt = Some(Box::new(|| Ok(vec![listed("gpt-x", &[])])));
        models.sign_in = Some(Box::new(|_: &mut dyn Prompt, _| {
            Ok(Some("me@example.org".into()))
        }));
        let mut term = keys("2\r1\r2\r\r4\r\rgpt\n\rqqq\r");
        edit_with(&path, &mut term, &mut models).unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(out.contains("the main model stays"), "{out}");
        let config = Config::load(&path).unwrap();
        assert!(config.chatgpt());
        assert_eq!(config.model.model, "gpt-x");
    }

    fn keys(script: &str) -> Term<&[u8], Vec<u8>> {
        Term {
            input: script.as_bytes(),
            out: Vec::new(),
            hide_secrets: false,
            arrows: true,
            raw: false,
            lang: Lang::En,
        }
    }

    #[test]
    fn picks_models_from_the_list_and_shows_what_deciders_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[model]\nmodel = \"a/old\"\n").unwrap();
        let model = |id: &str, images| ModelInfo {
            id: id.into(),
            name: id.into(),
            context: 100_000,
            price: None,
            images,
            audio: false,
            tools: true,
            max_output: None,
            efforts: Vec::new(),
        };
        let mut loads = 0;
        let mut models = Models::new(|| {
            loads += 1;
            Ok(vec![model("maker/blind", false), model("other/sees", true)])
        });
        // Model › provider, the model field (3rd), search "sees" and take
        // it; back, then Model › messages during a reply, its provider, one
        // down to openrouter-chat; back out and save.
        let script = "2\r1\r4\r\rsees\n\rq3\r2\rj\rqqq\r";
        let mut term = keys(script);
        edit_with(&path, &mut term, &mut models).unwrap();
        drop(models);
        let out = String::from_utf8(term.out).unwrap();
        assert_eq!(loads, 1);
        assert!(out.contains("openrouter/typesafe/jev-1.13"), "{out}");
        assert!(out.contains("openrouter/other/sees"), "{out}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.model.model, "other/sees");
        assert_eq!(config.guide.provider, ReviewProvider::OpenrouterChat);
    }

    #[test]
    fn menus_move_with_arrows_and_wrap_around() {
        let items: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        let pick = |script: &str| keys(script).select("T", &items, 0, &|_| None).unwrap();
        assert_eq!(pick("\x1b[B\r"), Picked::Item(1));
        assert_eq!(pick("\x1b[B\x1b[B\x1b[B\r"), Picked::Item(0));
        assert_eq!(pick("\x1b[A\r"), Picked::Item(2));
        assert_eq!(pick("jjk\r"), Picked::Item(1));
        assert_eq!(pick("\x1b[F\x1b[H\r"), Picked::Item(0));
        assert_eq!(pick("3\r"), Picked::Item(2));
        assert_eq!(pick("9x\r"), Picked::Item(0));
        assert_eq!(pick("q"), Picked::Back);
        assert_eq!(pick("\x1b\x1b"), Picked::Back);
        assert_eq!(pick(""), Picked::End);
        assert_eq!(pick("\x03"), Picked::End);
        let mut term = keys("j\r");
        let help = |i: usize| Some(format!("help for {i}"));
        term.select("Title", &items, 0, &help).unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(
            out.contains("help for 0") && out.contains("help for 1"),
            "{out}"
        );
        assert!(out.ends_with("Title › b\r\n\x1b[?25h"), "{out:?}");
        let mut term = keys("j\r");
        assert_eq!(term.confirm("Go on? [Y/n]: ", true).unwrap(), Some(false));
        let out = String::from_utf8(term.out).unwrap();
        assert!(out.contains("Go on? › No"), "{out:?}");
    }

    #[test]
    fn wraps_and_cuts_by_display_width() {
        assert_eq!(text_width("中文ab"), 6);
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("中文中文", 5), "中文…");
        assert_eq!(wrap("one two three", 8), ["one two", "three"]);
        assert_eq!(wrap("中文中文中文", 4), ["中文", "中文", "中文"]);
    }

    #[test]
    fn edits_with_arrow_menus() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Tools and the web (4) › Tools (1): shell → allow (1). Back twice.
        // Gateway and updates (7) › Gateway (1): bind typed, token
        // generated. Back twice. End, up: "Save and exit".
        let script = "4\r1\r\r1\rqq7\r1\r\r127.0.0.1:1\nj\r+\nqq\x1b[F\x1b[A\r";
        let mut term = keys(script);
        edit(&path, &mut term).unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(out.contains("Saved"), "{out}");
        assert!(out.contains("Shell commands [ask] › allow"), "{out}");
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.tools.shell, crate::config::Permission::Allow);
        assert_eq!(config.gateway.bind, "127.0.0.1:1");
        assert_eq!(config.gateway.token.unwrap().len(), 48);
    }

    #[test]
    fn picks_a_context_budget_from_the_menu_or_types_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Model (2) › context (2), the budget (3rd field): 128k from the menu.
        let mut term = keys("2\r2\r3\r4\rqq\x1b[F\x1b[A\r");
        edit(&path, &mut term).unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(out.contains("Context budget [auto] › 128k"), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("context_tokens = \"128k\""), "{text}");
        // "Other…" takes a typed count, written back with k or m.
        let mut term = keys("2\r2\r3\r7\r1500000\nqq\x1b[F\x1b[A\r");
        edit(&path, &mut term).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("context_tokens = \"1.5m\""), "{text}");
    }

    #[test]
    fn going_back_with_changes_asks_to_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Language: zh; back to the main menu; back again: "Save and exit".
        let mut term = keys("1\r\r2\rqq\r");
        edit(&path, &mut term).unwrap();
        let out = String::from_utf8(term.out).unwrap();
        assert!(out.contains("There are unsaved changes."), "{out}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("language = \"zh\""), "{text}");
        // Quitting from that question saves nothing.
        let other = dir.path().join("other.toml");
        edit(&other, &mut keys("1\r\r2\rqqj\r")).unwrap();
        assert!(!other.exists());
    }

    #[test]
    fn rejects_a_choice_outside_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = drive(&path, "4\n1\nmaybe\n9\nask\n\n\n\n\n\n\ns\n");
        assert_eq!(
            out.matches("pick one of: allow, ask, deny").count(),
            2,
            "{out}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("shell = \"ask\"")
        );
    }
}
