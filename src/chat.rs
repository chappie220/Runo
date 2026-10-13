//! What the chat-app channels (QQ, Feishu, Zulip) share: how replies are
//! cut into messages, how an owner answers an approval, which sessions
//! belong to which channel.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::gateway::APPROVAL_TIMEOUT;

/// Chat apps whose private chats are `<channel>:<dm>:<id>` and whose
/// senders are `<channel>:<id>`.
const CHANNELS: &[(&str, &str)] = &[
    ("qq", "c2c"),
    ("feishu", "dm"),
    ("zulip", "dm"),
    ("telegram", "dm"),
    ("discord", "dm"),
];

/// Longest reply that is split into several messages; longer ones are
/// answers to read, not chat.
const CHAT_CHARS: usize = 600;
/// Messages one chatty reply becomes at most.
const CHAT_PARTS: usize = 3;
const SEEN_CAPACITY: usize = 1000;

/// Sessions owned by an external channel rather than the terminal or Web UI.
pub fn is_channel_session(session: &str) -> bool {
    session.starts_with("mail:") || is_chat_app(session)
}

/// Sessions in a chat app, where people text rather than write.
pub fn is_chat_app(session: &str) -> bool {
    CHANNELS
        .iter()
        .any(|(channel, _)| session.starts_with(&format!("{channel}:")))
}

/// The private chat with `owner` (`qq:<openid>`, `feishu:<open_id>`,
/// `zulip:<email>`), if they are on a chat app.
pub fn private_chat(owner: &str) -> Option<String> {
    let (channel, id) = owner.split_once(':')?;
    let (_, dm) = CHANNELS.iter().find(|(c, _)| *c == channel)?;
    // Zulip sessions use the email as it arrives, lowercased.
    let id = if channel == "zulip" {
        id.trim().to_lowercase()
    } else {
        id.trim().to_owned()
    };
    (!id.is_empty()).then(|| format!("{channel}:{dm}:{id}"))
}

/// Who writes in a private chat: the inverse of `private_chat`.
pub fn private_sender(session: &str) -> Option<String> {
    CHANNELS.iter().find_map(|(channel, dm)| {
        session
            .strip_prefix(&format!("{channel}:{dm}:"))
            .filter(|id| !id.is_empty())
            .map(|id| format!("{channel}:{id}"))
    })
}

/// Splits on character boundaries, preferring line breaks.
pub fn split(text: &str, max_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return vec![crate::i18n::chat::EMPTY.now().into()];
    }
    let mut chunks = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    while rest.len() > max_chars {
        let window = &rest[..max_chars];
        let cut = window
            .iter()
            .rposition(|c| *c == '\n')
            .filter(|i| *i > max_chars / 2)
            .map_or(max_chars, |i| i + 1);
        chunks.push(rest.drain(..cut).collect::<String>().trim_end().to_owned());
    }
    chunks.push(rest.into_iter().collect());
    chunks
}

/// A short chatty reply as the messages it is sent in, one per paragraph
/// up to `CHAT_PARTS` (the last takes the rest). Anything with code, a
/// table or more length than chat stays one message.
pub fn chat_parts(text: &str) -> Vec<String> {
    let text = text.trim();
    let structured = text.contains("```") || text.lines().any(|l| l.trim_start().starts_with('|'));
    if structured || text.chars().count() > CHAT_CHARS {
        return vec![text.to_owned()];
    }
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if paragraphs.len() < 2 {
        return vec![text.to_owned()];
    }
    let mut parts: Vec<String> = paragraphs
        .iter()
        .take(CHAT_PARTS - 1)
        .map(|p| (*p).to_owned())
        .collect();
    let rest = &paragraphs[parts.len()..];
    if !rest.is_empty() {
        parts.push(rest.join("\n\n"));
    }
    parts
}

/// A reply as it goes out: split into chatty parts when `split` is on.
pub fn reply_parts(reply: String, split: bool) -> Vec<String> {
    if split {
        chat_parts(&reply)
    } else {
        vec![reply]
    }
}

/// The pause before sending `next`, as if typing it.
pub fn typing_pause(next: &str) -> Duration {
    let chars = next.chars().count() as u64;
    Duration::from_millis((500 + 40 * chars).min(2500))
}

/// A reply to an approval request: whether it allows, and the request
/// number a button adds (`批准 #3`); typed replies carry none.
pub fn parse_answer(text: &str) -> Option<(bool, Option<u64>)> {
    let text = text.trim();
    let (word, id) = match text.rsplit_once('#') {
        Some((word, id)) => (word, Some(id.trim().parse().ok()?)),
        None => (text, None),
    };
    let word = word
        .trim()
        .trim_end_matches(['.', '!', '。', '！'])
        .to_lowercase();
    let allow = match word.as_str() {
        "y" | "yes" | "ok" | "allow" | "是" | "好" | "可以" | "同意" | "允许" | "批准" => {
            true
        }
        "n" | "no" | "deny" | "decline" | "否" | "不" | "不行" | "不同意" | "拒绝" | "不允许" => {
            false
        }
        _ => return None,
    };
    Some((allow, id))
}

/// Recently seen event ids: chat platforms may deliver an event twice.
#[derive(Default)]
pub struct Seen(Mutex<(HashSet<String>, VecDeque<String>)>);

impl Seen {
    /// True the first time `id` comes by.
    pub fn first(&self, id: &str) -> bool {
        let mut seen = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !seen.0.insert(id.to_owned()) {
            return false;
        }
        seen.1.push_back(id.to_owned());
        if seen.1.len() > SEEN_CAPACITY
            && let Some(old) = seen.1.pop_front()
        {
            seen.0.remove(&old);
        }
        true
    }
}

/// Approvals waiting for an owner's yes or no, by session. A session runs
/// one turn at a time, and a turn calls its tools one by one, so one per
/// session is enough.
pub struct Approvals {
    waiting: Mutex<HashMap<String, Pending>>,
    next: AtomicU64,
}

/// An approval request waiting for its answer.
struct Pending {
    /// Numbers the request, so a stale button cannot answer a newer one.
    id: u64,
    /// The only sender whose answer counts.
    sender: String,
    answer: oneshot::Sender<bool>,
}

impl Default for Approvals {
    fn default() -> Self {
        Self {
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }
}

impl Approvals {
    /// Opens a request in `session` that only `sender` may answer: its
    /// number, and where the answer arrives.
    pub fn open(&self, session: &str, sender: &str) -> (u64, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            session.to_owned(),
            Pending {
                id,
                sender: sender.to_owned(),
                answer: tx,
            },
        );
        (id, rx)
    }

    /// Waits for the answer to a request `asked` (or not), then closes it
    /// so nobody may answer it later. Unanswered declines.
    pub async fn wait(
        &self,
        session: &str,
        asked: anyhow::Result<()>,
        answer: oneshot::Receiver<bool>,
    ) -> bool {
        let allowed = match asked {
            Ok(()) => matches!(
                tokio::time::timeout(APPROVAL_TIMEOUT, answer).await,
                Ok(Ok(true))
            ),
            Err(err) => {
                eprintln!("cannot ask for approval in {session}: {err:#}");
                false
            }
        };
        self.lock().remove(session);
        allowed
    }

    /// Hands a typed or button yes/no from `sender` to the request waiting
    /// in `session`. True when `text` was an answer the agent should not
    /// see: one that counted, or a button for a request no longer waiting.
    pub fn answer(&self, session: &str, sender: &str, text: &str) -> bool {
        let Some((allow, id)) = parse_answer(text) else {
            return false;
        };
        let mut waiting = self.lock();
        let matches = waiting
            .get(session)
            .is_some_and(|p| p.sender == sender && id.is_none_or(|id| id == p.id));
        if !matches {
            return id.is_some();
        }
        let pending = waiting.remove(session).expect("checked above");
        let _ = pending.answer.send(allow);
        true
    }

    /// Answers request `id` wherever it waits, for buttons that report
    /// their request rather than a message (Feishu cards). False when it
    /// is no longer waiting or `sender` may not answer it.
    pub fn answer_id(&self, id: u64, sender: &str, allow: bool) -> bool {
        let mut waiting = self.lock();
        let Some(session) = waiting
            .iter()
            .find(|(_, p)| p.id == id && p.sender == sender)
            .map(|(s, _)| s.clone())
        else {
            return false;
        };
        let pending = waiting.remove(&session).expect("found above");
        pending.answer.send(allow).is_ok()
    }

    /// Whether a request waits in `session`.
    #[cfg(test)]
    pub fn waiting(&self, session: &str) -> bool {
        self.lock().contains_key(session)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.waiting.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A tool summary cut to `max` characters for a card.
pub fn shorten(summary: &str, max: usize) -> String {
    let mut shown: String = summary.chars().take(max).collect();
    if shown.len() < summary.len() {
        shown.push('…');
    }
    shown
}

/// The text an unattended chat turn ends with: the reply, or the error.
pub fn reply_or_error(turn: anyhow::Result<String>) -> String {
    turn.unwrap_or_else(|err| crate::i18n::chat::FAILED.with(&[&format!("{err:#}")]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_chats_map_to_their_senders() {
        for (owner, session) in [
            ("qq:A", "qq:c2c:A"),
            ("feishu:ou_1", "feishu:dm:ou_1"),
            ("zulip:me@example.org", "zulip:dm:me@example.org"),
        ] {
            assert_eq!(private_chat(owner).as_deref(), Some(session));
            assert_eq!(private_sender(session).as_deref(), Some(owner));
        }
        assert_eq!(
            private_chat("zulip:Me@Example.org").as_deref(),
            Some("zulip:dm:me@example.org")
        );
        assert_eq!(private_chat("mail:me@example.org"), None);
        assert_eq!(private_chat("qq:"), None);
        for group in ["qq:group:G", "feishu:group:oc_1", "zulip:stream:3:hi"] {
            assert_eq!(private_sender(group), None);
            assert!(is_chat_app(group) && is_channel_session(group));
        }
        assert!(is_channel_session("mail:a@b") && !is_chat_app("mail:a@b"));
        assert!(!is_channel_session("main") && !is_channel_session("qqfriend"));
    }

    #[tokio::test]
    async fn approvals_take_only_their_senders_answer() {
        let approvals = Approvals::default();
        let (id, rx) = approvals.open("s", "boss");
        assert!(!approvals.answer("s", "eve", "yes"));
        assert!(!approvals.answer("s", "boss", "hello"));
        // A stale button is swallowed, not sent to the agent.
        assert!(approvals.answer("s", "boss", &format!("批准 #{}", id + 1)));
        assert!(approvals.answer("s", "boss", &format!("拒绝 #{id}")));
        assert!(!approvals.wait("s", Ok(()), rx).await);

        let (id, rx) = approvals.open("t", "boss");
        assert!(!approvals.answer_id(id, "eve", true));
        assert!(approvals.answer_id(id, "boss", true));
        assert!(!approvals.answer_id(id, "boss", true));
        assert!(approvals.wait("t", Ok(()), rx).await);
    }
}
