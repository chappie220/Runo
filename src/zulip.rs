//! Zulip bot: long-polls the event queue, sends through the REST API.
//!
//! Direct messages, group direct messages and channel (stream) topics that
//! @-mention the bot each map to one session: `zulip:dm:<email>`,
//! `zulip:group:<user ids>` and `zulip:stream:<stream id>:<topic>`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{Model, Tools};
use crate::attachments::Upload;
use crate::chat::{Approvals, Seen, reply_or_error, reply_parts, split, typing_pause};
use crate::config::ZulipConfig;
use crate::gateway::{Gateway, Notifier};
use crate::tools::{Approver, with_approver};

/// Under Zulip's default 10,000-character message limit.
const CHUNK_CHARS: usize = 9000;
/// Files taken from one message.
const MAX_FILES: usize = 10;
/// How long one poll may wait; Zulip answers within about 90 seconds.
const POLL_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A one-to-one direct message, by the person's email.
    User(String),
    /// A group direct message, by its members' user ids (not the bot's).
    Group(Vec<u64>),
    /// A channel topic.
    Stream { id: u64, topic: String },
}

impl Target {
    pub fn session(&self) -> String {
        match self {
            Target::User(email) => format!("zulip:dm:{email}"),
            Target::Group(ids) => {
                let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
                format!("zulip:group:{}", ids.join(","))
            }
            Target::Stream { id, topic } => format!("zulip:stream:{id}:{topic}"),
        }
    }

    pub fn from_session(session: &str) -> Option<Self> {
        if let Some(email) = session.strip_prefix("zulip:dm:") {
            return (!email.is_empty()).then(|| Target::User(email.to_owned()));
        }
        if let Some(ids) = session.strip_prefix("zulip:group:") {
            let ids: Vec<u64> = ids
                .split(',')
                .map(|id| id.parse().ok())
                .collect::<Option<_>>()?;
            return (!ids.is_empty()).then_some(Target::Group(ids));
        }
        let (id, topic) = session.strip_prefix("zulip:stream:")?.split_once(':')?;
        Some(Target::Stream {
            id: id.parse().ok()?,
            topic: topic.to_owned(),
        })
    }

    /// The form fields that address a message here.
    fn address(&self) -> Vec<(&'static str, String)> {
        match self {
            Target::User(email) => vec![
                ("type", "direct".into()),
                ("to", json!([email]).to_string()),
            ],
            Target::Group(ids) => vec![("type", "direct".into()), ("to", json!(ids).to_string())],
            Target::Stream { id, topic } => vec![
                ("type", "stream".into()),
                ("to", id.to_string()),
                ("topic", topic.clone()),
            ],
        }
    }
}

pub struct ZulipBot {
    http: reqwest::Client,
    config: ZulipConfig,
    key: String,
    seen: Seen,
    approvals: Approvals,
}

/// The bot's own account.
#[derive(Debug, Clone)]
struct Me {
    id: u64,
    name: String,
}

/// An incoming message the agent may answer.
#[derive(Debug, Clone, PartialEq)]
struct Incoming {
    id: u64,
    target: Target,
    /// Lowercased email.
    sender: String,
    /// The channel's name, for `zulip.allow`.
    stream: Option<String>,
    text: String,
    /// Uploads linked in the message, as `/user_uploads/…` paths and names.
    files: Vec<(String, String)>,
    /// Whether the bot was @-mentioned; only direct messages to it alone
    /// are answered without.
    mentioned: bool,
}

/// `pairs` as an `application/x-www-form-urlencoded` body.
fn form(pairs: &[(&str, String)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("static URL");
    url.query_pairs_mut()
        .extend_pairs(pairs.iter().map(|(k, v)| (*k, v.as_str())));
    url.query().unwrap_or("").to_owned()
}

impl ZulipBot {
    pub fn new(config: ZulipConfig) -> Result<Arc<Self>> {
        if config.site.trim().is_empty() {
            bail!("zulip.site is empty");
        }
        reqwest::Url::parse(config.site.trim()).context("zulip.site is not a URL")?;
        if config.email.trim().is_empty() {
            bail!("zulip.email is empty");
        }
        let key = config
            .api_key()
            .context("no Zulip API key: set ZULIP_API_KEY or zulip.api_key")?;
        let http = crate::net::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Arc::new(Self {
            http,
            config,
            key,
            seen: Seen::default(),
            approvals: Approvals::default(),
        }))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.site.trim().trim_end_matches('/'))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .basic_auth(self.config.email.trim(), Some(&self.key))
    }

    /// Sends a request and returns its successful JSON body.
    async fn call(&self, request: reqwest::RequestBuilder, path: &str) -> Result<Value> {
        let response = request
            .send()
            .await
            .with_context(|| format!("Zulip API {path} failed"))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() || body["result"] != "success" {
            bail!(ApiError {
                path: path.to_owned(),
                status: status.as_u16(),
                code: body["code"].as_str().unwrap_or("").to_owned(),
                message: body["msg"]
                    .as_str()
                    .map_or_else(|| body.to_string(), str::to_owned),
            });
        }
        Ok(body)
    }

    async fn post(&self, path: &str, pairs: &[(&str, String)]) -> Result<Value> {
        let request = self
            .request(reqwest::Method::POST, path)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form(pairs));
        self.call(request, path).await
    }

    async fn me(&self) -> Result<Me> {
        let path = "/api/v1/users/me";
        let body = self
            .call(self.request(reqwest::Method::GET, path), path)
            .await?;
        Ok(Me {
            id: body["user_id"]
                .as_u64()
                .context("Zulip did not say who the bot is")?,
            name: body["full_name"].as_str().unwrap_or("").to_owned(),
        })
    }

    /// Logs in: `doctor`'s proof that the site, email and key work.
    pub async fn check(&self) -> Result<String> {
        let me = self.me().await?;
        Ok(format!("logged in as {}", me.name))
    }

    /// Sends `text`, in as many messages as it needs.
    pub async fn send(&self, target: &Target, text: &str) -> Result<()> {
        for chunk in split(text, CHUNK_CHARS) {
            let mut pairs = target.address();
            pairs.push(("content", chunk));
            self.post("/api/v1/messages", &pairs).await?;
        }
        Ok(())
    }

    /// Downloads an upload linked in a message, up to the attachment limit.
    async fn download(&self, path: &str, name: &str) -> Result<Upload> {
        let mut response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("cannot download {name}"))?;
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            data.extend_from_slice(&chunk);
            if data.len() > crate::attachments::MAX_FILE_BYTES {
                bail!("{name} is larger than the attachment limit");
            }
        }
        Ok(Upload {
            name: name.to_owned(),
            mime: None,
            data,
        })
    }

    fn allowed(&self, incoming: &Incoming) -> bool {
        let allow = &self.config.allow;
        allow.is_empty()
            || allow
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&incoming.sender))
            || incoming
                .stream
                .as_ref()
                .is_some_and(|s| allow.iter().any(|a| a.trim_start_matches('#') == s))
    }
}

/// A refused API call, keeping Zulip's error code.
#[derive(Debug)]
struct ApiError {
    path: String,
    status: u16,
    code: String,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Zulip API {} returned HTTP {}: {}",
            self.path, self.status, self.message
        )
    }
}

impl std::error::Error for ApiError {}

/// `content` without mentions of the bot (`@**Name**`, `@**Name|12**`,
/// silent `@_**Name**`).
fn without_mention(content: &str, me: &Me) -> String {
    let mut text = content.to_owned();
    for silent in ["@_**", "@**"] {
        for name in [format!("{}|{}", me.name, me.id), me.name.clone()] {
            text = text.replace(&format!("{silent}{name}**"), "");
        }
    }
    text.trim().to_owned()
}

/// Uploads linked as `[name](/user_uploads/…)`.
fn uploads(content: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (start, _) in content.match_indices("](/user_uploads/") {
        let path_start = start + 2;
        let Some(len) = content[path_start..].find(')') else {
            continue;
        };
        let path = &content[path_start..path_start + len];
        let name = content[..start]
            .rfind('[')
            .map(|open| &content[open + 1..start])
            .filter(|n| !n.is_empty() && !n.contains(']'))
            .map_or_else(
                || path.rsplit('/').next().unwrap_or("zulip-file").to_owned(),
                str::to_owned,
            );
        if !path.contains("..") && !found.iter().any(|(p, _)| p == path) {
            found.push((path.to_owned(), name));
        }
    }
    found.truncate(MAX_FILES);
    found
}

/// A `message` event as a message, unless the bot sent it.
fn parse_incoming(event: &Value, me: &Me) -> Option<Incoming> {
    let message = &event["message"];
    let id = message["id"].as_u64()?;
    if message["sender_id"].as_u64()? == me.id {
        return None;
    }
    let sender = message["sender_email"].as_str()?.to_lowercase();
    let flags: Vec<&str> = event["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let content = message["content"].as_str().unwrap_or("");
    let (target, stream, alone) = match message["type"].as_str()? {
        "private" | "direct" => {
            let mut others: Vec<u64> = message["display_recipient"]
                .as_array()?
                .iter()
                .filter_map(|r| r["id"].as_u64())
                .filter(|id| *id != me.id)
                .collect();
            others.sort_unstable();
            if others.len() <= 1 {
                (Target::User(sender.clone()), None, true)
            } else {
                (Target::Group(others), None, false)
            }
        }
        "stream" | "channel" => {
            let target = Target::Stream {
                id: message["stream_id"].as_u64()?,
                topic: message["subject"].as_str().unwrap_or("").to_owned(),
            };
            let name = message["display_recipient"].as_str().map(str::to_owned);
            (target, name, false)
        }
        _ => return None,
    };
    let files = uploads(content);
    let text = without_mention(content, me);
    if text.is_empty() && files.is_empty() {
        return None;
    }
    Some(Incoming {
        id,
        target,
        sender,
        stream,
        text,
        files,
        mentioned: alone || flags.contains(&"mentioned"),
    })
}

/// Answers a Zulip turn's approvals: the owner is asked in the
/// conversation and replies yes or no; anyone else is declined unasked.
struct ZulipApprover {
    bot: Arc<ZulipBot>,
    target: Target,
    sender: String,
    owner: bool,
}

#[async_trait]
impl Approver for ZulipApprover {
    async fn approve(&self, tool: &str, summary: &str) -> bool {
        let session = self.target.session();
        if !self.owner {
            eprintln!(
                "zulip: declined {tool} for {} in {session} (not an owner)",
                self.sender
            );
            return false;
        }
        let (_, answer) = self.bot.approvals.open(&session, &self.sender);
        let shown = summary.replace("```", "'''");
        let prompt = crate::i18n::chat::APPROVAL_ASK.with(&[tool, &format!("```\n{shown}\n```")]);
        let asked = self.bot.send(&self.target, &prompt).await;
        self.bot.approvals.wait(&session, asked, answer).await
    }
}

#[async_trait]
impl Notifier for ZulipBot {
    fn handles(&self, session: &str) -> bool {
        Target::from_session(session).is_some()
    }

    async fn notify(&self, session: &str, text: &str) -> Result<()> {
        let target = Target::from_session(session).context("not a Zulip session")?;
        self.send(&target, text).await
    }
}

/// Keeps polling for the life of the process.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<ZulipBot>,
    gateway: Arc<Gateway<M, T>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        if let Err(err) = poll(&bot, &gateway).await {
            eprintln!("zulip: event queue ended: {err:#}");
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Registers an event queue and reads it until it fails or expires.
async fn poll<M: Model + 'static, T: Tools + 'static>(
    bot: &Arc<ZulipBot>,
    gateway: &Arc<Gateway<M, T>>,
) -> Result<()> {
    let me = bot.me().await?;
    let queue = bot
        .post(
            "/api/v1/register",
            &[
                ("event_types", json!(["message"]).to_string()),
                ("apply_markdown", "false".into()),
            ],
        )
        .await?;
    let queue_id = queue["queue_id"]
        .as_str()
        .context("Zulip gave no queue id")?
        .to_owned();
    let mut last = queue["last_event_id"].as_i64().unwrap_or(-1);
    eprintln!("zulip: connected as {}", me.name);
    loop {
        let path = "/api/v1/events";
        let query = form(&[
            ("queue_id", queue_id.clone()),
            ("last_event_id", last.to_string()),
        ]);
        let request = bot
            .request(reqwest::Method::GET, &format!("{path}?{query}"))
            .timeout(POLL_TIMEOUT);
        let body = match bot.call(request, path).await {
            Ok(body) => body,
            Err(err)
                if err
                    .downcast_ref::<ApiError>()
                    .is_some_and(|e| e.code == "BAD_EVENT_QUEUE_ID") =>
            {
                // Queues expire after ten idle minutes; start a new one.
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        for event in body["events"].as_array().into_iter().flatten() {
            if let Some(id) = event["id"].as_i64() {
                last = last.max(id);
            }
            if event["type"] == "message"
                && let Some(incoming) = parse_incoming(event, &me)
            {
                handle(bot.clone(), gateway.clone(), incoming);
            }
        }
    }
}

fn handle<M: Model + 'static, T: Tools + 'static>(
    bot: Arc<ZulipBot>,
    gateway: Arc<Gateway<M, T>>,
    incoming: Incoming,
) {
    if !bot.seen.first(&incoming.id.to_string()) {
        return;
    }
    let session = incoming.target.session();
    if !bot.allowed(&incoming) {
        if incoming.mentioned {
            eprintln!(
                "zulip: ignored {session} from {} (not in zulip.allow)",
                incoming.sender
            );
        }
        return;
    }
    if bot
        .approvals
        .answer(&session, &incoming.sender, &incoming.text)
    {
        eprintln!(
            "zulip: approval answered by {} in {session}",
            incoming.sender
        );
        return;
    }
    if !incoming.mentioned {
        return;
    }
    eprintln!("zulip: message from {} in {session}", incoming.sender);
    tokio::spawn(async move {
        let actor = gateway.actor(&format!("zulip:{}", incoming.sender), &session);
        let mut uploads = Vec::new();
        let mut problems = Vec::new();
        for (path, name) in &incoming.files {
            match bot.download(path, name).await {
                Ok(upload) => uploads.push(upload),
                Err(err) => problems.push(format!("{err:#}")),
            }
        }
        let text = crate::attachments::with_problems(&incoming.text, &problems);
        let approver: Arc<dyn Approver> = Arc::new(ZulipApprover {
            bot: bot.clone(),
            target: incoming.target.clone(),
            sender: incoming.sender.clone(),
            owner: actor.owner,
        });
        let turn = gateway.chat_unattended_with(actor, &session, &text, &uploads);
        let reply = reply_or_error(with_approver(approver, turn).await);
        // A message that joined a running turn is answered when it goes in.
        if reply.is_empty() {
            return;
        }
        let split =
            crate::live::read(|c| c.zulip.split_replies).unwrap_or(bot.config.split_replies);
        for (index, part) in reply_parts(reply, split).iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(typing_pause(part)).await;
            }
            if let Err(err) = bot.send(&incoming.target, part).await {
                eprintln!("zulip: cannot reply in {session}: {err:#}");
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
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn me() -> Me {
        Me {
            id: 9,
            name: "Claw".into(),
        }
    }

    #[test]
    fn sessions_round_trip() {
        for target in [
            Target::User("me@example.org".into()),
            Target::Group(vec![3, 5]),
            Target::Stream {
                id: 4,
                topic: "plans: 2026".into(),
            },
        ] {
            assert_eq!(Target::from_session(&target.session()), Some(target));
        }
        assert_eq!(Target::from_session("zulip:group:"), None);
        assert_eq!(Target::from_session("zulip:stream:x:y"), None);
        assert_eq!(Target::from_session("zulip:dm:"), None);
    }

    #[test]
    fn form_bodies_are_encoded() {
        assert_eq!(
            form(&[("to", "[\"a@b\"]".into()), ("content", "你好 & bye".into())]),
            "to=%5B%22a%40b%22%5D&content=%E4%BD%A0%E5%A5%BD+%26+bye"
        );
    }

    fn event(kind: &str, content: &str, recipients: Value, flags: Value) -> Value {
        json!({
            "type": "message", "id": 1, "flags": flags,
            "message": {
                "id": 100, "type": kind, "sender_id": 1, "sender_email": "Boss@Example.org",
                "content": content, "display_recipient": recipients,
                "stream_id": 4, "subject": "lunch",
            }
        })
    }

    #[test]
    fn messages_parse_by_kind() {
        let dm = event(
            "private",
            "hi [a.pdf](/user_uploads/2/ab/a.pdf)",
            json!([{"id": 1}, {"id": 9}]),
            json!([]),
        );
        let incoming = parse_incoming(&dm, &me()).unwrap();
        assert_eq!(incoming.target, Target::User("boss@example.org".into()));
        assert!(incoming.mentioned);
        assert_eq!(
            incoming.files,
            [("/user_uploads/2/ab/a.pdf".to_owned(), "a.pdf".to_owned())]
        );

        let group = event(
            "private",
            "hey all",
            json!([{"id": 1}, {"id": 9}, {"id": 3}]),
            json!([]),
        );
        let incoming = parse_incoming(&group, &me()).unwrap();
        assert_eq!(incoming.target, Target::Group(vec![1, 3]));
        assert!(!incoming.mentioned);

        let stream = event(
            "stream",
            "@**Claw|9** 吃什么",
            json!("food"),
            json!(["mentioned"]),
        );
        let incoming = parse_incoming(&stream, &me()).unwrap();
        assert_eq!(
            incoming.target,
            Target::Stream {
                id: 4,
                topic: "lunch".into()
            }
        );
        assert_eq!(incoming.text, "吃什么");
        assert_eq!(incoming.stream.as_deref(), Some("food"));
        assert!(incoming.mentioned);

        let mut own = dm.clone();
        own["message"]["sender_id"] = json!(9);
        assert_eq!(parse_incoming(&own, &me()), None);
        assert_eq!(
            parse_incoming(
                &event("stream", "@**Claw**", json!("x"), json!(["mentioned"])),
                &me()
            ),
            None
        );
    }

    #[test]
    fn upload_links_are_found() {
        assert_eq!(
            uploads(
                "[x](/user_uploads/1/a/x.png) and [](/user_uploads/1/b/y.txt) [z](/user_uploads/../etc)"
            ),
            [
                ("/user_uploads/1/a/x.png".to_owned(), "x".to_owned()),
                ("/user_uploads/1/b/y.txt".to_owned(), "y.txt".to_owned()),
            ]
        );
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

    type Sent = Arc<Mutex<Vec<HashMap<String, String>>>>;

    /// Fake Zulip server: one registered queue that hands out `batches` of
    /// events, then expires, as Zulip does when a queue is idle.
    async fn fake_zulip(sent: Sent, batches: Vec<Vec<Value>>) -> String {
        use axum::extract::{Query, State};
        use axum::http::HeaderMap;
        use axum::routing::{get, post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        fn authorized(headers: &HeaderMap) -> bool {
            use base64::Engine;
            let expected = base64::engine::general_purpose::STANDARD.encode("bot@example.org:k");
            headers
                .get("authorization")
                .is_some_and(|v| v.to_str().unwrap() == format!("Basic {expected}"))
        }
        let batches = Arc::new(Mutex::new(batches.into_iter()));
        let app = axum::Router::new()
            .route(
                "/api/v1/users/me",
                get(|headers: HeaderMap| async move {
                    assert!(authorized(&headers));
                    axum::Json(json!({"result": "success", "user_id": 9, "full_name": "Claw"}))
                }),
            )
            .route(
                "/api/v1/register",
                post(|headers: HeaderMap, body: String| async move {
                    assert!(authorized(&headers));
                    assert!(body.contains("event_types=%5B%22message%22%5D"), "{body}");
                    axum::Json(json!({"result": "success", "queue_id": "q1", "last_event_id": -1}))
                }),
            )
            .route(
                "/api/v1/events",
                get(move |Query(query): Query<HashMap<String, String>>| {
                    let batches = batches.clone();
                    async move {
                        assert_eq!(query["queue_id"], "q1");
                        let next = batches.lock().unwrap().next();
                        match next {
                            Some(events) => (
                                axum::http::StatusCode::OK,
                                axum::Json(json!({"result": "success", "events": events})),
                            ),
                            None => {
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(json!({"result": "error", "code": "BAD_EVENT_QUEUE_ID", "msg": "Bad event queue id"})),
                                )
                            }
                        }
                    }
                }),
            )
            .route(
                "/api/v1/messages",
                post(|State(sent): State<Sent>, body: String| async move {
                    let fields: HashMap<String, String> =
                        reqwest::Url::parse(&format!("http://x/?{body}"))
                            .unwrap()
                            .query_pairs()
                            .into_owned()
                            .collect();
                    sent.lock().unwrap().push(fields);
                    axum::Json(json!({"result": "success", "id": 1}))
                }),
            )
            .with_state(sent);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[tokio::test]
    async fn answers_direct_messages_mentions_and_owner_approvals() {
        let sent: Sent = Arc::default();
        let message = |n: u64, kind: &str, from: u64, email: &str, content: &str, flags: Value| {
            json!({"type": "message", "id": n, "flags": flags, "message": {
                "id": 100 + n, "type": kind, "sender_id": from, "sender_email": email,
                "content": content, "stream_id": 4, "subject": "lunch",
                "display_recipient": if kind == "stream" { json!("food") }
                    else { json!([{"id": from}, {"id": 9}]) },
            }})
        };
        let batches = vec![
            vec![
                json!({"type": "heartbeat", "id": 0}),
                message(1, "private", 1, "boss@example.org", "你好", json!([])),
                message(1, "private", 1, "boss@example.org", "你好", json!([])),
                message(2, "stream", 3, "amy@example.org", "随便聊", json!([])),
                message(
                    3,
                    "stream",
                    3,
                    "amy@example.org",
                    "@**Claw** 在吗",
                    json!(["mentioned"]),
                ),
                message(4, "private", 1, "boss@example.org", "run it", json!([])),
            ],
            vec![],
        ];
        let base = fake_zulip(sent.clone(), batches).await;
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
            toml::from_str(r#"owners = ["zulip:boss@example.org"]"#).unwrap();
        let gateway = Gateway::new(agent, None, access);
        let bot = ZulipBot::new(ZulipConfig {
            enabled: true,
            site: base.clone(),
            email: "bot@example.org".into(),
            api_key: Some("k".into()),
            split_replies: false,
            ..ZulipConfig::default()
        })
        .unwrap();
        tokio::spawn(run(bot.clone(), gateway));
        for _ in 0..100 {
            if sent
                .lock()
                .unwrap()
                .iter()
                .any(|m| m["content"].contains("shell"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The answer comes from the owner in the same conversation.
        let me = me();
        let yes = parse_incoming(
            &json!({"type": "message", "id": 5, "flags": [], "message": {
                "id": 105, "type": "private", "sender_id": 1, "sender_email": "boss@example.org",
                "content": "是", "display_recipient": [{"id": 1}, {"id": me.id}]}}),
            &me,
        )
        .unwrap();
        assert!(
            bot.approvals
                .answer(&yes.target.session(), &yes.sender, &yes.text)
        );
        for _ in 0..100 {
            if sent
                .lock()
                .unwrap()
                .iter()
                .any(|m| m["content"].contains("allowed=true"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let sent = sent.lock().unwrap().clone();
        let to = |m: &HashMap<String, String>| m.get("to").cloned().unwrap_or_default();
        let replies: Vec<(String, String)> =
            sent.iter().map(|m| (to(m), m["content"].clone())).collect();
        assert!(
            replies.contains(&("[\"boss@example.org\"]".into(), "回声:你好".into())),
            "{replies:?}"
        );
        assert!(
            replies.contains(&("4".into(), "回声:在吗".into())),
            "{replies:?}"
        );
        assert!(
            replies.iter().any(|(_, c)| c == "回声:allowed=true"),
            "{replies:?}"
        );
        assert!(
            !replies.iter().any(|(_, c)| c.contains("随便聊")),
            "{replies:?}"
        );
        assert_eq!(replies.iter().filter(|(_, c)| c == "回声:你好").count(), 1);
        let topic = sent.iter().find(|m| m["content"] == "回声:在吗").unwrap();
        assert_eq!(
            (topic["type"].as_str(), topic["topic"].as_str()),
            ("stream", "lunch")
        );
    }
}
