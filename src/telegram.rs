//! Telegram bot: long-polls `getUpdates`, answers through the Bot API.
//!
//! Private chats are `telegram:dm:<user id>`; groups are
//! `telegram:group:<chat id>`, answered when the bot is @-mentioned or
//! replied to. Pictures and voice messages that come in are taken as
//! attachments (voice is transcribed); the agent's pictures go out with
//! `sendPhoto`, its voice messages with `sendVoice`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{Model, Tools};
use crate::attachments::Upload;
use crate::chat::{Approvals, Seen, reply_or_error, reply_parts, split, typing_pause};
use crate::gateway::{Gateway, Notifier};
use crate::tools::{Approver, with_approver};

/// Telegram's message limit is 4096 characters.
const CHUNK_CHARS: usize = 4000;
/// How long one poll waits for updates, in seconds.
const POLL_SECS: u64 = 50;
/// Largest file the Bot API lets a bot download.
const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;

/// `[telegram]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: bool,
    /// Through `network.proxy`, which Telegram often needs.
    pub proxy: bool,
    /// From @BotFather; prefer `TELEGRAM_BOT_TOKEN`.
    pub token: Option<String>,
    /// Bot API origin; for a local Bot API server or tests.
    pub api_url: String,
    /// User ids (or @usernames) and group chat ids that may talk to the
    /// bot; empty allows everyone.
    pub allow: Vec<String>,
    /// As `qq.split_replies`.
    pub split_replies: bool,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            proxy: true,
            token: None,
            api_url: "https://api.telegram.org".into(),
            allow: Vec::new(),
            split_replies: true,
        }
    }
}

impl TelegramConfig {
    pub fn token(&self) -> Option<String> {
        std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .or_else(|| self.token.clone())
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A private chat, by the person's user id (which is also its chat id).
    User(i64),
    /// A group or supergroup, by its chat id.
    Group(i64),
}

impl Target {
    pub fn session(&self) -> String {
        match self {
            Target::User(id) => format!("telegram:dm:{id}"),
            Target::Group(id) => format!("telegram:group:{id}"),
        }
    }

    pub fn from_session(session: &str) -> Option<Self> {
        if let Some(id) = session.strip_prefix("telegram:dm:") {
            return id.parse().ok().map(Target::User);
        }
        session
            .strip_prefix("telegram:group:")?
            .parse()
            .ok()
            .map(Target::Group)
    }

    fn chat_id(&self) -> i64 {
        match self {
            Target::User(id) | Target::Group(id) => *id,
        }
    }
}

/// The bot's own account.
#[derive(Debug, Clone)]
struct Me {
    id: i64,
    username: String,
}

/// A file attached to a message: its id and a name for it.
#[derive(Debug, Clone, PartialEq)]
struct File {
    id: String,
    name: String,
    mime: Option<String>,
    size: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Incoming {
    update_id: i64,
    target: Target,
    /// The sender's user id.
    sender: i64,
    username: Option<String>,
    text: String,
    files: Vec<File>,
    /// Private, or @-mentioned or replied to in a group.
    addressed: bool,
}

pub struct TelegramBot {
    http: reqwest::Client,
    config: TelegramConfig,
    token: String,
    seen: Seen,
    approvals: Approvals,
}

impl TelegramBot {
    pub fn new(config: TelegramConfig) -> Result<Arc<Self>> {
        let token = config
            .token()
            .context("telegram.token is not set (or TELEGRAM_BOT_TOKEN)")?;
        let http = crate::net::builder_for(config.proxy)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(POLL_SECS + 30))
            .build()
            .context("cannot build HTTP client for Telegram")?;
        Ok(Arc::new(Self {
            http,
            config,
            token,
            seen: Seen::default(),
            approvals: Approvals::default(),
        }))
    }

    fn url(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{method}",
            self.config.api_url.trim_end_matches('/'),
            self.token
        )
    }

    /// Calls a Bot API method and returns its `result`.
    async fn call(&self, method: &str, body: &Value) -> Result<Value> {
        let response = self
            .http
            .post(self.url(method))
            .json(body)
            .send()
            .await
            .with_context(|| format!("Telegram {method} failed"))?;
        Self::result(method, response).await
    }

    async fn result(method: &str, response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if body["ok"] != true {
            let message = body["description"].as_str().unwrap_or("no answer");
            bail!("Telegram {method} returned HTTP {status}: {message}");
        }
        Ok(body["result"].clone())
    }

    async fn me(&self) -> Result<Me> {
        let me = self.call("getMe", &json!({})).await?;
        Ok(Me {
            id: me["id"].as_i64().context("getMe has no id")?,
            username: me["username"].as_str().unwrap_or("").to_owned(),
        })
    }

    /// Who the token belongs to, for `doctor`.
    pub async fn check(&self) -> Result<String> {
        let me = self.me().await?;
        Ok(format!("@{}", me.username))
    }

    pub async fn send(&self, target: &Target, text: &str) -> Result<()> {
        for chunk in split(text, CHUNK_CHARS) {
            self.call(
                "sendMessage",
                &json!({ "chat_id": target.chat_id(), "text": chunk }),
            )
            .await?;
        }
        Ok(())
    }

    /// Sends a file with `method` (`sendPhoto`, `sendVoice`) as its `field`.
    async fn send_file(
        &self,
        target: &Target,
        method: &str,
        field: &str,
        name: &str,
        mime: &str,
        data: &[u8],
    ) -> Result<()> {
        let chat = target.chat_id().to_string();
        let (kind, body) = crate::net::multipart(&[
            ("chat_id", crate::net::Part::Text(&chat)),
            (field, crate::net::Part::File { name, mime, data }),
        ]);
        let response = self
            .http
            .post(self.url(method))
            .header(reqwest::header::CONTENT_TYPE, kind)
            .body(body)
            .send()
            .await
            .with_context(|| format!("Telegram {method} failed"))?;
        Self::result(method, response).await.map(|_| ())
    }

    async fn download(&self, file: &File) -> Result<Upload> {
        if file.size > MAX_DOWNLOAD {
            bail!("{} is larger than the 20 MB bots may download", file.name);
        }
        let info = self.call("getFile", &json!({ "file_id": file.id })).await?;
        let path = info["file_path"]
            .as_str()
            .context("Telegram gave no path for the file")?;
        let url = format!(
            "{}/file/bot{}/{path}",
            self.config.api_url.trim_end_matches('/'),
            self.token
        );
        let data = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("cannot download {}", file.name))?
            .bytes()
            .await?
            .to_vec();
        Ok(Upload {
            name: file.name.clone(),
            mime: file.mime.clone(),
            data,
        })
    }

    fn allowed(&self, incoming: &Incoming) -> bool {
        let allow = &self.config.allow;
        allow.is_empty()
            || allow.iter().any(|a| {
                let a = a.trim();
                a == incoming.sender.to_string()
                    || a == incoming.target.chat_id().to_string()
                    || incoming
                        .username
                        .as_deref()
                        .is_some_and(|u| a.trim_start_matches('@').eq_ignore_ascii_case(u))
            })
    }
}

/// The files of a message: the largest photo size, a voice message, audio
/// or a document.
fn files(message: &Value) -> Vec<File> {
    let mut found = Vec::new();
    let size = |v: &Value| v["file_size"].as_u64().unwrap_or(0);
    if let Some(photo) = message["photo"]
        .as_array()
        .and_then(|sizes| sizes.iter().max_by_key(|p| size(p)))
        && let Some(id) = photo["file_id"].as_str()
    {
        found.push(File {
            id: id.to_owned(),
            name: "photo.jpg".into(),
            mime: Some("image/jpeg".into()),
            size: size(photo),
        });
    }
    for (key, name, mime) in [
        ("voice", "voice.ogg", "audio/ogg"),
        ("audio", "audio", "audio/mpeg"),
        ("document", "file", "application/octet-stream"),
    ] {
        let item = &message[key];
        if let Some(id) = item["file_id"].as_str() {
            found.push(File {
                id: id.to_owned(),
                name: item["file_name"].as_str().unwrap_or(name).to_owned(),
                mime: Some(item["mime_type"].as_str().unwrap_or(mime).to_owned()),
                size: size(item),
            });
        }
    }
    found
}

/// An update as a message the bot may answer, unless it sent it.
fn parse_incoming(update: &Value, me: &Me) -> Option<Incoming> {
    let message = &update["message"];
    let from = &message["from"];
    let sender = from["id"].as_i64()?;
    if sender == me.id || from["is_bot"] == true {
        return None;
    }
    let chat = &message["chat"];
    let chat_id = chat["id"].as_i64()?;
    let raw = message["text"]
        .as_str()
        .or_else(|| message["caption"].as_str())
        .unwrap_or("");
    let mention = format!("@{}", me.username);
    let (target, addressed) = match chat["type"].as_str()? {
        "private" => (Target::User(sender), true),
        "group" | "supergroup" => {
            let mentioned = !me.username.is_empty() && raw.contains(&mention);
            let replied = message["reply_to_message"]["from"]["id"].as_i64() == Some(me.id);
            (Target::Group(chat_id), mentioned || replied)
        }
        _ => return None,
    };
    let text = if me.username.is_empty() {
        raw.trim().to_owned()
    } else {
        raw.replace(&mention, "").trim().to_owned()
    };
    let files = files(message);
    if text.is_empty() && files.is_empty() {
        return None;
    }
    Some(Incoming {
        update_id: update["update_id"].as_i64()?,
        target,
        sender,
        username: from["username"].as_str().map(str::to_owned),
        text,
        files,
        addressed,
    })
}

/// Answers a Telegram turn's approvals: the owner is asked in the chat and
/// replies yes or no; anyone else is declined unasked.
struct TelegramApprover {
    bot: Arc<TelegramBot>,
    target: Target,
    sender: String,
    owner: bool,
}

#[async_trait]
impl Approver for TelegramApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let session = self.target.session();
        if !self.owner {
            eprintln!(
                "telegram: declined {tool} for {} in {session} (not an owner)",
                self.sender
            );
            return false;
        }
        let (_, answer) = self.bot.approvals.open(&session, &self.sender);
        let prompt = crate::i18n::chat::APPROVAL_ASK.with(&[tool, summary]);
        let asked = self.bot.send(&self.target, &prompt).await;
        self.bot.approvals.wait(&session, asked, answer).await
    }

    async fn send_image(&self, mime: &str, data: &[u8]) -> Result<bool> {
        let name = format!("picture.{}", mime.rsplit('/').next().unwrap_or("png"));
        self.bot
            .send_file(&self.target, "sendPhoto", "photo", &name, mime, data)
            .await?;
        Ok(true)
    }

    fn plays_voice(&self) -> bool {
        true
    }

    async fn send_voice(&self, mime: &str, data: &[u8]) -> Result<bool> {
        self.bot
            .send_file(&self.target, "sendVoice", "voice", "voice.mp3", mime, data)
            .await?;
        Ok(true)
    }
}

#[async_trait]
impl Notifier for TelegramBot {
    fn handles(&self, session: &str) -> bool {
        Target::from_session(session).is_some()
    }

    async fn notify(&self, session: &str, text: &str) -> Result<()> {
        let target = Target::from_session(session).context("not a Telegram session")?;
        self.send(&target, text).await
    }
}

/// Keeps polling for the life of the process.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<TelegramBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        if let Err(err) = poll(&bot, &gateway).await {
            eprintln!("telegram: polling stopped: {err:#}");
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn poll<M: Model + 'static, T: Tools + 'static>(
    bot: &Arc<TelegramBot>,
    gateway: &Arc<Gateway<M, T>>,
) -> Result<()> {
    let me = bot.me().await?;
    eprintln!("telegram: connected as @{}", me.username);
    let mut offset: i64 = 0;
    loop {
        let updates = bot
            .call(
                "getUpdates",
                &json!({ "offset": offset, "timeout": POLL_SECS, "allowed_updates": ["message"] }),
            )
            .await?;
        for update in updates.as_array().into_iter().flatten() {
            if let Some(id) = update["update_id"].as_i64() {
                offset = offset.max(id + 1);
            }
            if let Some(incoming) = parse_incoming(update, &me) {
                handle(bot.clone(), gateway.clone(), incoming);
            }
        }
    }
}

fn handle<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<TelegramBot>,
    gateway: Arc<Gateway<M, T>>,
    incoming: Incoming,
) {
    if !bot.seen.first(&incoming.update_id.to_string()) {
        return;
    }
    let session = incoming.target.session();
    let sender = format!("telegram:{}", incoming.sender);
    if !bot.allowed(&incoming) {
        if incoming.addressed {
            eprintln!("telegram: ignored {session} from {sender} (not in telegram.allow)");
        }
        return;
    }
    if bot.approvals.answer(&session, &sender, &incoming.text) {
        eprintln!("telegram: approval answered by {sender} in {session}");
        return;
    }
    if !incoming.addressed {
        return;
    }
    eprintln!("telegram: message from {sender} in {session}");
    tokio::spawn(async move {
        let actor = gateway.actor(&sender, &session);
        let mut uploads = Vec::new();
        let mut problems = Vec::new();
        for file in &incoming.files {
            match bot.download(file).await {
                Ok(upload) => uploads.push(upload),
                Err(err) => problems.push(format!("{err:#}")),
            }
        }
        let text = crate::attachments::with_problems(&incoming.text, &problems);
        let approver: Arc<dyn Approver> = Arc::new(TelegramApprover {
            bot: bot.clone(),
            target: incoming.target.clone(),
            sender: sender.clone(),
            owner: actor.owner,
        });
        let turn = gateway.chat_unattended_with(actor, &session, &text, &uploads);
        let reply = reply_or_error(with_approver(approver, turn).await);
        if reply.is_empty() {
            return;
        }
        let split =
            crate::live::read(|c| c.telegram.split_replies).unwrap_or(bot.config.split_replies);
        for (index, part) in reply_parts(reply, split).iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(typing_pause(part)).await;
            }
            if let Err(err) = bot.send(&incoming.target, part).await {
                eprintln!("telegram: cannot reply in {session}: {err:#}");
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Me {
        Me {
            id: 99,
            username: "runo_bot".into(),
        }
    }

    #[test]
    fn sessions_round_trip() {
        for target in [Target::User(42), Target::Group(-1001234)] {
            assert_eq!(Target::from_session(&target.session()), Some(target));
        }
        assert_eq!(Target::from_session("qq:c2c:1"), None);
        assert_eq!(
            crate::chat::private_chat("telegram:42").as_deref(),
            Some("telegram:dm:42")
        );
        assert_eq!(
            crate::chat::private_sender("telegram:dm:42").as_deref(),
            Some("telegram:42")
        );
    }

    #[test]
    fn messages_parse_by_chat_and_mention() {
        let private = json!({"update_id": 1, "message": {
            "from": {"id": 42, "username": "Alice"}, "chat": {"id": 42, "type": "private"},
            "text": "你好"}});
        let got = parse_incoming(&private, &me()).unwrap();
        assert_eq!(got.target, Target::User(42));
        assert!(got.addressed);
        assert_eq!(got.text, "你好");

        let group = |text: &str, reply_to: Option<i64>| {
            let mut update = json!({"update_id": 2, "message": {
                "from": {"id": 42}, "chat": {"id": -100, "type": "supergroup"}, "text": text}});
            if let Some(id) = reply_to {
                update["message"]["reply_to_message"] = json!({"from": {"id": id}});
            }
            parse_incoming(&update, &me()).unwrap()
        };
        let mentioned = group("@runo_bot 早上好", None);
        assert!(mentioned.addressed);
        assert_eq!(mentioned.text, "早上好");
        assert!(!group("大家早上好", None).addressed);
        assert!(group("对", Some(99)).addressed);

        // Its own messages and other bots are not answered.
        let own = json!({"update_id": 3, "message": {
            "from": {"id": 99}, "chat": {"id": 42, "type": "private"}, "text": "hi"}});
        assert!(parse_incoming(&own, &me()).is_none());
    }

    #[test]
    fn photos_and_voice_come_as_files() {
        let update = json!({"update_id": 4, "message": {
            "from": {"id": 42}, "chat": {"id": 42, "type": "private"}, "caption": "看",
            "photo": [{"file_id": "small", "file_size": 10}, {"file_id": "big", "file_size": 900}],
            "voice": {"file_id": "v1", "mime_type": "audio/ogg", "file_size": 3000}}});
        let got = parse_incoming(&update, &me()).unwrap();
        assert_eq!(got.text, "看");
        assert_eq!(got.files[0].id, "big");
        assert_eq!(got.files[0].mime.as_deref(), Some("image/jpeg"));
        assert_eq!(got.files[1].name, "voice.ogg");
        assert_eq!(got.files[1].mime.as_deref(), Some("audio/ogg"));
    }

    #[test]
    fn the_allow_list_takes_ids_usernames_and_groups() {
        let bot = |allow: &[&str]| {
            TelegramBot::new(TelegramConfig {
                token: Some("t".into()),
                allow: allow.iter().map(|s| (*s).to_owned()).collect(),
                ..TelegramConfig::default()
            })
            .unwrap()
        };
        let update = json!({"update_id": 5, "message": {
            "from": {"id": 42, "username": "Alice"}, "chat": {"id": -100, "type": "group"},
            "text": "@runo_bot hi"}});
        let incoming = parse_incoming(&update, &me()).unwrap();
        assert!(bot(&[]).allowed(&incoming));
        assert!(bot(&["42"]).allowed(&incoming));
        assert!(bot(&["@alice"]).allowed(&incoming));
        assert!(bot(&["-100"]).allowed(&incoming));
        assert!(!bot(&["7"]).allowed(&incoming));
    }

    #[tokio::test]
    async fn sends_text_pictures_and_voice_through_the_bot_api() {
        use axum::{Router, body::Bytes, extract::Path, routing::post};
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
        let log = seen.clone();
        let app = Router::new().route(
            "/{bot}/{method}",
            post(
                move |Path((bot, method)): Path<(String, String)>, body: Bytes| {
                    let log = log.clone();
                    async move {
                        assert_eq!(bot, "bottok");
                        log.lock()
                            .unwrap()
                            .push((method, String::from_utf8_lossy(&body).into_owned()));
                        axum::Json(json!({"ok": true, "result": {}}))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bot = TelegramBot::new(TelegramConfig {
            token: Some("tok".into()),
            api_url: format!("http://{addr}"),
            proxy: false,
            ..TelegramConfig::default()
        })
        .unwrap();
        bot.notify("telegram:dm:42", "晚安").await.unwrap();
        let approver = TelegramApprover {
            bot: bot.clone(),
            target: Target::User(42),
            sender: "telegram:42".into(),
            owner: true,
        };
        assert!(
            approver
                .send_voice("audio/mpeg", b"ID3voice")
                .await
                .unwrap()
        );
        assert!(approver.send_image("image/png", b"\x89PNG").await.unwrap());
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "sendMessage");
        assert!(seen[0].1.contains("晚安") && seen[0].1.contains("42"));
        assert_eq!(seen[1].0, "sendVoice");
        assert!(seen[1].1.contains("name=\"voice\"") && seen[1].1.contains("ID3voice"));
        assert_eq!(seen[2].0, "sendPhoto");
    }
}
