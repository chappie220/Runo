//! Feishu (Lark) bot: events over the long connection, so no public
//! address is needed, and OpenAPI messages out.
//!
//! Private chats and group @-mentions each map to one session:
//! `feishu:dm:<open_id>` and `feishu:group:<chat_id>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::agent::{Model, Tools};
use crate::attachments::Upload;
use crate::chat::{Approvals, Seen, reply_or_error, reply_parts, shorten, split, typing_pause};
use crate::config::FeishuConfig;
use crate::gateway::{Gateway, Notifier};
use crate::tools::{Approver, with_approver};

/// Well under Feishu's 150 KB text and 30 KB rich-text limits.
const CHUNK_CHARS: usize = 4000;
/// Longest tool summary shown in an approval card.
const SUMMARY_CHARS: usize = 1000;
/// Files taken from one message.
const MAX_FILES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A private chat, by the person's open_id.
    User(String),
    /// A group, by its chat_id.
    Group(String),
}

impl Target {
    pub fn session(&self) -> String {
        match self {
            Target::User(id) => format!("feishu:dm:{id}"),
            Target::Group(id) => format!("feishu:group:{id}"),
        }
    }

    pub fn from_session(session: &str) -> Option<Self> {
        if let Some(id) = session.strip_prefix("feishu:dm:") {
            return Some(Target::User(id.to_owned()));
        }
        session
            .strip_prefix("feishu:group:")
            .map(|id| Target::Group(id.to_owned()))
    }

    /// `receive_id_type` and `receive_id` for sending here.
    fn receiver(&self) -> (&'static str, &str) {
        match self {
            Target::User(id) => ("open_id", id),
            Target::Group(id) => ("chat_id", id),
        }
    }
}

pub struct FeishuBot {
    http: reqwest::Client,
    config: FeishuConfig,
    secret: String,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
    /// The bot's own open_id, to tell when a group mentions it.
    me: Mutex<Option<String>>,
    seen: Seen,
    approvals: Approvals,
}

/// An incoming message the agent may answer.
#[derive(Debug, Clone, PartialEq)]
struct Incoming {
    id: String,
    target: Target,
    sender: String,
    text: String,
    files: Vec<FeishuFile>,
    /// Whether the bot was @-mentioned; groups are answered only then.
    mentioned: bool,
}

/// A file attached to a message, still on Feishu's servers.
#[derive(Debug, Clone, PartialEq)]
struct FeishuFile {
    key: String,
    /// `image` or `file`, as the resource API wants it.
    kind: &'static str,
    name: String,
    mime: Option<String>,
}

impl FeishuBot {
    pub fn new(config: FeishuConfig) -> Result<Arc<Self>> {
        if config.app_id.trim().is_empty() {
            bail!("feishu.app_id is empty");
        }
        let secret = config
            .app_secret()
            .context("no Feishu app secret: set FEISHU_APP_SECRET or feishu.app_secret")?;
        let http = crate::net::builder_for(config.proxy)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Arc::new(Self {
            http,
            config,
            secret,
            token: tokio::sync::Mutex::new(None),
            me: Mutex::new(None),
            seen: Seen::default(),
            approvals: Approvals::default(),
        }))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.api_base.trim_end_matches('/'))
    }

    /// Logs in and reads the bot's name: `doctor`'s proof that the app id,
    /// secret and bot capability work.
    pub async fn check(&self) -> Result<String> {
        let (_, name) = self.bot_info().await?;
        self.endpoint().await?;
        Ok(format!("logged in as {name}, long connection ready"))
    }

    /// Cached tenant access token, renewed a minute before it expires.
    async fn access_token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + Duration::from_secs(60) < *expires
        {
            return Ok(token.clone());
        }
        let body: Value = self
            .http
            .post(self.url("/open-apis/auth/v3/tenant_access_token/internal"))
            .json(&json!({ "app_id": self.config.app_id, "app_secret": self.secret }))
            .send()
            .await
            .context("Feishu access token request failed")?
            .json()
            .await
            .context("Feishu access token response is not JSON")?;
        let token = body["tenant_access_token"]
            .as_str()
            .filter(|_| body["code"] == 0)
            .with_context(|| format!("Feishu access token refused: {}", describe(&body)))?
            .to_owned();
        let ttl = body["expire"].as_u64().unwrap_or(7200);
        *cached = Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }

    async fn api(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let token = self.access_token().await?;
        let mut request = self.http.request(method, self.url(path)).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("Feishu API {path} failed"))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() || body["code"] != 0 {
            bail!(
                "Feishu API {path} returned HTTP {status}: {}",
                describe(&body)
            );
        }
        Ok(body)
    }

    /// The bot's open_id and name.
    async fn bot_info(&self) -> Result<(String, String)> {
        let body = self
            .api(reqwest::Method::GET, "/open-apis/bot/v3/info", None)
            .await?;
        let open_id = body["bot"]["open_id"]
            .as_str()
            .context("Feishu bot info has no open_id; is the bot capability on?")?
            .to_owned();
        let name = body["bot"]["app_name"].as_str().unwrap_or("bot").to_owned();
        *self.me.lock().unwrap_or_else(|p| p.into_inner()) = Some(open_id.clone());
        Ok((open_id, name))
    }

    /// The long connection's address and how often to ping it.
    async fn endpoint(&self) -> Result<(String, Duration)> {
        let body: Value = self
            .http
            .post(self.url("/callback/ws/endpoint"))
            .header("locale", "zh")
            .json(&json!({ "AppID": self.config.app_id, "AppSecret": self.secret }))
            .send()
            .await
            .context("Feishu long connection request failed")?
            .json()
            .await
            .context("Feishu long connection response is not JSON")?;
        let url = body["data"]["URL"]
            .as_str()
            .filter(|_| body["code"] == 0)
            .with_context(|| {
                format!(
                    "Feishu refused the long connection ({}); subscribe to events with \
                     the long connection in the developer console",
                    describe(&body)
                )
            })?
            .to_owned();
        let ping = body["data"]["ClientConfig"]["PingInterval"]
            .as_u64()
            .unwrap_or(120)
            .clamp(5, 600);
        Ok((url, Duration::from_secs(ping)))
    }

    /// Sends one message: a reply to `reply_to` when given, which falls
    /// back to a new message if the original is gone.
    async fn post(
        &self,
        target: &Target,
        reply_to: Option<&str>,
        msg_type: &str,
        content: &Value,
    ) -> Result<()> {
        let content = content.to_string();
        if let Some(id) = reply_to {
            let path = format!("/open-apis/im/v1/messages/{id}/reply");
            let body = json!({ "msg_type": msg_type, "content": content });
            match self.api(reqwest::Method::POST, &path, Some(body)).await {
                Ok(_) => return Ok(()),
                Err(err) => eprintln!("feishu: reply failed ({err:#}); sending a new message"),
            }
        }
        let (kind, id) = target.receiver();
        let path = format!("/open-apis/im/v1/messages?receive_id_type={kind}");
        let body = json!({ "receive_id": id, "msg_type": msg_type, "content": content });
        self.api(reqwest::Method::POST, &path, Some(body)).await?;
        Ok(())
    }

    /// Sends `text` as rich text with markdown, or as plain text where
    /// that is refused, in as many messages as it needs.
    pub async fn send(&self, target: &Target, text: &str, reply_to: Option<&str>) -> Result<()> {
        let mut reply_to = reply_to;
        for chunk in split(text, CHUNK_CHARS) {
            let rich = json!({ "zh_cn": { "content": [[{ "tag": "md", "text": chunk }]] } });
            if let Err(err) = self.post(target, reply_to, "post", &rich).await {
                eprintln!("feishu: rich text refused ({err:#}); sending plain text");
                self.post(target, reply_to, "text", &json!({ "text": chunk }))
                    .await?;
            }
            reply_to = None;
        }
        Ok(())
    }

    /// Uploads a picture and sends it as an image message.
    async fn send_image(
        &self,
        target: &Target,
        mime: &str,
        data: &[u8],
        reply_to: Option<&str>,
    ) -> Result<()> {
        let token = self.access_token().await?;
        let name = format!("image.{}", mime.rsplit('/').next().unwrap_or("png"));
        let (kind, body) = crate::net::multipart(&[
            ("image_type", crate::net::Part::Text("message")),
            (
                "image",
                crate::net::Part::File {
                    name: &name,
                    mime,
                    data,
                },
            ),
        ]);
        let path = "/open-apis/im/v1/images";
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, kind)
            .body(body)
            .send()
            .await
            .with_context(|| format!("Feishu API {path} failed"))?;
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        let key = value["data"]["image_key"]
            .as_str()
            .filter(|_| status.is_success() && value["code"] == 0)
            .with_context(|| {
                format!(
                    "Feishu API {path} returned HTTP {status}: {}",
                    describe(&value)
                )
            })?;
        self.post(target, reply_to, "image", &json!({ "image_key": key }))
            .await
    }

    /// Asks with Allow/Decline buttons, or in text where cards are refused.
    async fn ask_approval(
        &self,
        target: &Target,
        id: u64,
        tool: &str,
        summary: &str,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let card = approval_card(id, tool, summary);
        if let Err(err) = self.post(target, reply_to, "interactive", &card).await {
            eprintln!("feishu: approval card refused ({err:#}); asking in text");
            let prompt = crate::i18n::chat::APPROVAL_ASK.with(&[tool, summary]);
            self.send(target, &prompt, reply_to).await?;
        }
        Ok(())
    }

    /// Downloads a message's file, up to the attachment size limit.
    async fn download(&self, message: &str, file: &FeishuFile) -> Result<Upload> {
        let token = self.access_token().await?;
        let path = format!(
            "/open-apis/im/v1/messages/{message}/resources/{}?type={}",
            file.key, file.kind
        );
        let mut response = self
            .http
            .get(self.url(&path))
            .bearer_auth(token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("cannot download {}", file.name))?;
        let mime = file.mime.clone().or_else(|| {
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .filter(|m| !m.starts_with("application/octet-stream"))
                .map(str::to_owned)
        });
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            data.extend_from_slice(&chunk);
            if data.len() > crate::attachments::MAX_FILE_BYTES {
                bail!("{} is larger than the attachment limit", file.name);
            }
        }
        Ok(Upload {
            name: file.name.clone(),
            mime,
            data,
        })
    }

    fn allowed(&self, incoming: &Incoming) -> bool {
        let allow = &self.config.allow;
        allow.is_empty()
            || allow.contains(&incoming.sender)
            || matches!(&incoming.target, Target::Group(id) if allow.contains(id))
    }

    fn me(&self) -> Option<String> {
        self.me.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

fn describe(body: &Value) -> String {
    body.get("msg")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map_or_else(
            || body.to_string(),
            |m| format!("{m} (code {})", body["code"]),
        )
}

/// A card with Allow/Decline buttons. A tap comes back as a
/// `card.action.trigger` event naming the request.
fn approval_card(id: u64, tool: &str, summary: &str) -> Value {
    use crate::i18n::chat::{APPROVAL_ALLOW, APPROVAL_CARD, APPROVAL_DECLINE};
    let shown = shorten(summary, SUMMARY_CHARS).replace("```", "'''");
    let button = |label: &str, kind: &str, allow: bool| {
        json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": label },
            "type": kind,
            "value": { "approval": id.to_string(), "allow": allow },
        })
    };
    json!({
        "config": { "wide_screen_mode": true },
        "elements": [
            { "tag": "markdown", "content": APPROVAL_CARD.with(&[tool, &shown]) },
            { "tag": "action", "actions": [
                button(APPROVAL_ALLOW.now(), "primary", true),
                button(APPROVAL_DECLINE.now(), "danger", false),
            ] },
        ],
    })
}

/// A tapped approval button: the request, who tapped, and the answer.
fn parse_card_action(event: &Value) -> Option<(u64, String, bool)> {
    let value = &event["action"]["value"];
    let id = match &value["approval"] {
        Value::String(id) => id.parse().ok()?,
        id => id.as_u64()?,
    };
    let allow = value["allow"].as_bool()?;
    // The new callback names the operator; the old one puts open_id on top.
    let sender = event["operator"]["open_id"]
        .as_str()
        .or_else(|| event["open_id"].as_str())?;
    Some((id, sender.to_owned(), allow))
}

/// The text of a message's content, with mentions of the bot removed and
/// other mentions as `@name`, and the files it carries.
fn parse_content(
    kind: &str,
    content: &Value,
    mentions: &[(String, String, bool)],
) -> (String, Vec<FeishuFile>) {
    let mention = |key: &str| {
        mentions
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, name, me)| {
                if *me {
                    String::new()
                } else {
                    format!("@{name}")
                }
            })
    };
    let mut files = Vec::new();
    let text = match kind {
        "text" => {
            let mut text = content["text"].as_str().unwrap_or("").to_owned();
            // Longest keys first, so @_user_1 does not eat into @_user_10.
            let mut keys: Vec<&(String, String, bool)> = mentions.iter().collect();
            keys.sort_by_key(|(k, _, _)| std::cmp::Reverse(k.len()));
            for (key, _, _) in keys {
                text = text.replace(key.as_str(), &mention(key).unwrap_or_default());
            }
            text
        }
        "post" => {
            // Received posts are flat; sent ones are keyed by language.
            let post = if content["content"].is_array() {
                content
            } else {
                content
                    .as_object()
                    .and_then(|o| o.values().find(|v| v["content"].is_array()))
                    .unwrap_or(content)
            };
            let mut lines = Vec::new();
            if let Some(title) = post["title"].as_str().filter(|t| !t.trim().is_empty()) {
                lines.push(title.to_owned());
            }
            for paragraph in post["content"].as_array().into_iter().flatten() {
                let mut line = String::new();
                for element in paragraph.as_array().into_iter().flatten() {
                    let text = element["text"].as_str().unwrap_or("");
                    match element["tag"].as_str().unwrap_or("") {
                        "text" | "md" => line.push_str(text),
                        "a" => match element["href"].as_str() {
                            Some(href) if href != text => {
                                line.push_str(&format!("{text} ({href})"))
                            }
                            _ => line.push_str(text),
                        },
                        "at" => {
                            let key = element["user_id"].as_str().unwrap_or("");
                            let name = element["user_name"].as_str().unwrap_or(key);
                            line.push_str(&mention(key).unwrap_or_else(|| format!("@{name}")));
                        }
                        "code_block" => {
                            let language = element["language"].as_str().unwrap_or("");
                            line.push_str(&format!("```{language}\n{text}\n```"));
                        }
                        "img" => {
                            if let Some(key) = element["image_key"].as_str() {
                                files.push(image(key));
                            }
                        }
                        "media" => {
                            if let Some(key) = element["file_key"].as_str() {
                                let name =
                                    element["file_name"].as_str().unwrap_or("feishu-video.mp4");
                                files.push(file(key, name, None));
                            }
                        }
                        _ => {}
                    }
                }
                lines.push(line);
            }
            lines.join("\n")
        }
        "image" => {
            if let Some(key) = content["image_key"].as_str() {
                files.push(image(key));
            }
            String::new()
        }
        "file" | "media" => {
            if let Some(key) = content["file_key"].as_str() {
                let name = content["file_name"].as_str().unwrap_or("feishu-file");
                files.push(file(key, name, None));
            }
            String::new()
        }
        "audio" => {
            if let Some(key) = content["file_key"].as_str() {
                files.push(file(key, "feishu-voice.opus", Some("audio/ogg")));
            }
            String::new()
        }
        _ => String::new(),
    };
    files.truncate(MAX_FILES);
    (text.trim().to_owned(), files)
}

fn image(key: &str) -> FeishuFile {
    FeishuFile {
        key: key.to_owned(),
        kind: "image",
        name: "feishu-image.png".into(),
        mime: None,
    }
}

fn file(key: &str, name: &str, mime: Option<&str>) -> FeishuFile {
    FeishuFile {
        key: key.to_owned(),
        kind: "file",
        name: name.to_owned(),
        mime: mime.map(str::to_owned),
    }
}

/// An `im.message.receive_v1` event as a message, unless another bot sent
/// it or it carries nothing to answer.
fn parse_incoming(event: &Value, me: Option<&str>) -> Option<Incoming> {
    if event["sender"]["sender_type"].as_str() == Some("app") {
        return None;
    }
    let sender = event["sender"]["sender_id"]["open_id"].as_str()?.to_owned();
    let message = &event["message"];
    let id = message["message_id"].as_str()?.to_owned();
    let mentions: Vec<(String, String, bool)> = message["mentions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let key = m["key"].as_str()?.to_owned();
            let name = m["name"].as_str().unwrap_or("").to_owned();
            let is_me = me.is_some_and(|me| m["id"]["open_id"].as_str() == Some(me));
            Some((key, name, is_me))
        })
        .collect();
    let mentioned = mentions.iter().any(|(_, _, me)| *me);
    let target = match message["chat_type"].as_str()? {
        "p2p" => Target::User(sender.clone()),
        "group" => Target::Group(message["chat_id"].as_str()?.to_owned()),
        _ => return None,
    };
    let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
    let kind = message["message_type"].as_str().unwrap_or("text");
    let (text, files) = parse_content(kind, &content, &mentions);
    if text.is_empty() && files.is_empty() {
        return None;
    }
    Some(Incoming {
        id,
        target,
        sender,
        text,
        files,
        mentioned,
    })
}

/// Answers a Feishu turn's approvals: the owner gets a card with buttons;
/// anyone else is declined without being asked.
struct FeishuApprover {
    bot: Arc<FeishuBot>,
    target: Target,
    sender: String,
    owner: bool,
    reply_to: Option<String>,
}

#[async_trait]
impl Approver for FeishuApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let session = self.target.session();
        if !self.owner {
            eprintln!(
                "feishu: declined {tool} for {} in {session} (not an owner)",
                self.sender
            );
            return false;
        }
        let (id, answer) = self.bot.approvals.open(&session, &self.sender);
        let asked = self
            .bot
            .ask_approval(&self.target, id, tool, summary, self.reply_to.as_deref())
            .await;
        self.bot.approvals.wait(&session, asked, answer).await
    }

    async fn send_image(&self, mime: &str, data: &[u8]) -> Result<bool> {
        self.bot
            .send_image(&self.target, mime, data, self.reply_to.as_deref())
            .await?;
        Ok(true)
    }
}

#[async_trait]
impl Notifier for FeishuBot {
    fn handles(&self, session: &str) -> bool {
        Target::from_session(session).is_some()
    }

    async fn notify(&self, session: &str, text: &str) -> Result<()> {
        let target = Target::from_session(session).context("not a Feishu session")?;
        self.send(&target, text, None).await
    }
}

/// The long connection's frame (`pbbp2.Frame`), in protobuf.
#[derive(Debug, Clone, Default, PartialEq)]
struct Frame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    /// 0: control (ping, pong), 1: data (events).
    method: i32,
    headers: Vec<(String, String)>,
    payload_encoding: Option<String>,
    payload_type: Option<String>,
    payload: Option<Vec<u8>>,
    log_id_new: Option<String>,
}

const METHOD_CONTROL: i32 = 0;
const METHOD_DATA: i32 = 1;

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_varint(out, field << 3);
    put_varint(out, value);
}

/// Reads protobuf fields as (number, varint or bytes).
fn fields(mut data: &[u8]) -> Result<Vec<(u64, Field<'_>)>> {
    fn varint(data: &mut &[u8]) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = data.split_first().context("truncated varint")?;
            *data = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        bail!("varint too long")
    }
    let mut out = Vec::new();
    while !data.is_empty() {
        let key = varint(&mut data)?;
        let field = match key & 7 {
            0 => Field::Number(varint(&mut data)?),
            2 => {
                let len = usize::try_from(varint(&mut data)?)?;
                if len > data.len() {
                    bail!("truncated field");
                }
                let (bytes, rest) = data.split_at(len);
                data = rest;
                Field::Bytes(bytes)
            }
            1 | 5 => {
                let len = if key & 7 == 1 { 8 } else { 4 };
                data = data.get(len..).context("truncated field")?;
                continue;
            }
            wire => bail!("unsupported wire type {wire}"),
        };
        out.push((key >> 3, field));
    }
    Ok(out)
}

enum Field<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_number(&mut out, 1, self.seq_id);
        put_number(&mut out, 2, self.log_id);
        // Negative int32s are sign-extended, as protobuf does.
        put_number(&mut out, 3, i64::from(self.service) as u64);
        put_number(&mut out, 4, i64::from(self.method) as u64);
        for (key, value) in &self.headers {
            let mut header = Vec::new();
            put_bytes(&mut header, 1, key.as_bytes());
            put_bytes(&mut header, 2, value.as_bytes());
            put_bytes(&mut out, 5, &header);
        }
        if let Some(encoding) = &self.payload_encoding {
            put_bytes(&mut out, 6, encoding.as_bytes());
        }
        if let Some(kind) = &self.payload_type {
            put_bytes(&mut out, 7, kind.as_bytes());
        }
        if let Some(payload) = &self.payload {
            put_bytes(&mut out, 8, payload);
        }
        if let Some(log_id) = &self.log_id_new {
            put_bytes(&mut out, 9, log_id.as_bytes());
        }
        out
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        let mut frame = Frame::default();
        for (number, field) in fields(data)? {
            match (number, field) {
                (1, Field::Number(n)) => frame.seq_id = n,
                (2, Field::Number(n)) => frame.log_id = n,
                (3, Field::Number(n)) => frame.service = n as i32,
                (4, Field::Number(n)) => frame.method = n as i32,
                (5, Field::Bytes(header)) => {
                    let (mut key, mut value) = (String::new(), String::new());
                    for (number, field) in fields(header)? {
                        match (number, field) {
                            (1, Field::Bytes(b)) => key = text(b),
                            (2, Field::Bytes(b)) => value = text(b),
                            _ => {}
                        }
                    }
                    frame.headers.push((key, value));
                }
                (6, Field::Bytes(b)) => frame.payload_encoding = Some(text(b)),
                (7, Field::Bytes(b)) => frame.payload_type = Some(text(b)),
                (8, Field::Bytes(b)) => frame.payload = Some(b.to_vec()),
                (9, Field::Bytes(b)) => frame.log_id_new = Some(text(b)),
                _ => {}
            }
        }
        Ok(frame)
    }

    fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn ping(service: i32) -> Self {
        Frame {
            service,
            method: METHOD_CONTROL,
            headers: vec![("type".into(), "ping".into())],
            ..Default::default()
        }
    }

    /// This frame sent back as the acknowledgement Feishu waits for,
    /// carrying `data` (a card callback's answer) if any.
    fn ack(&self, data: Option<&Value>) -> Self {
        let mut frame = self.clone();
        frame.headers.push(("biz_rt".into(), "0".into()));
        let data = data.map(|d| base64::engine::general_purpose::STANDARD.encode(d.to_string()));
        frame.payload = Some(
            json!({ "code": 200, "headers": {}, "data": data })
                .to_string()
                .into_bytes(),
        );
        frame
    }
}

/// Keeps the bot connected for the life of the process.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<FeishuBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        if let Err(err) = connect(&bot, &gateway).await {
            eprintln!("feishu: connection ended: {err:#}");
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn connect<M: Model + 'static, T: Tools + 'static>(
    bot: &Arc<FeishuBot>,
    gateway: &Arc<Gateway<M, T>>,
) -> Result<()> {
    let (_, name) = bot.bot_info().await?;
    let (url, ping) = bot.endpoint().await?;
    let service = reqwest::Url::parse(&url)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "service_id")
                .and_then(|(_, v)| v.parse().ok())
        })
        .unwrap_or(0);
    let socket = crate::net::websocket(&url, bot.config.proxy)
        .await
        .context("cannot open the Feishu long connection")?;
    eprintln!("feishu: connected as {name}");
    let (mut sink, mut stream) = socket.split();
    let (out, mut outbox) = mpsc::unbounded_channel::<Frame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbox.recv().await {
            if sink
                .send(Message::Binary(frame.encode().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let pinger = out.clone();
    let pings = tokio::spawn(async move {
        let mut tick = tokio::time::interval(ping);
        loop {
            tick.tick().await;
            if pinger.send(Frame::ping(service)).is_err() {
                break;
            }
        }
    });
    // Events too large for one frame arrive in parts.
    let mut parts: HashMap<String, Vec<Option<Vec<u8>>>> = HashMap::new();
    let result = loop {
        let message = match stream.next().await {
            Some(Ok(message)) => message,
            Some(Err(err)) => break Err(err.into()),
            None => break Err(anyhow::anyhow!("Feishu closed the long connection")),
        };
        let data = match message {
            Message::Binary(data) => data,
            Message::Close(frame) => break Err(anyhow::anyhow!("Feishu closed: {frame:?}")),
            _ => continue,
        };
        let frame = match Frame::decode(&data) {
            Ok(frame) => frame,
            Err(err) => {
                eprintln!("feishu: malformed frame: {err:#}");
                continue;
            }
        };
        if frame.method != METHOD_DATA {
            continue;
        }
        let Some(payload) = whole_payload(&frame, &mut parts) else {
            continue;
        };
        let answer = match serde_json::from_slice::<Value>(&payload) {
            Ok(event) => dispatch(bot, gateway, &event),
            Err(err) => {
                eprintln!("feishu: malformed event: {err}");
                None
            }
        };
        if out.send(frame.ack(answer.as_ref())).is_err() {
            break Err(anyhow::anyhow!("Feishu writer stopped"));
        }
    };
    pings.abort();
    drop(out);
    writer.abort();
    result
}

/// The frame's payload once all its parts have come.
fn whole_payload(
    frame: &Frame,
    parts: &mut HashMap<String, Vec<Option<Vec<u8>>>>,
) -> Option<Vec<u8>> {
    let payload = frame.payload.clone().unwrap_or_default();
    let sum: usize = frame
        .header("sum")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    if sum <= 1 {
        return Some(payload);
    }
    let seq: usize = frame.header("seq").and_then(|s| s.parse().ok())?;
    let id = frame.header("message_id")?.to_owned();
    if seq >= sum || sum > 64 {
        return None;
    }
    let slots = parts.entry(id.clone()).or_insert_with(|| vec![None; sum]);
    if slots.len() != sum {
        *slots = vec![None; sum];
    }
    slots[seq] = Some(payload);
    if slots.iter().any(Option::is_none) {
        return None;
    }
    let whole = parts.remove(&id)?.into_iter().flatten().flatten().collect();
    Some(whole)
}

/// Handles one event; what a card callback answers with, if anything.
fn dispatch<M: Model + 'static, T: Tools + 'static>(
    bot: &Arc<FeishuBot>,
    gateway: &Arc<Gateway<M, T>>,
    event: &Value,
) -> Option<Value> {
    use crate::i18n::chat::{APPROVAL_ALLOWED, APPROVAL_DECLINED, APPROVAL_GONE};
    let kind = event["header"]["event_type"].as_str().unwrap_or("");
    // The old card callback has no header and puts the action on top.
    let card = if kind == "card.action.trigger" {
        Some(&event["event"])
    } else if event["action"].is_object() {
        Some(event)
    } else {
        None
    };
    if let Some(card) = card {
        let (id, sender, allow) = parse_card_action(card)?;
        let (style, text) = if bot.approvals.answer_id(id, &sender, allow) {
            eprintln!("feishu: approval answered by {sender}");
            let text = if allow {
                APPROVAL_ALLOWED
            } else {
                APPROVAL_DECLINED
            };
            ("success", text.now())
        } else {
            ("warning", APPROVAL_GONE.now())
        };
        return Some(json!({ "toast": { "type": style, "content": text } }));
    }
    if kind == "im.message.receive_v1"
        && let Some(incoming) = parse_incoming(&event["event"], bot.me().as_deref())
    {
        handle(bot.clone(), gateway.clone(), incoming);
    }
    None
}

fn handle<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<FeishuBot>,
    gateway: Arc<Gateway<M, T>>,
    incoming: Incoming,
) {
    if !bot.seen.first(&incoming.id) {
        return;
    }
    let session = incoming.target.session();
    if !bot.allowed(&incoming) {
        eprintln!(
            "feishu: ignored {session} from {} (not in feishu.allow)",
            incoming.sender
        );
        return;
    }
    if bot
        .approvals
        .answer(&session, &incoming.sender, &incoming.text)
    {
        eprintln!(
            "feishu: approval answered by {} in {session}",
            incoming.sender
        );
        return;
    }
    if matches!(incoming.target, Target::Group(_)) && !incoming.mentioned {
        return;
    }
    eprintln!("feishu: message from {} in {session}", incoming.sender);
    tokio::spawn(async move {
        let actor = gateway.actor(&format!("feishu:{}", incoming.sender), &session);
        let mut uploads = Vec::new();
        let mut problems = Vec::new();
        for file in &incoming.files {
            match bot.download(&incoming.id, file).await {
                Ok(upload) => uploads.push(upload),
                Err(err) => problems.push(format!("{err:#}")),
            }
        }
        let text = crate::attachments::with_problems(&incoming.text, &problems);
        // Quoting keeps a group's threads apart; a private chat needs none.
        let reply_to = matches!(incoming.target, Target::Group(_)).then(|| incoming.id.clone());
        let approver: Arc<dyn Approver> = Arc::new(FeishuApprover {
            bot: bot.clone(),
            target: incoming.target.clone(),
            sender: incoming.sender.clone(),
            owner: actor.owner,
            reply_to: reply_to.clone(),
        });
        let turn = gateway.chat_unattended_with(actor, &session, &text, &uploads);
        let reply = reply_or_error(with_approver(approver, turn).await);
        // A message that joined a running turn is answered when it goes in.
        if reply.is_empty() {
            return;
        }
        let split =
            crate::live::read(|c| c.feishu.split_replies).unwrap_or(bot.config.split_replies);
        for (index, part) in reply_parts(reply, split).iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(typing_pause(part)).await;
            }
            let quote = if index == 0 {
                reply_to.as_deref()
            } else {
                None
            };
            if let Err(err) = bot.send(&incoming.target, part, quote).await {
                eprintln!("feishu: cannot reply in {session}: {err:#}");
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::config::AgentConfig;
    use crate::llm::{ChatMessage, Completion, FunctionCall, ToolCall, ToolSpec};
    use crate::store::Store;

    #[test]
    fn frames_round_trip_through_protobuf() {
        let frame = Frame {
            seq_id: 300,
            log_id: 1 << 40,
            service: 7,
            method: METHOD_DATA,
            headers: vec![("type".into(), "event".into()), ("sum".into(), "1".into())],
            payload_encoding: None,
            payload_type: Some("json".into()),
            payload: Some(b"{\"a\":1}".to_vec()),
            log_id_new: Some("x".into()),
        };
        assert_eq!(Frame::decode(&frame.encode()).unwrap(), frame);
        // Known bytes: field 1 = 150 is 08 96 01.
        let mut out = Vec::new();
        put_number(&mut out, 1, 150);
        assert_eq!(out, [0x08, 0x96, 0x01]);
        assert!(Frame::decode(&[0x0a, 0x05, 0x01]).is_err());
        let ack = frame.ack(Some(&json!({"toast": {"type": "success"}})));
        assert_eq!(ack.header("biz_rt"), Some("0"));
        let body: Value = serde_json::from_slice(ack.payload.as_ref().unwrap()).unwrap();
        assert_eq!(body["code"], 200);
        let data = base64::engine::general_purpose::STANDARD
            .decode(body["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&data).unwrap()["toast"]["type"],
            "success"
        );
    }

    #[test]
    fn split_events_are_joined() {
        let mut parts = HashMap::new();
        let part = |seq: usize, data: &str| Frame {
            method: METHOD_DATA,
            headers: vec![
                ("message_id".into(), "m".into()),
                ("sum".into(), "2".into()),
                ("seq".into(), seq.to_string()),
            ],
            payload: Some(data.as_bytes().to_vec()),
            ..Default::default()
        };
        assert_eq!(whole_payload(&part(1, "lo"), &mut parts), None);
        assert_eq!(
            whole_payload(&part(0, "hel"), &mut parts),
            Some(b"hello".to_vec())
        );
        assert!(parts.is_empty());
    }

    fn message(chat_type: &str, kind: &str, content: Value, mentions: Value) -> Value {
        json!({
            "sender": { "sender_id": { "open_id": "ou_boss" }, "sender_type": "user" },
            "message": {
                "message_id": "om_1", "chat_id": "oc_g", "chat_type": chat_type,
                "message_type": kind, "content": content.to_string(), "mentions": mentions,
            }
        })
    }

    #[test]
    fn messages_parse_with_mentions_and_files() {
        let mentions = json!([
            { "key": "@_user_1", "id": { "open_id": "ou_bot" }, "name": "Claw" },
            { "key": "@_user_10", "id": { "open_id": "ou_amy" }, "name": "Amy" },
        ]);
        let group = message(
            "group",
            "text",
            json!({"text": "@_user_1 问问 @_user_10"}),
            mentions,
        );
        let incoming = parse_incoming(&group, Some("ou_bot")).unwrap();
        assert_eq!(incoming.target, Target::Group("oc_g".into()));
        assert_eq!(incoming.text, "问问 @Amy");
        assert!(incoming.mentioned);
        assert!(!parse_incoming(&group, Some("ou_other")).unwrap().mentioned);

        let private = message(
            "p2p",
            "post",
            json!({"title": "", "content": [
                [{"tag": "text", "text": "看看"}, {"tag": "img", "image_key": "img_1"}],
                [{"tag": "a", "text": "这个", "href": "https://example.org"}],
            ]}),
            json!([]),
        );
        let incoming = parse_incoming(&private, Some("ou_bot")).unwrap();
        assert_eq!(incoming.target, Target::User("ou_boss".into()));
        assert_eq!(incoming.text, "看看\n这个 (https://example.org)");
        assert_eq!(incoming.files, [image("img_1")]);

        let voice = message("p2p", "audio", json!({"file_key": "f_1"}), json!([]));
        let incoming = parse_incoming(&voice, None).unwrap();
        assert_eq!(incoming.files[0].kind, "file");
        assert_eq!(incoming.files[0].mime.as_deref(), Some("audio/ogg"));

        let mut bot = private.clone();
        bot["sender"]["sender_type"] = json!("app");
        assert_eq!(parse_incoming(&bot, None), None);
        assert_eq!(
            parse_incoming(&message("p2p", "sticker", json!({}), json!([])), None),
            None
        );
    }

    #[test]
    fn card_buttons_name_their_request() {
        let card = approval_card(7, "shell", "ls ```x```");
        let actions = &card["elements"][1]["actions"];
        assert_eq!(actions[0]["value"], json!({"approval": "7", "allow": true}));
        assert_eq!(actions[1]["value"]["allow"], false);
        assert!(
            !card["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("```x")
        );
        let tapped = json!({
            "operator": { "open_id": "ou_boss" },
            "action": { "value": { "approval": "7", "allow": true }, "tag": "button" },
        });
        assert_eq!(
            parse_card_action(&tapped),
            Some((7, "ou_boss".to_owned(), true))
        );
        assert_eq!(
            parse_card_action(
                &json!({"open_id": "ou_x", "action": {"value": {"approval": 3, "allow": false}}})
            ),
            Some((3, "ou_x".to_owned(), false))
        );
        assert_eq!(parse_card_action(&json!({"action": {"value": {}}})), None);
    }

    #[test]
    fn sessions_round_trip() {
        for target in [Target::User("ou_1".into()), Target::Group("oc_1".into())] {
            assert_eq!(Target::from_session(&target.session()), Some(target));
        }
        assert_eq!(Target::from_session("qq:c2c:x"), None);
    }

    struct Echo;

    #[async_trait]
    impl Model for Echo {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: &[ToolSpec],
            _on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<Completion> {
            let last = messages
                .last()
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            // Asks for the shell once, when told to, to test approvals.
            if last.contains("run it") && !tools.is_empty() {
                return Ok(Completion {
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "shell".into(),
                            arguments: r#"{"command":"true"}"#.into(),
                        },
                    }],
                    ..Default::default()
                });
            }
            Ok(Completion {
                text: format!("回声:{last}"),
                ..Default::default()
            })
        }
    }

    struct AskTools;

    #[async_trait]
    impl Tools for AskTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec::function(
                "shell",
                "run",
                json!({"type": "object"}),
            )]
        }
        async fn call(&self, call: &ToolCall) -> String {
            let allowed = crate::tools::approve(&call.function.name, "true").await;
            format!("allowed={allowed}")
        }
    }

    type Sent = Arc<Mutex<Vec<(String, Value)>>>;
    type Acks = Arc<Mutex<Vec<Value>>>;

    /// Fake Feishu OpenAPI and long connection following the SDK's protocol.
    async fn fake_feishu(sent: Sent, acks: Acks, events: Vec<Value>) -> String {
        use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
        use axum::extract::{Path, Query, State};
        use axum::http::HeaderMap;
        use axum::routing::{get, post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let ws_url = base.replace("http://", "ws://") + "/ws?device_id=d&service_id=42";
        fn authorized(headers: &HeaderMap) -> bool {
            headers
                .get("authorization")
                .is_some_and(|v| v == "Bearer T1")
        }
        let events = Arc::new(events);
        let app = axum::Router::new()
            .route(
                "/open-apis/auth/v3/tenant_access_token/internal",
                post(|axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(body, json!({"app_id": "cli_1", "app_secret": "s"}));
                    axum::Json(json!({"code": 0, "tenant_access_token": "T1", "expire": 7200}))
                }),
            )
            .route(
                "/open-apis/bot/v3/info",
                get(|headers: HeaderMap| async move {
                    assert!(authorized(&headers));
                    axum::Json(json!({"code": 0, "bot": {"open_id": "ou_bot", "app_name": "Claw"}}))
                }),
            )
            .route(
                "/callback/ws/endpoint",
                post(move |axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(body, json!({"AppID": "cli_1", "AppSecret": "s"}));
                    axum::Json(json!({"code": 0, "data": {
                        "URL": ws_url, "ClientConfig": {"PingInterval": 5}}}))
                }),
            )
            .route(
                "/open-apis/im/v1/messages",
                post(
                    |State(sent): State<Sent>,
                     headers: HeaderMap,
                     Query(query): Query<HashMap<String, String>>,
                     axum::Json(body): axum::Json<Value>| async move {
                        assert!(authorized(&headers));
                        let to = format!(
                            "{}:{}",
                            query["receive_id_type"],
                            body["receive_id"].as_str().unwrap()
                        );
                        sent.lock().unwrap().push((to, body));
                        axum::Json(json!({"code": 0, "data": {}}))
                    },
                ),
            )
            .route(
                "/open-apis/im/v1/messages/{id}/reply",
                post(
                    |State(sent): State<Sent>,
                     Path(id): Path<String>,
                     axum::Json(body): axum::Json<Value>| async move {
                        sent.lock().unwrap().push((format!("reply:{id}"), body));
                        axum::Json(json!({"code": 0, "data": {}}))
                    },
                ),
            )
            .route(
                "/ws",
                get(move |upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |mut ws| async move {
                        for (n, event) in events.iter().enumerate() {
                            // A card tap must wait for the card it answers.
                            if event["header"]["event_type"] == "card.action.trigger" {
                                tokio::time::sleep(Duration::from_millis(700)).await;
                            }
                            let frame = Frame {
                                seq_id: n as u64,
                                service: 42,
                                method: METHOD_DATA,
                                headers: vec![
                                    ("type".into(), "event".into()),
                                    ("message_id".into(), format!("f{n}")),
                                    ("sum".into(), "1".into()),
                                    ("seq".into(), "0".into()),
                                ],
                                payload: Some(event.to_string().into_bytes()),
                                ..Default::default()
                            };
                            ws.send(WsMessage::Binary(frame.encode().into()))
                                .await
                                .unwrap();
                            // Acks come back in order, pings may come between.
                            loop {
                                let Some(Ok(WsMessage::Binary(data))) = ws.recv().await else {
                                    return;
                                };
                                let reply = Frame::decode(&data).unwrap();
                                if reply.method == METHOD_CONTROL {
                                    assert_eq!(reply.service, 42);
                                    continue;
                                }
                                assert_eq!(
                                    reply.header("message_id"),
                                    Some(format!("f{n}").as_str())
                                );
                                let body: Value =
                                    serde_json::from_slice(reply.payload.as_ref().unwrap())
                                        .unwrap();
                                acks.lock().unwrap().push(body);
                                break;
                            }
                        }
                        while ws.recv().await.is_some() {}
                    })
                }),
            )
            .with_state(sent);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    fn event(kind: &str, body: Value) -> Value {
        json!({"schema": "2.0", "header": {"event_type": kind, "event_id": "e"}, "event": body})
    }

    #[tokio::test]
    async fn answers_owners_with_cards_and_groups_only_when_mentioned() {
        let sent: Sent = Arc::default();
        let acks: Acks = Arc::default();
        let text = |chat: &str, id: &str, from: &str, text: &str, mentions: Value| {
            let mut m = message(chat, "text", json!({"text": text}), mentions);
            m["message"]["message_id"] = json!(id);
            m["sender"]["sender_id"]["open_id"] = json!(from);
            event("im.message.receive_v1", m)
        };
        let at_bot = json!([{ "key": "@_user_1", "id": { "open_id": "ou_bot" }, "name": "Claw" }]);
        let events = vec![
            text("p2p", "om_1", "ou_boss", "你好", json!([])),
            text("p2p", "om_1", "ou_boss", "你好", json!([])),
            text("group", "om_2", "ou_amy", "随便聊", json!([])),
            text("group", "om_3", "ou_amy", "@_user_1 在吗", at_bot),
            text("p2p", "om_4", "ou_boss", "run it", json!([])),
            event(
                "card.action.trigger",
                json!({"operator": {"open_id": "ou_boss"},
                       "action": {"value": {"approval": "1", "allow": true}}}),
            ),
        ];
        let base = fake_feishu(sent.clone(), acks.clone(), events).await;
        let agent = Arc::new(Agent {
            model: Echo,
            summarizer: None,
            vision: None,
            voice: None,
            tools: AskTools,
            store: Store::open_in_memory().unwrap(),
            config: AgentConfig {
                time_sense: false,
                ..AgentConfig::default()
            },
        });
        let access: crate::access::AccessConfig =
            toml::from_str(r#"owners = ["feishu:ou_boss"]"#).unwrap();
        let gateway = Gateway::new(agent, None, access);
        let bot = FeishuBot::new(FeishuConfig {
            enabled: true,
            app_id: "cli_1".into(),
            app_secret: Some("s".into()),
            api_base: base,
            split_replies: false,
            ..FeishuConfig::default()
        })
        .unwrap();
        tokio::spawn(run(bot, gateway));
        for _ in 0..100 {
            if sent.lock().unwrap().len() >= 4 && acks.lock().unwrap().len() >= 6 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let sent = sent.lock().unwrap().clone();
        let texts: Vec<(String, String)> = sent
            .iter()
            .map(|(to, body)| (to.clone(), body["msg_type"].as_str().unwrap().to_owned()))
            .collect();
        assert!(
            texts.contains(&("open_id:ou_boss".into(), "post".into())),
            "{sent:?}"
        );
        assert!(
            texts.contains(&("reply:om_3".into(), "post".into())),
            "{sent:?}"
        );
        assert!(
            texts.contains(&("open_id:ou_boss".into(), "interactive".into())),
            "{sent:?}"
        );
        let said: Vec<String> = sent
            .iter()
            .filter(|(_, b)| b["msg_type"] == "post")
            .map(|(_, b)| b["content"].as_str().unwrap().to_owned())
            .collect();
        assert!(said.iter().any(|s| s.contains("回声:你好")), "{said:?}");
        assert!(said.iter().any(|s| s.contains("回声:在吗")), "{said:?}");
        assert!(said.iter().any(|s| s.contains("allowed=true")), "{said:?}");
        assert!(!said.iter().any(|s| s.contains("随便聊")), "{said:?}");
        assert_eq!(said.iter().filter(|s| s.contains("回声:你好")).count(), 1);
        let acks = acks.lock().unwrap().clone();
        assert!(acks.iter().all(|a| a["code"] == 200), "{acks:?}");
        let toast = base64::engine::general_purpose::STANDARD
            .decode(acks[5]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&toast).unwrap()["toast"]["type"],
            "success"
        );
    }
}
