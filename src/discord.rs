//! Discord bot: receives messages over the Gateway (a WebSocket), answers
//! through the REST API.
//!
//! Direct messages are `discord:dm:<user id>`; server channels are
//! `discord:channel:<channel id>`, answered when the bot is @-mentioned or
//! replied to. The bot needs the Message Content intent (Developer Portal
//! › Bot). Pictures and voice messages go out as attachments.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::agent::{Model, Tools};
use crate::attachments::Upload;
use crate::chat::{Approvals, Seen, reply_or_error, reply_parts, split, typing_pause};
use crate::gateway::{Gateway, Notifier};
use crate::tools::{Approver, with_approver};

const API: &str = "https://discord.com/api/v10";
/// Discord's message limit is 2000 characters.
const CHUNK_CHARS: usize = 1900;
/// Guild messages, direct messages, and their content.
const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
/// Largest attachment taken from a message.
const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;

/// `[discord]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub enabled: bool,
    /// Through `network.proxy`, which Discord often needs.
    pub proxy: bool,
    /// The bot token; prefer `DISCORD_BOT_TOKEN`.
    pub token: Option<String>,
    /// REST API origin; for tests.
    pub api_url: String,
    /// User ids and channel ids that may talk to the bot; empty allows
    /// everyone who can reach it.
    pub allow: Vec<String>,
    /// As `qq.split_replies`.
    pub split_replies: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            proxy: true,
            token: None,
            api_url: API.into(),
            allow: Vec::new(),
            split_replies: true,
        }
    }
}

impl DiscordConfig {
    pub fn token(&self) -> Option<String> {
        std::env::var("DISCORD_BOT_TOKEN")
            .ok()
            .or_else(|| self.token.clone())
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A direct message, by the person's user id.
    User(String),
    /// A server channel (or thread), by its id.
    Channel(String),
}

impl Target {
    pub fn session(&self) -> String {
        match self {
            Target::User(id) => format!("discord:dm:{id}"),
            Target::Channel(id) => format!("discord:channel:{id}"),
        }
    }

    pub fn from_session(session: &str) -> Option<Self> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if let Some(id) = session.strip_prefix("discord:dm:") {
            return digits(id).then(|| Target::User(id.to_owned()));
        }
        let id = session.strip_prefix("discord:channel:")?;
        digits(id).then(|| Target::Channel(id.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Attachment {
    url: String,
    name: String,
    mime: Option<String>,
    size: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Incoming {
    id: String,
    target: Target,
    /// The channel the message came in, where the reply goes.
    channel: String,
    sender: String,
    text: String,
    files: Vec<Attachment>,
    /// A direct message, or @-mentioned or replied to in a server.
    addressed: bool,
}

pub struct DiscordBot {
    http: reqwest::Client,
    config: DiscordConfig,
    token: String,
    seen: Seen,
    approvals: Approvals,
}

impl DiscordBot {
    pub fn new(config: DiscordConfig) -> Result<Arc<Self>> {
        let token = config
            .token()
            .context("discord.token is not set (or DISCORD_BOT_TOKEN)")?;
        let http = crate::net::builder_for(config.proxy)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(60))
            .build()
            .context("cannot build HTTP client for Discord")?;
        Ok(Arc::new(Self {
            http,
            config,
            token,
            seen: Seen::default(),
            approvals: Approvals::default(),
        }))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(
                method,
                format!("{}{path}", self.config.api_url.trim_end_matches('/')),
            )
            .header("Authorization", format!("Bot {}", self.token))
            .header(
                "User-Agent",
                "DiscordBot (https://github.com/chappie220/Runo, 0.5)",
            )
    }

    async fn call(&self, request: reqwest::RequestBuilder, path: &str) -> Result<Value> {
        let response = request
            .send()
            .await
            .with_context(|| format!("Discord API {path} failed"))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body["message"].as_str().unwrap_or("no answer");
            bail!("Discord API {path} returned HTTP {status}: {message}");
        }
        Ok(body)
    }

    /// Who the token belongs to, for `doctor`.
    pub async fn check(&self) -> Result<String> {
        let me = self
            .call(
                self.request(reqwest::Method::GET, "/users/@me"),
                "/users/@me",
            )
            .await?;
        Ok(me["username"].as_str().unwrap_or("bot").to_owned())
    }

    /// The channel messages to `target` go to: a DM channel is opened for a
    /// user (Discord returns the same one each time).
    async fn channel(&self, target: &Target) -> Result<String> {
        match target {
            Target::Channel(id) => Ok(id.clone()),
            Target::User(id) => {
                let path = "/users/@me/channels";
                let dm = self
                    .call(
                        self.request(reqwest::Method::POST, path)
                            .json(&json!({ "recipient_id": id })),
                        path,
                    )
                    .await?;
                dm["id"]
                    .as_str()
                    .map(str::to_owned)
                    .context("Discord opened no DM channel")
            }
        }
    }

    async fn send_to(&self, channel: &str, text: &str) -> Result<()> {
        let path = format!("/channels/{channel}/messages");
        for chunk in split(text, CHUNK_CHARS) {
            self.call(
                self.request(reqwest::Method::POST, &path)
                    .json(&json!({ "content": chunk, "allowed_mentions": {"parse": []} })),
                &path,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn send(&self, target: &Target, text: &str) -> Result<()> {
        let channel = self.channel(target).await?;
        self.send_to(&channel, text).await
    }

    async fn send_file(&self, channel: &str, name: &str, mime: &str, data: &[u8]) -> Result<()> {
        let path = format!("/channels/{channel}/messages");
        let payload = json!({ "attachments": [{"id": 0, "filename": name}] }).to_string();
        let (kind, body) = crate::net::multipart(&[
            ("payload_json", crate::net::Part::Text(&payload)),
            ("files[0]", crate::net::Part::File { name, mime, data }),
        ]);
        self.call(
            self.request(reqwest::Method::POST, &path)
                .header(reqwest::header::CONTENT_TYPE, kind)
                .body(body),
            &path,
        )
        .await
        .map(|_| ())
    }

    async fn download(&self, file: &Attachment) -> Result<Upload> {
        if file.size > MAX_DOWNLOAD {
            bail!("{} is larger than the attachment limit", file.name);
        }
        let data = self
            .http
            .get(&file.url)
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
            || allow
                .iter()
                .any(|a| a.trim() == incoming.sender || a.trim() == incoming.channel)
    }
}

/// A `MESSAGE_CREATE` as a message the bot may answer, unless a bot sent it.
fn parse_incoming(message: &Value, me: &str) -> Option<Incoming> {
    let author = &message["author"];
    let sender = author["id"].as_str()?.to_owned();
    if sender == me || author["bot"] == true {
        return None;
    }
    let channel = message["channel_id"].as_str()?.to_owned();
    let content = message["content"].as_str().unwrap_or("");
    let (target, addressed) = if message["guild_id"].is_null() {
        (Target::User(sender.clone()), true)
    } else {
        let mentioned = message["mentions"]
            .as_array()
            .is_some_and(|m| m.iter().any(|u| u["id"].as_str() == Some(me)));
        let replied = message["referenced_message"]["author"]["id"].as_str() == Some(me);
        (Target::Channel(channel.clone()), mentioned || replied)
    };
    let text = content
        .replace(&format!("<@{me}>"), "")
        .replace(&format!("<@!{me}>"), "")
        .trim()
        .to_owned();
    let files: Vec<Attachment> = message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(Attachment {
                url: a["url"].as_str()?.to_owned(),
                name: a["filename"].as_str().unwrap_or("file").to_owned(),
                mime: a["content_type"].as_str().map(str::to_owned),
                size: a["size"].as_u64().unwrap_or(0),
            })
        })
        .collect();
    if text.is_empty() && files.is_empty() {
        return None;
    }
    Some(Incoming {
        id: message["id"].as_str()?.to_owned(),
        target,
        channel,
        sender,
        text,
        files,
        addressed,
    })
}

/// Answers a Discord turn's approvals: the owner is asked in the chat and
/// replies yes or no; anyone else is declined unasked.
struct DiscordApprover {
    bot: Arc<DiscordBot>,
    channel: String,
    session: String,
    sender: String,
    owner: bool,
}

#[async_trait]
impl Approver for DiscordApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        if !self.owner {
            eprintln!(
                "discord: declined {tool} for {} in {} (not an owner)",
                self.sender, self.session
            );
            return false;
        }
        let (_, answer) = self.bot.approvals.open(&self.session, &self.sender);
        let shown = summary.replace("```", "'''");
        let prompt = crate::i18n::chat::APPROVAL_ASK.with(&[tool, &format!("```\n{shown}\n```")]);
        let asked = self.bot.send_to(&self.channel, &prompt).await;
        self.bot.approvals.wait(&self.session, asked, answer).await
    }

    async fn send_image(&self, mime: &str, data: &[u8]) -> Result<bool> {
        let name = format!("picture.{}", mime.rsplit('/').next().unwrap_or("png"));
        self.bot.send_file(&self.channel, &name, mime, data).await?;
        Ok(true)
    }

    fn plays_voice(&self) -> bool {
        true
    }

    async fn send_voice(&self, mime: &str, data: &[u8]) -> Result<bool> {
        self.bot
            .send_file(&self.channel, "voice.mp3", mime, data)
            .await?;
        Ok(true)
    }
}

#[async_trait]
impl Notifier for DiscordBot {
    fn handles(&self, session: &str) -> bool {
        Target::from_session(session).is_some()
    }

    async fn notify(&self, session: &str, text: &str) -> Result<()> {
        let target = Target::from_session(session).context("not a Discord session")?;
        self.send(&target, text).await
    }
}

/// Keeps the bot connected for the life of the process.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<DiscordBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match connect(&bot, &gateway).await {
            Ok(()) => eprintln!("discord: gateway asked to reconnect"),
            Err(err) => eprintln!("discord: connection ended: {err:#}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn connect<M: Model + 'static, T: Tools + 'static>(
    bot: &Arc<DiscordBot>,
    gateway: &Arc<Gateway<M, T>>,
) -> Result<()> {
    let url = bot
        .call(
            bot.request(reqwest::Method::GET, "/gateway/bot"),
            "/gateway/bot",
        )
        .await?["url"]
        .as_str()
        .context("Discord gave no gateway url")?
        .to_owned();
    let url = format!("{url}/?v=10&encoding=json");
    let socket = crate::net::websocket(&url, bot.config.proxy)
        .await
        .with_context(|| format!("cannot connect to {url}"))?;
    let (mut sink, mut stream) = socket.split();
    let hello = next_payload(&mut stream).await?;
    if hello["op"] != 10 {
        bail!("expected Hello, got {hello}");
    }
    let interval = Duration::from_millis(
        hello["d"]["heartbeat_interval"]
            .as_u64()
            .unwrap_or(41_250)
            .max(1_000),
    );
    let identify = json!({ "op": 2, "d": {
        "token": bot.token, "intents": INTENTS,
        "properties": {"os": "linux", "browser": "runo", "device": "runo"},
    }});
    sink.send(Message::Text(identify.to_string().into()))
        .await?;
    let (out, mut outbox) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(payload) = outbox.recv().await {
            if sink
                .send(Message::Text(payload.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let seq = Arc::new(AtomicI64::new(-1));
    let heartbeat_seq = seq.clone();
    let heartbeat_out = out.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            let last = heartbeat_seq.load(Ordering::Relaxed);
            let d = if last >= 0 { json!(last) } else { Value::Null };
            if heartbeat_out.send(json!({ "op": 1, "d": d })).is_err() {
                break;
            }
        }
    });
    let mut me = String::new();
    let result = loop {
        let payload = match next_payload(&mut stream).await {
            Ok(payload) => payload,
            Err(err) => break Err(err),
        };
        if let Some(s) = payload["s"].as_i64() {
            seq.store(s, Ordering::Relaxed);
        }
        match payload["op"].as_i64() {
            Some(0) => match payload["t"].as_str().unwrap_or("") {
                "READY" => {
                    me = payload["d"]["user"]["id"].as_str().unwrap_or("").to_owned();
                    eprintln!(
                        "discord: connected as {}",
                        payload["d"]["user"]["username"].as_str().unwrap_or("bot")
                    );
                }
                "MESSAGE_CREATE" => {
                    if let Some(incoming) = parse_incoming(&payload["d"], &me) {
                        handle(bot.clone(), gateway.clone(), incoming);
                    }
                }
                _ => {}
            },
            // The gateway asks for a heartbeat now.
            Some(1) => {
                let last = seq.load(Ordering::Relaxed);
                let _ = out.send(
                    json!({ "op": 1, "d": if last >= 0 { json!(last) } else { Value::Null } }),
                );
            }
            Some(7) => break Ok(()),
            Some(9) => break Err(anyhow::anyhow!("session invalidated; identifying again")),
            _ => {}
        }
    };
    heartbeat.abort();
    drop(out);
    writer.abort();
    result
}

async fn next_payload<S>(stream: &mut S) -> Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream
            .next()
            .await
            .context("Discord gateway closed the connection")??
        {
            Message::Text(text) => {
                return serde_json::from_str(&text).context("malformed Discord gateway payload");
            }
            Message::Close(frame) => bail!("Discord gateway closed: {frame:?}"),
            _ => continue,
        }
    }
}

fn handle<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<DiscordBot>,
    gateway: Arc<Gateway<M, T>>,
    incoming: Incoming,
) {
    if !bot.seen.first(&incoming.id) {
        return;
    }
    let session = incoming.target.session();
    let sender = format!("discord:{}", incoming.sender);
    if !bot.allowed(&incoming) {
        if incoming.addressed {
            eprintln!("discord: ignored {session} from {sender} (not in discord.allow)");
        }
        return;
    }
    if bot.approvals.answer(&session, &sender, &incoming.text) {
        eprintln!("discord: approval answered by {sender} in {session}");
        return;
    }
    if !incoming.addressed {
        return;
    }
    eprintln!("discord: message from {sender} in {session}");
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
        let approver: Arc<dyn Approver> = Arc::new(DiscordApprover {
            bot: bot.clone(),
            channel: incoming.channel.clone(),
            session: session.clone(),
            sender: sender.clone(),
            owner: actor.owner,
        });
        let turn = gateway.chat_unattended_with(actor, &session, &text, &uploads);
        let reply = reply_or_error(with_approver(approver, turn).await);
        if reply.is_empty() {
            return;
        }
        let split =
            crate::live::read(|c| c.discord.split_replies).unwrap_or(bot.config.split_replies);
        for (index, part) in reply_parts(reply, split).iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(typing_pause(part)).await;
            }
            if let Err(err) = bot.send_to(&incoming.channel, part).await {
                eprintln!("discord: cannot reply in {session}: {err:#}");
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_round_trip() {
        for target in [Target::User("42".into()), Target::Channel("7".into())] {
            assert_eq!(Target::from_session(&target.session()), Some(target));
        }
        assert_eq!(Target::from_session("discord:dm:x"), None);
        assert_eq!(
            crate::chat::private_chat("discord:42").as_deref(),
            Some("discord:dm:42")
        );
    }

    #[test]
    fn messages_parse_by_place_and_mention() {
        let dm = json!({"id": "1", "channel_id": "500", "author": {"id": "42"}, "content": "你好"});
        let got = parse_incoming(&dm, "99").unwrap();
        assert_eq!(got.target, Target::User("42".into()));
        assert_eq!(got.channel, "500");
        assert!(got.addressed);

        let guild = |content: &str, mentions: Value| {
            json!({"id": "2", "channel_id": "7", "guild_id": "g", "author": {"id": "42"},
                   "content": content, "mentions": mentions})
        };
        let mentioned =
            parse_incoming(&guild("<@99> 早上好", json!([{"id": "99"}])), "99").unwrap();
        assert_eq!(mentioned.target, Target::Channel("7".into()));
        assert!(mentioned.addressed);
        assert_eq!(mentioned.text, "早上好");
        assert!(
            !parse_incoming(&guild("大家好", json!([])), "99")
                .unwrap()
                .addressed
        );

        let bot = json!({"id": "3", "channel_id": "7", "author": {"id": "5", "bot": true}, "content": "x"});
        assert!(parse_incoming(&bot, "99").is_none());

        let file = json!({"id": "4", "channel_id": "500", "author": {"id": "42"}, "content": "",
            "attachments": [{"url": "https://cdn/x.ogg", "filename": "voice-message.ogg",
                             "content_type": "audio/ogg", "size": 100}]});
        let got = parse_incoming(&file, "99").unwrap();
        assert_eq!(got.files[0].mime.as_deref(), Some("audio/ogg"));
    }

    #[tokio::test]
    async fn direct_messages_open_a_dm_channel_and_files_are_attached() {
        use axum::{Json, Router, body::Bytes, extract::Path, routing::post};
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
        let (log1, log2) = (seen.clone(), seen.clone());
        let app = Router::new()
            .route(
                "/users/@me/channels",
                post(move |Json(body): Json<Value>| {
                    let log = log1.clone();
                    async move {
                        log.lock().unwrap().push(("dm".into(), body.to_string()));
                        Json(json!({"id": "500"}))
                    }
                }),
            )
            .route(
                "/channels/{id}/messages",
                post(move |Path(id): Path<String>, body: Bytes| {
                    let log = log2.clone();
                    async move {
                        log.lock()
                            .unwrap()
                            .push((id, String::from_utf8_lossy(&body).into_owned()));
                        Json(json!({"id": "1"}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bot = DiscordBot::new(DiscordConfig {
            token: Some("tok".into()),
            api_url: format!("http://{addr}"),
            proxy: false,
            ..DiscordConfig::default()
        })
        .unwrap();
        bot.notify("discord:dm:42", "晚安").await.unwrap();
        let approver = DiscordApprover {
            bot: bot.clone(),
            channel: "7".into(),
            session: "discord:channel:7".into(),
            sender: "discord:42".into(),
            owner: true,
        };
        assert!(
            approver
                .send_voice("audio/mpeg", b"ID3voice")
                .await
                .unwrap()
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "dm");
        assert!(seen[0].1.contains("\"recipient_id\":\"42\""));
        assert_eq!(seen[1].0, "500");
        assert!(seen[1].1.contains("晚安"));
        assert_eq!(seen[2].0, "7");
        assert!(seen[2].1.contains("files[0]") && seen[2].1.contains("voice.mp3"));
    }
}
