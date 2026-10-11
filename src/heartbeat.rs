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
    pub fn from_config(config: &HeartbeatConfig, access: &AccessConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let session = match &config.session {
            Some(session) => session.clone(),
            None => access
                .owners
                .iter()
                .find_map(|o| o.strip_prefix("qq:"))
                .map(|id| format!("qq:c2c:{id}"))
                .context(
                    "heartbeat: set heartbeat.session, or add a \"qq:<openid>\" owner to access.owners",
                )?,
        };
        let actor = match sender(&session) {
            Some(sender) => access.resolve(&sender, &session),
            None if session.starts_with("qq:group:") => {
                bail!("heartbeat: {session} is a group; use a private chat")
            }
            None => Actor::owner(crate::access::CLI),
        };
        if !actor.owner {
            bail!("heartbeat: {} is not in access.owners", actor.id);
        }
        if config.every_minutes == 0 {
            bail!("heartbeat.every_minutes must be at least 1");
        }
        let mut prompt = PROMPT.to_owned();
        if let Some(extra) = config.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            prompt.push_str("\n\nThe owner adds: ");
            prompt.push_str(extra.trim());
        }
        Ok(Some(Self {
            session,
            actor,
            every: Duration::from_secs(config.every_minutes * 60),
            quiet: Quiet::parse(&config.quiet_hours)?,
            max_per_day: config.max_per_day,
            prompt,
        }))
    }

    /// Why this moment is not one to reach out in, if it is not.
    fn hold_back(&self, at: Moment) -> Option<&'static str> {
        if self.quiet.is_some_and(|q| q.contains(at.time)) {
            return Some("quiet hours");
        }
        if at.sent_today >= self.max_per_day {
            return Some("daily limit reached");
        }
        let unanswered = at
            .last_sent
            .is_some_and(|sent| at.last_spoken.is_none_or(|spoken| spoken < sent));
        if unanswered
            && at
                .last_sent
                .is_some_and(|sent| at.now - sent < UNANSWERED_FOR)
        {
            return Some("the last one is unanswered");
        }
        if at
            .last_spoken
            .is_some_and(|spoken| at.now - spoken < JUST_TALKED)
        {
            return Some("they are talking already");
        }
        None
    }
}

/// The facts a heartbeat decides on.
#[derive(Debug, Clone, Copy)]
struct Moment {
    now: i64,
    time: NaiveTime,
    sent_today: usize,
    last_sent: Option<i64>,
    last_spoken: Option<i64>,
}

/// Who writes in a channel session: `qq:<openid>` or `mail:<address>`.
fn sender(session: &str) -> Option<String> {
    if let Some(id) = session.strip_prefix("qq:c2c:") {
        return Some(format!("qq:{id}"));
    }
    session.starts_with("mail:").then(|| session.to_owned())
}

/// Whether the model chose to say nothing.
pub fn is_silent(reply: &str) -> bool {
    let reply = reply.trim();
    reply.is_empty() || reply.contains(SILENT)
}

/// Beats for the life of the process.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    heartbeat: Heartbeat,
    gateway: Arc<Gateway<M, T>>,
) {
    eprintln!(
        "heartbeat: every {} min in {}",
        heartbeat.every.as_secs() / 60,
        heartbeat.session
    );
    let mut tick = tokio::time::interval(heartbeat.every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is now; wait one period instead.
    tick.tick().await;
    loop {
        tick.tick().await;
        match beat(&heartbeat, &gateway).await {
            Ok(outcome) => eprintln!("heartbeat: {outcome}"),
            Err(err) => eprintln!("heartbeat: {err:#}"),
        }
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
    let sent = store.prompts_since(session_id, PROMPT_PREFIX, midnight)?;
    let last_sent = match sent.last() {
        Some(&t) => Some(t),
        None => store
            .prompts_since(session_id, PROMPT_PREFIX, now.timestamp() - UNANSWERED_FOR)?
            .last()
            .copied(),
    };
    let at = Moment {
        now: now.timestamp(),
        time: now.time(),
        sent_today: sent.len(),
        last_sent,
        last_spoken: store.last_spoken_at(session_id, crate::agent::AUTOMATIC_PROMPTS)?,
    };
    if let Some(why) = heartbeat.hold_back(at) {
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
        assert!(
            Heartbeat::from_config(&on(HeartbeatConfig::default()), &AccessConfig::default())
                .is_err(),
            "no QQ owner to default to"
        );
    }

    #[test]
    fn holds_back_when_it_would_be_too_much() {
        let beat = Heartbeat::from_config(&on(HeartbeatConfig::default()), &owner_access())
            .unwrap()
            .unwrap();
        let now = 1_000_000;
        let free = Moment {
            now,
            time: t("10:00"),
            sent_today: 0,
            last_sent: None,
            last_spoken: Some(now - 5 * 3600),
        };
        assert_eq!(beat.hold_back(free), None);
        let at = |m: Moment| beat.hold_back(m);
        assert_eq!(
            at(Moment {
                time: t("23:30"),
                ..free
            }),
            Some("quiet hours")
        );
        assert_eq!(
            at(Moment {
                sent_today: 3,
                ..free
            }),
            Some("daily limit reached")
        );
        // Sent after they last wrote, an hour ago: wait for an answer.
        assert_eq!(
            at(Moment {
                last_sent: Some(now - 3600),
                ..free
            }),
            Some("the last one is unanswered")
        );
        // Unanswered for longer than that: try again.
        assert_eq!(
            at(Moment {
                last_sent: Some(now - 13 * 3600),
                last_spoken: Some(now - 20 * 3600),
                ..free
            }),
            None
        );
        // Answered.
        assert_eq!(
            at(Moment {
                last_sent: Some(now - 3 * 3600),
                last_spoken: Some(now - 2 * 3600),
                ..free
            }),
            None
        );
        assert_eq!(
            at(Moment {
                last_spoken: Some(now - 60),
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
