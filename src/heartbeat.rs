//! Heartbeat: every so often the agent gets a quiet turn in the owner's chat
//! to reach out on its own, and decides itself whether there is anything
//! worth saying. A turn it lets pass is erased, so silence leaves no trace.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

use crate::access::{AccessConfig, Actor};
use crate::agent::{Model, Tools};
use crate::gateway::Gateway;

/// How a heartbeat prompt begins, so it never counts as the person writing.
pub const PROMPT_PREFIX: &str = "[heartbeat]";
/// What the model answers when it has nothing to say.
pub const SILENT: &str = "NO_REPLY";
/// A heartbeat nobody answered blocks the next one for this long.
const UNANSWERED_FOR: i64 = 12 * 3600;
/// An answer to a heartbeat this soon counts as glad to hear from it.
const QUICK_ANSWER: i64 = 15 * 60;
/// How far back heartbeats and answers are weighed.
const LOOK_BACK: i64 = 7 * 86_400;
/// Someone who wrote this recently is in a conversation; no need to call.
const JUST_TALKED: i64 = 30 * 60;

/// Read by the model, so it stays in English.
const PROMPT: &str = "[heartbeat] This is not a message from the person. It is a quiet moment \
the program gives you every few hours to reach out on your own. Think about what you know of \
them (search your memories if it helps), what you talked about lately, the time of day and how \
long it has been since they last wrote. If there is something genuinely worth saying now (a \
follow-up on something they mentioned, a timely reminder, a warm word after a long silence, \
something you noticed that they would care about), write just that message: short and natural, \
the way a friend texts, in the language you two use. Do not mention this check-in or that you \
were prompted, and never repeat what you already said. Otherwise, or if you are unsure, reply \
with exactly NO_REPLY.";

/// `[heartbeat]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    /// The conversation it happens in: `qq:c2c:<openid>`, `mail:<address>`
    /// or a local session. Default: the private QQ chat of the first
    /// `qq:` owner in `access.owners`.
    pub session: Option<String>,
    /// Minutes between heartbeats.
    pub every_minutes: u64,
    /// Local times with no heartbeat, `HH:MM-HH:MM`; may cross midnight.
    pub quiet_hours: String,
    /// Messages it may start in one day.
    pub max_per_day: usize,
    /// Added to the prompt: what to keep an eye on, how often to speak up.
    pub prompt: Option<String>,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session: None,
            every_minutes: 120,
            quiet_hours: "23:00-08:00".into(),
            max_per_day: 3,
            prompt: None,
        }
    }
}

/// Local hours with no heartbeat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quiet {
    from: NaiveTime,
    to: NaiveTime,
}

impl Quiet {
    pub fn parse(text: &str) -> Result<Option<Self>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let (from, to) = text
            .split_once('-')
            .with_context(|| format!("heartbeat.quiet_hours {text:?} is not HH:MM-HH:MM"))?;
        let time = |t: &str| {
            NaiveTime::parse_from_str(t.trim(), "%H:%M")
                .with_context(|| format!("heartbeat.quiet_hours: {t:?} is not HH:MM"))
        };
        Ok(Some(Self {
            from: time(from)?,
            to: time(to)?,
        }))
    }

    pub fn contains(&self, at: NaiveTime) -> bool {
        let at = at.with_second(0).unwrap_or(at);
        if self.from <= self.to {
            self.from <= at && at < self.to
        } else {
            at >= self.from || at < self.to
        }
    }
}

/// What a heartbeat needs, checked at startup.
pub struct Heartbeat {
    pub session: String,
    pub actor: Actor,
    every: Duration,
    quiet: Option<Quiet>,
    max_per_day: usize,
    prompt: String,
}

impl Heartbeat {
    /// `None` when it is off. The session's person must be an owner: the
    /// heartbeat acts with their permissions, unasked.
    /// `None` when it is off. Checked either way, so a setting that would
    /// stop it from starting is refused before it is ever turned on.
    pub fn from_config(config: &HeartbeatConfig, access: &AccessConfig) -> Result<Option<Self>> {
        let (session, actor) = owner_chat("heartbeat", config.session.as_deref(), access)?;
        if config.every_minutes == 0 {
            bail!("heartbeat.every_minutes must be at least 1");
        }
        let mut prompt = PROMPT.to_owned();
        if let Some(extra) = config.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            prompt.push_str("\n\nThe owner adds: ");
            prompt.push_str(extra.trim());
        }
        let quiet = Quiet::parse(&config.quiet_hours)?;
        if !config.enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            session,
            actor,
            every: Duration::from_secs(config.every_minutes * 60),
            quiet,
            max_per_day: config.max_per_day,
            prompt,
        }))
    }

    /// Why this moment is not one to reach out in, if it is not. Ignored
    /// heartbeats make it wait longer each time; a quick answer to the
    /// last one allows one more that day.
    fn hold_back(&self, at: &Moment) -> Option<&'static str> {
        if self.quiet.is_some_and(|q| q.contains(at.time)) {
            return Some("quiet hours");
        }
        let sent_today = at.sent.iter().filter(|&&t| t >= at.midnight).count();
        let warm = at.warm();
        if sent_today >= self.max_per_day + usize::from(warm) {
            return Some("daily limit reached");
        }
        let ignored = at.ignored();
        if let Some(&last) = at.sent.last()
            && ignored > 0
        {
            // 12 h after one unanswered heartbeat, then 24 h, then 48 h.
            let wait = UNANSWERED_FOR << (ignored.min(3) - 1);
            if at.now - last < wait {
                return Some(if ignored == 1 {
                    "the last one is unanswered"
                } else {
                    "several in a row were unanswered; waiting longer"
                });
            }
        }
        if at
            .spoken
            .last()
            .is_some_and(|&spoken| at.now - spoken < JUST_TALKED)
        {
            return Some("they are talking already");
        }
        None
    }
}

/// The facts a heartbeat decides on.
#[derive(Debug, Clone, Default)]
struct Moment {
    now: i64,
    time: NaiveTime,
    /// Start of the local day.
    midnight: i64,
    /// When recent heartbeats were sent, oldest first.
    sent: Vec<i64>,
    /// When the person wrote recently, oldest first.
    spoken: Vec<i64>,
}

impl Moment {
    /// When the person first wrote after the heartbeat at `index`, before
    /// the next one.
    fn answer(&self, index: usize) -> Option<i64> {
        let sent = self.sent[index];
        let until = self.sent.get(index + 1).copied().unwrap_or(i64::MAX);
        self.spoken
            .iter()
            .copied()
            .find(|&t| t > sent && t <= until)
    }

    /// The latest heartbeats in a row that got no answer.
    fn ignored(&self) -> u32 {
        (0..self.sent.len())
            .rev()
            .take_while(|&i| self.answer(i).is_none())
            .count() as u32
    }

    /// Whether the last heartbeat was answered within a few minutes.
    fn warm(&self) -> bool {
        let Some(last) = self.sent.len().checked_sub(1) else {
            return false;
        };
        self.answer(last)
            .is_some_and(|t| t - self.sent[last] <= QUICK_ANSWER)
    }
}

/// The owner's conversation that `section` (`heartbeat`, `life`) reaches
/// out in, and the owner it acts as: `session`, or by default the private
/// chat of the first owner on a chat app (`qq:`, `feishu:`, `zulip:`),
/// else the local `main` session (seen in
/// the terminal and the Web UI). Refuses anyone who is not an owner, since
/// the turn uses their permissions unasked.
pub fn owner_chat(
    section: &str,
    session: Option<&str>,
    access: &AccessConfig,
) -> Result<(String, Actor)> {
    let session = match session.map(str::trim).filter(|s| !s.is_empty()) {
        Some(session) => session.to_owned(),
        None => access
            .owners
            .iter()
            .find_map(|o| crate::chat::private_chat(o))
            .unwrap_or_else(|| "main".to_owned()),
    };
    let actor = match sender(&session) {
        Some(sender) => access.resolve(&sender, &session),
        None if crate::chat::is_chat_app(&session) => {
            bail!("{section}: {session} is a group; use a private chat")
        }
        None => Actor::owner(crate::access::CLI),
    };
    if !actor.owner {
        bail!("{section}: {} is not in access.owners", actor.id);
    }
    Ok((session, actor))
}

/// Who writes in a private channel session: `qq:<openid>`,
/// `feishu:<open_id>`, `zulip:<email>` or `mail:<address>`.
pub fn sender(session: &str) -> Option<String> {
    if let Some(sender) = crate::chat::private_sender(session) {
        return Some(sender);
    }
    session.starts_with("mail:").then(|| session.to_owned())
}

/// Whether the model chose to say nothing.
pub fn is_silent(reply: &str) -> bool {
    let reply = reply.trim();
    reply.is_empty() || reply.contains(SILENT)
}

/// How often the loops look at the live settings.
const CHECK: Duration = Duration::from_secs(60);

/// What the settings say now: the live ones, else those it started with.
pub(crate) fn current<C: Clone>(
    started: &(C, AccessConfig),
    pick: impl FnOnce(&crate::config::Config) -> C,
) -> (C, AccessConfig) {
    crate::live::read(|c| (pick(c), c.access.clone())).unwrap_or_else(|| started.clone())
}

/// Says once what the loop is doing now, when that changes.
pub(crate) fn announce(shown: &mut String, now: String) {
    if *shown != now {
        eprintln!("{now}");
        *shown = now;
    }
}

/// Beats for the life of the process, following the settings as they
/// change: turned on or off, a new interval or quiet hours apply within a
/// minute.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    config: HeartbeatConfig,
    access: AccessConfig,
    gateway: Arc<Gateway<M, T>>,
) {
    let started = (config, access);
    let mut last = tokio::time::Instant::now();
    let mut shown = String::new();
    loop {
        let (config, access) = current(&started, |c| c.heartbeat.clone());
        let heartbeat = match Heartbeat::from_config(&config, &access) {
            Ok(Some(heartbeat)) => heartbeat,
            Ok(None) => {
                announce(&mut shown, "heartbeat: off".into());
                tokio::time::sleep(CHECK).await;
                continue;
            }
            Err(err) => {
                announce(&mut shown, format!("heartbeat: off: {err:#}"));
                tokio::time::sleep(CHECK).await;
                continue;
            }
        };
        announce(
            &mut shown,
            format!(
                "heartbeat: every {} min in {}",
                heartbeat.every.as_secs() / 60,
                heartbeat.session
            ),
        );
        if last.elapsed() >= heartbeat.every {
            last = tokio::time::Instant::now();
            match beat(&heartbeat, &gateway).await {
                Ok(outcome) => eprintln!("heartbeat: {outcome}"),
                Err(err) => eprintln!("heartbeat: {err:#}"),
            }
        }
        tokio::time::sleep(CHECK).await;
    }
}

async fn beat<M: Model + 'static, T: Tools + 'static>(
    heartbeat: &Heartbeat,
    gateway: &Gateway<M, T>,
) -> Result<String> {
    let store = gateway.store();
    let session_id = store.session_id(&heartbeat.session)?;
    let now = chrono::Local::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
        .map_or(now.timestamp() - 86_400, |t| t.timestamp());
    let since = now.timestamp() - LOOK_BACK;
    let at = Moment {
        now: now.timestamp(),
        time: now.time(),
        midnight,
        sent: store.prompts_since(session_id, PROMPT_PREFIX, since)?,
        spoken: store.spoken_since(session_id, crate::agent::AUTOMATIC_PROMPTS, since)?,
    };
    if let Some(why) = heartbeat.hold_back(&at) {
        return Ok(format!("skipped ({why})"));
    }
    Ok(
        match gateway
            .heartbeat(
                heartbeat.actor.clone(),
                &heartbeat.session,
                &heartbeat.prompt,
            )
            .await?
        {
            Some(text) => format!("sent {} characters", text.chars().count()),
            None => "nothing to say".into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    #[test]
    fn quiet_hours_cross_midnight() {
        let q = Quiet::parse("23:00-08:00").unwrap().unwrap();
        for at in ["23:00", "23:59", "00:00", "03:30", "07:59"] {
            assert!(q.contains(t(at)), "{at}");
        }
        for at in ["08:00", "12:00", "22:59"] {
            assert!(!q.contains(t(at)), "{at}");
        }
        let day = Quiet::parse("12:00-13:30").unwrap().unwrap();
        assert!(day.contains(t("12:45")) && !day.contains(t("13:30")));
        assert_eq!(Quiet::parse(" ").unwrap(), None);
        assert!(Quiet::parse("23-8").is_err());
        assert!(Quiet::parse("25:00-08:00").is_err());
    }

    fn owner_access() -> AccessConfig {
        AccessConfig {
            owners: vec!["mail:me@example.com".into(), "qq:ME".into()],
            ..AccessConfig::default()
        }
    }

    fn on(config: HeartbeatConfig) -> HeartbeatConfig {
        HeartbeatConfig {
            enabled: true,
            ..config
        }
    }

    #[test]
    fn defaults_to_the_owners_private_qq_chat() {
        let access = owner_access();
        assert!(
            Heartbeat::from_config(&HeartbeatConfig::default(), &access)
                .unwrap()
                .is_none()
        );
        let beat = Heartbeat::from_config(&on(HeartbeatConfig::default()), &access)
            .unwrap()
            .unwrap();
        assert_eq!(beat.session, "qq:c2c:ME");
        assert!(beat.actor.owner);
        assert_eq!(beat.every, Duration::from_secs(120 * 60));

        let stranger = HeartbeatConfig {
            session: Some("qq:c2c:SOMEONE".into()),
            ..on(HeartbeatConfig::default())
        };
        assert!(Heartbeat::from_config(&stranger, &access).is_err());
        let group = HeartbeatConfig {
            session: Some("qq:group:G".into()),
            ..on(HeartbeatConfig::default())
        };
        assert!(Heartbeat::from_config(&group, &access).is_err());
        let local = HeartbeatConfig {
            session: Some("main".into()),
            prompt: Some("Ask about my run.".into()),
            ..on(HeartbeatConfig::default())
        };
        let beat = Heartbeat::from_config(&local, &AccessConfig::default())
            .unwrap()
            .unwrap();
        assert!(beat.actor.owner);
        assert!(beat.prompt.starts_with(PROMPT_PREFIX));
        assert!(beat.prompt.ends_with("Ask about my run."));
        let local =
            Heartbeat::from_config(&on(HeartbeatConfig::default()), &AccessConfig::default())
                .unwrap()
                .unwrap();
        assert_eq!(local.session, "main", "no QQ owner to default to");
        let blank = HeartbeatConfig {
            session: Some(" ".into()),
            ..on(HeartbeatConfig::default())
        };
        assert_eq!(
            Heartbeat::from_config(&blank, &owner_access())
                .unwrap()
                .unwrap()
                .session,
            "qq:c2c:ME"
        );
    }

    #[test]
    fn holds_back_when_it_would_be_too_much() {
        let beat = Heartbeat::from_config(&on(HeartbeatConfig::default()), &owner_access())
            .unwrap()
            .unwrap();
        let now = 1_000_000;
        let h = 3600;
        let free = Moment {
            now,
            time: t("10:00"),
            midnight: now - 10 * h,
            sent: Vec::new(),
            spoken: vec![now - 5 * h],
        };
        let at = |m: Moment| beat.hold_back(&m);
        assert_eq!(at(free.clone()), None);
        assert_eq!(
            at(Moment {
                time: t("23:30"),
                ..free.clone()
            }),
            Some("quiet hours")
        );
        // Three today, each answered after a while.
        let three = Moment {
            sent: vec![now - 9 * h, now - 6 * h, now - 3 * h],
            spoken: vec![now - 8 * h, now - 5 * h, now - 2 * h],
            ..free.clone()
        };
        assert_eq!(at(three.clone()), Some("daily limit reached"));
        // The last answered within minutes: one more is welcome.
        assert_eq!(
            at(Moment {
                spoken: vec![now - 8 * h, now - 5 * h, now - 3 * h + 300],
                ..three
            }),
            None
        );
        // Sent after they last wrote, an hour ago: wait for an answer.
        assert_eq!(
            at(Moment {
                sent: vec![now - h],
                ..free.clone()
            }),
            Some("the last one is unanswered")
        );
        // Unanswered for longer than 12 hours: try again.
        assert_eq!(
            at(Moment {
                sent: vec![now - 13 * h],
                spoken: vec![now - 20 * h],
                ..free.clone()
            }),
            None
        );
        // Two ignored in a row: a day between them; three: two days.
        let ignored = |sent: Vec<i64>| Moment {
            sent,
            spoken: vec![now - 100 * h],
            ..free.clone()
        };
        assert_eq!(
            at(ignored(vec![now - 40 * h, now - 20 * h])),
            Some("several in a row were unanswered; waiting longer")
        );
        assert_eq!(at(ignored(vec![now - 50 * h, now - 25 * h])), None);
        assert_eq!(
            at(ignored(vec![now - 90 * h, now - 70 * h, now - 40 * h])),
            Some("several in a row were unanswered; waiting longer")
        );
        assert_eq!(
            at(ignored(vec![now - 90 * h, now - 70 * h, now - 49 * h])),
            None
        );
        assert_eq!(
            at(Moment {
                spoken: vec![now - 60],
                ..free
            }),
            Some("they are talking already")
        );
    }

    #[test]
    fn silence_is_recognized() {
        assert!(is_silent("NO_REPLY"));
        assert!(is_silent("  NO_REPLY\n"));
        assert!(is_silent(""));
        assert!(is_silent("Nothing now. NO_REPLY"));
        assert!(!is_silent("早上好，面试准备得怎么样？"));
    }
}
