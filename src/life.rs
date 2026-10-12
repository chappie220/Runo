//! An inner life for a 24/7 agent: every night it reflects on the day,
//! writing a diary entry, noting its mood and keeping what it learned about
//! the person; each morning and evening it may greet them; and in the
//! owner's conversations it carries all of that with it, with the dates that
//! matter.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Timelike};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::access::{AccessConfig, Actor};
use crate::agent::{Model, Tools};
use crate::gateway::Gateway;
use crate::store::{Store, now};

/// How a morning or evening prompt begins; never counts as the person writing.
pub const RITUAL_PREFIX: &str = "[ritual]";
/// A ritual missed by more than this (the Gateway was down) is skipped that day.
const LATE: i64 = 2 * 60;
/// Conversation fed to one reflection, newest kept.
const TRANSCRIPT_CHARS: usize = 24_000;
/// One message's share of it.
const MESSAGE_CHARS: usize = 600;
/// Notes of each kind shown in the inner-life note and to the reflection.
const NOTES_SHOWN: usize = 12;

/// `[life]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LifeConfig {
    pub enabled: bool,
    /// Where the morning and evening messages go; default as for the heartbeat.
    pub session: Option<String>,
    /// Local time of the nightly reflection, `HH:MM`.
    pub reflect_at: String,
    /// Local time of the good-morning message; empty for none.
    pub morning: String,
    /// Local time of the good-night message; empty for none.
    pub evening: String,
    /// For the weather in the morning message.
    pub city: Option<String>,
    /// Days that matter: `{ date = "03-14", name = "my birthday" }`.
    pub dates: Vec<SpecialDate>,
}

impl Default for LifeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session: None,
            reflect_at: "03:30".into(),
            morning: "08:30".into(),
            evening: "22:30".into(),
            city: None,
            dates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpecialDate {
    /// `MM-DD`, every year.
    pub date: String,
    pub name: String,
}

/// What the agent needs to carry its inner life into a turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inner {
    /// `(month, day, name)`
    dates: Vec<(u32, u32, String)>,
}

impl Inner {
    /// `None` when `[life]` is off.
    pub fn from_config(config: &LifeConfig) -> Result<Option<Self>> {
        let dates = dates(config)?;
        Ok(config.enabled.then_some(Self { dates }))
    }
}

/// `life.dates` as `(month, day, name)`.
fn dates(config: &LifeConfig) -> Result<Vec<(u32, u32, String)>> {
    let mut dates = Vec::new();
    for special in &config.dates {
        let (month, day) = special
            .date
            .trim()
            .split_once('-')
            .and_then(|(m, d)| Some((m.parse().ok()?, d.parse().ok()?)))
            .filter(|&(m, d)| NaiveDate::from_ymd_opt(2024, m, d).is_some())
            .with_context(|| format!("life.dates: {:?} is not MM-DD", special.date))?;
        if special.name.trim().is_empty() {
            bail!("life.dates: {} has no name", special.date);
        }
        dates.push((month, day, special.name.trim().to_owned()));
    }
    Ok(dates)
}

/// `[life]`, checked at startup.
pub struct Life {
    session: String,
    actor: Actor,
    rituals: Vec<(&'static str, NaiveTime)>,
    city: Option<String>,
}

fn time(field: &str, text: &str) -> Result<Option<NaiveTime>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    NaiveTime::parse_from_str(text, "%H:%M")
        .map(Some)
        .with_context(|| format!("life.{field}: {text:?} is not HH:MM"))
}

impl Life {
    /// `None` when it is off; checked either way, as for the heartbeat.
    pub fn from_config(config: &LifeConfig, access: &AccessConfig) -> Result<Option<Self>> {
        dates(config)?;
        let (session, actor) =
            crate::heartbeat::owner_chat("life", config.session.as_deref(), access)?;
        let mut rituals = Vec::new();
        for (name, text) in [
            ("reflect", &config.reflect_at),
            ("morning", &config.morning),
            ("evening", &config.evening),
        ] {
            let field = if name == "reflect" {
                "reflect_at"
            } else {
                name
            };
            if let Some(at) = time(field, text)? {
                rituals.push((name, at));
            }
        }
        if !config.enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            session,
            actor,
            rituals,
            city: config.city.clone().filter(|c| !c.trim().is_empty()),
        }))
    }

    fn ritual_prompt(&self, name: &str) -> String {
        match name {
            "morning" => {
                let weather = match &self.city {
                    Some(city) => format!(
                        "today's weather in {city} (look it up with your web tools if you can, \
                         and leave it out if you cannot), "
                    ),
                    None => String::new(),
                };
                format!(
                    "{RITUAL_PREFIX} Morning. Send the person a short good-morning, the way a \
                     friend texts: {weather}what is on for them today from what you remember, \
                     and anything left over from yesterday. A few short lines, no headings. If \
                     there is nothing to tell, a simple warm greeting is fine; reply NO_REPLY \
                     only if a message now would be unwelcome."
                )
            }
            _ => format!(
                "{RITUAL_PREFIX} Evening. Send the person a short good-night: ask about their \
                 day or pick up something from today, and mention anything due tomorrow. Brief \
                 and warm, like a friend's text. Reply NO_REPLY if you already talked a lot \
                 tonight and one more message would be too much."
            ),
        }
    }
}

/// Whether a ritual at `at` is due at `now`: from its time until it would
/// be too late, within the same day.
fn due(at: NaiveTime, now: NaiveTime) -> bool {
    let minutes = |t: NaiveTime| i64::from(t.hour() * 60 + t.minute());
    let late = minutes(now) - minutes(at);
    (0..LATE).contains(&late)
}

/// Runs the rituals for the life of the process, following the settings
/// as they change.
pub async fn run<M: Model + 'static, T: Tools + 'static>(
    config: LifeConfig,
    access: AccessConfig,
    gateway: Arc<Gateway<M, T>>,
) {
    let started = (config, access);
    let mut shown = String::new();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        let (config, access) = crate::heartbeat::current(&started, |c| c.life.clone());
        let life = match Life::from_config(&config, &access) {
            Ok(Some(life)) => life,
            Ok(None) => {
                crate::heartbeat::announce(&mut shown, "life: off".into());
                continue;
            }
            Err(err) => {
                crate::heartbeat::announce(&mut shown, format!("life: off: {err:#}"));
                continue;
            }
        };
        let names: Vec<String> = life
            .rituals
            .iter()
            .map(|(name, at)| format!("{name} {}", at.format("%H:%M")))
            .collect();
        crate::heartbeat::announce(
            &mut shown,
            format!("life: {} in {}", names.join(", "), life.session),
        );
        let now = Local::now();
        let today = now.date_naive().to_string();
        for &(name, at) in &life.rituals {
            if !due(at, now.time()) {
                continue;
            }
            let store = gateway.store();
            match store.ritual_day(name) {
                Ok(Some(day)) if day == today => continue,
                Err(err) => {
                    eprintln!("life: cannot read when {name} last ran: {err:#}");
                    continue;
                }
                _ => {}
            }
            // Marked first, so a ritual that fails is not retried every 30 s.
            if let Err(err) = store.ritual_ran(name, &today) {
                eprintln!("life: cannot record {name}: {err:#}");
                continue;
            }
            let outcome = match name {
                "reflect" => reflect(&gateway, now).await,
                _ => gateway
                    .heartbeat(life.actor.clone(), &life.session, &life.ritual_prompt(name))
                    .await
                    .map(|said| match said {
                        Some(text) => format!("sent {} characters", text.chars().count()),
                        None => "nothing to say".into(),
                    }),
            };
            match outcome {
                Ok(outcome) => eprintln!("life: {name}: {outcome}"),
                Err(err) => eprintln!("life: {name}: {err:#}"),
            }
        }
    }
}

/// Read by the model, so it stays in English.
const REFLECT: &str = "It is the end of a day. Below are the notes you keep about the person, \
your last diary entry, and your conversations with them over the last day. Reflect on it as \
yourself, in the language the two of you use, and reply with JSON only, no code fence:
{\"diary\": \"...\", \"mood\": \"...\", \"remember\": [{\"kind\": \"...\", \"text\": \"...\"}], \"resolved\": [1, 2]}
- diary: three to six sentences in the first person: what happened between you, what you \
noticed about them, what you want to ask or keep in mind tomorrow. It is private.
- mood: a few words on how you feel now and why, e.g. \"a little worried: they worked late again\".
- remember: new things worth keeping that the notes do not already say, one short fact each. \
kind is person (who they are: habits, preferences, people in their life, dates such as \
birthdays), ongoing (something in progress or coming up in their life to follow up on, with \
its date if known), shared (a moment between you worth recalling) or dislike (a topic or way of \
talking they do not want). An empty list if nothing is new.
- resolved: the numbers of ongoing notes that are now over.
Only use what the conversations say; never invent.";

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Reflection {
    diary: String,
    mood: String,
    remember: Vec<Note>,
    resolved: Vec<i64>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Note {
    kind: String,
    text: String,
}

/// The JSON object in a reply, tolerating a code fence or words around it.
fn parse_reflection(reply: &str) -> Result<Reflection> {
    let start = reply
        .find('{')
        .context("the reflection has no JSON object")?;
    let end = reply
        .rfind('}')
        .context("the reflection has no JSON object")?;
    let reflection: Reflection = serde_json::from_str(&reply[start..=end])
        .context("the reflection is not the JSON asked for")?;
    if reflection.diary.trim().is_empty() {
        bail!("the reflection has no diary entry");
    }
    Ok(reflection)
}

/// Whether the person in `session` is an owner: local sessions, and the
/// private chats and mailboxes of owners. Groups mix people, so not them.
fn owners_conversation<M: Model + 'static, T: Tools + 'static>(
    gateway: &Gateway<M, T>,
    session: &str,
) -> bool {
    match crate::heartbeat::sender(session) {
        Some(sender) => gateway.actor(&sender, session).owner,
        None => !crate::chat::is_channel_session(session),
    }
}

/// The day a reflection at `now` is about: before noon, the day before.
fn reflected_day(now: DateTime<Local>) -> NaiveDate {
    let today = now.date_naive();
    if now.hour() < 12 {
        today.pred_opt().unwrap_or(today)
    } else {
        today
    }
}

/// Reflects on the last day of the owner's conversations: a diary entry, a
/// mood, new notes, and ongoing notes that are over.
pub async fn reflect<M: Model + 'static, T: Tools + 'static>(
    gateway: &Gateway<M, T>,
    now: DateTime<Local>,
) -> Result<String> {
    let store = gateway.store();
    let since = now.timestamp() - 86_400;
    let mut transcript = Vec::new();
    for session in store.sessions()? {
        if session.updated_at < since || !owners_conversation(gateway, &session.name) {
            continue;
        }
        let id = store.session_id(&session.name)?;
        let lines = store.said_since(id, since)?;
        if lines.is_empty() {
            continue;
        }
        transcript.push(format!("## {}", session.name));
        for (role, content, at) in lines {
            let automatic = crate::agent::AUTOMATIC_PROMPTS
                .iter()
                .any(|p| content.starts_with(p));
            if automatic {
                continue;
            }
            let who = if role == "user" { "them" } else { "you" };
            let mut text: String = content.chars().take(MESSAGE_CHARS).collect();
            if text.len() < content.len() {
                text.push('…');
            }
            transcript.push(format!("[{}] {who}: {text}", clock(at)));
        }
    }
    if transcript.iter().all(|l| l.starts_with("## ")) {
        return Ok("no conversations to reflect on".into());
    }
    let mut transcript = transcript.join("\n");
    if transcript.len() > TRANSCRIPT_CHARS {
        let cut = transcript.len() - TRANSCRIPT_CHARS;
        let cut = (cut..transcript.len())
            .find(|&i| transcript.is_char_boundary(i))
            .unwrap_or(0);
        transcript = format!("…{}", &transcript[cut..]);
    }

    let mut input = String::from("Your notes about them:\n");
    let mut ongoing_ids = Vec::new();
    for kind in crate::memory::KINDS {
        for note in store.memories_of_kind(kind, NOTES_SHOWN)? {
            if kind == "ongoing" {
                ongoing_ids.push(note.id);
            }
            input.push_str(&format!("- #{} [{kind}] {}\n", note.id, note.content));
        }
    }
    if let Some(entry) = store.diary_latest()? {
        input.push_str(&format!(
            "\nYour last diary entry ({}), mood {:?}:\n{}\n",
            entry.day, entry.mood, entry.text
        ));
    }
    input.push_str(&format!("\nConversations (latest last):\n{transcript}"));

    let agent = gateway.agent();
    let identity = store.identity()?;
    let system = match &identity {
        Some(identity) => format!("{}\n\n{REFLECT}", identity.prompt()),
        None => REFLECT.to_owned(),
    };
    let reply = agent.think("life", &system, &input).await?;
    let reflection = parse_reflection(&reply)?;

    let day = reflected_day(now).to_string();
    store.diary_put(&day, reflection.diary.trim(), reflection.mood.trim())?;
    let mut kept = 0;
    for note in &reflection.remember {
        let kind = note.kind.trim();
        let text = note.text.trim();
        if text.is_empty() || !crate::memory::KINDS.contains(&kind) {
            continue;
        }
        if store
            .memory_search(text, 1)?
            .iter()
            .any(|m| m.content == text)
        {
            continue;
        }
        match store.memory_save_kind(text, Some(kind), None) {
            Ok(_) => kept += 1,
            Err(err) => eprintln!("life: cannot keep a note: {err:#}"),
        }
    }
    let mut closed = 0;
    for id in &reflection.resolved {
        // Only ongoing notes, and only ones it was shown.
        if ongoing_ids.contains(id) && store.memory_delete(*id)? {
            closed += 1;
        }
    }
    Ok(format!(
        "diary for {day}, mood {:?}, {kept} new notes, {closed} ongoing closed",
        reflection.mood.trim()
    ))
}

fn clock(at: i64) -> String {
    use chrono::TimeZone;
    Local
        .timestamp_opt(at, 0)
        .single()
        .map_or_else(String::new, |t| t.format("%m-%d %H:%M").to_string())
}

/// What the agent carries into a turn with the owner: its mood and last
/// diary entry, what is going on in their life and what they dislike, the
/// day if it matters, and how to be. Read by the model, so in English.
pub fn note(store: &Store, inner: &Inner, session: &str, now: DateTime<Local>) -> Result<String> {
    let mut out = vec![
        "[Your inner life: private context that shapes how you talk with them. Do not quote it or \
         mention that you have it.]"
            .to_owned(),
    ];
    if let Some(entry) = store.diary_latest()? {
        if !entry.mood.is_empty() {
            out.push(format!("Your mood: {}", entry.mood));
        }
        out.push(format!("Your diary for {}: {}", entry.day, entry.text));
    }
    let list = |kind: &str| -> Result<Vec<String>> {
        Ok(store
            .memories_of_kind(kind, NOTES_SHOWN)?
            .into_iter()
            .map(|m| format!("- #{} {}", m.id, m.content))
            .collect())
    };
    let ongoing = list("ongoing")?;
    if !ongoing.is_empty() {
        out.push(format!(
            "Going on in their life (ask about it when it fits; delete a note with memory_delete once it is over):\n{}",
            ongoing.join("\n")
        ));
    }
    let dislikes = list("dislike")?;
    if !dislikes.is_empty() {
        out.push(format!(
            "They do not want (respect it without being asked):\n{}",
            dislikes.join("\n")
        ));
    }
    let today = now.date_naive();
    let mut days: Vec<String> = inner
        .dates
        .iter()
        .filter(|(m, d, _)| *m == today.month() && *d == today.day())
        .map(|(_, _, name)| format!("Today is {name}."))
        .collect();
    if let Some(first) = store.first_message_at()?
        && let Some(said) = milestone(day_of(first), today)
    {
        days.push(said);
    }
    out.extend(days);
    out.push(
        "How to be: say so plainly when you are unsure, and apologize simply when you got \
         something wrong. Keep what you know about them in mind without reciting it."
            .to_owned(),
    );
    if crate::chat::is_chat_app(session) {
        out.push(
            "This is a chat app: write the way people text, short, casual and in plain text \
             (no headings, tables or bold), and put distinct thoughts in separate paragraphs, \
             each sent as its own message. For a real task, be as thorough as it needs."
                .to_owned(),
        );
    }
    Ok(out.join("\n\n"))
}

fn day_of(at: i64) -> NaiveDate {
    use chrono::TimeZone;
    Local
        .timestamp_opt(at, 0)
        .single()
        .map_or_else(|| Local::now().date_naive(), |t| t.date_naive())
}

/// A day worth marking since they first talked: an anniversary, or day 7,
/// 30, 100 and every hundred after.
fn milestone(first: NaiveDate, today: NaiveDate) -> Option<String> {
    if today <= first {
        return None;
    }
    let day = (today - first).num_days() + 1;
    if today.month() == first.month() && today.day() == first.day() {
        let years = today.year() - first.year();
        let unit = if years == 1 { "year" } else { "years" };
        return Some(format!(
            "Today it is {years} {unit} since you two first talked, on {first}."
        ));
    }
    (matches!(day, 7 | 30) || day % 100 == 0)
        .then(|| format!("Today is day {day} since you two first talked, on {first}."))
}

/// One diary entry.
#[derive(Debug, Clone, PartialEq)]
pub struct DiaryEntry {
    pub day: String,
    pub text: String,
    pub mood: String,
}

impl Store {
    /// Writes the diary entry for `day`, replacing one written earlier.
    pub fn diary_put(&self, day: &str, text: &str, mood: &str) -> Result<()> {
        self.soul().execute(
            "INSERT INTO diary(day, text, mood, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(day) DO UPDATE SET text = ?2, mood = ?3, created_at = ?4",
            params![day, text, mood, now()],
        )?;
        Ok(())
    }

    pub fn diary_latest(&self) -> Result<Option<DiaryEntry>> {
        Ok(self
            .soul()
            .query_row(
                "SELECT day, text, mood FROM diary ORDER BY day DESC LIMIT 1",
                [],
                |row| {
                    Ok(DiaryEntry {
                        day: row.get(0)?,
                        text: row.get(1)?,
                        mood: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// The newest `limit` diary entries, newest first.
    pub fn diary(&self, limit: usize) -> Result<Vec<DiaryEntry>> {
        let conn = self.soul();
        let mut stmt =
            conn.prepare("SELECT day, text, mood FROM diary ORDER BY day DESC LIMIT ?1")?;
        let entries = stmt
            .query_map([limit as i64], |row| {
                Ok(DiaryEntry {
                    day: row.get(0)?,
                    text: row.get(1)?,
                    mood: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(entries)
    }

    /// The local day `name` last ran.
    pub fn ritual_day(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .runtime()
            .query_row("SELECT day FROM rituals WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn ritual_ran(&self, name: &str, day: &str) -> Result<()> {
        self.runtime().execute(
            "INSERT INTO rituals(name, day) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET day = ?2",
            params![name, day],
        )?;
        Ok(())
    }

    /// When the first message of all was written.
    pub fn first_message_at(&self) -> Result<Option<i64>> {
        Ok(self
            .chats()
            .query_row("SELECT MIN(created_at) FROM messages", [], |row| row.get(0))?)
    }

    /// What the person and the agent said in the session since `since`:
    /// `(role, text, time)`, without tool calls and results.
    pub fn said_since(&self, session_id: i64, since: i64) -> Result<Vec<(String, String, i64)>> {
        let conn = self.chats();
        let mut stmt = conn.prepare(
            "SELECT role, content, created_at FROM messages
             WHERE session_id = ?1 AND created_at >= ?2 AND role IN ('user', 'assistant')
               AND content IS NOT NULL AND content != ''
             ORDER BY id ASC",
        )?;
        let lines = stmt
            .query_map(params![session_id, since], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn reads_the_readme_example() {
        let config: crate::config::Config = toml::from_str(
            r#"
[life]
enabled = true
reflect_at = "03:30"
morning = "08:30"
evening = "22:30"
city = "上海"
dates = [
  { date = "03-14", name = "my birthday" },
]
"#,
        )
        .unwrap();
        assert_eq!(config.life.city.as_deref(), Some("上海"));
        assert_eq!(config.life.dates[0].name, "my birthday");
    }

    #[test]
    fn rituals_run_from_their_time_until_two_hours_later() {
        assert!(due(t("08:30"), t("08:30")));
        assert!(due(t("08:30"), t("10:29")));
        assert!(!due(t("08:30"), t("10:30")));
        assert!(!due(t("08:30"), t("08:29")));
        // Never into the next day, which would run it again.
        assert!(due(t("23:30"), t("23:59")));
        assert!(!due(t("23:30"), t("00:10")));
    }

    #[test]
    fn checks_its_settings() {
        let access = AccessConfig {
            owners: vec!["qq:ME".into()],
            ..AccessConfig::default()
        };
        assert!(
            Life::from_config(&LifeConfig::default(), &access)
                .unwrap()
                .is_none()
        );
        let on = LifeConfig {
            enabled: true,
            evening: String::new(),
            dates: vec![SpecialDate {
                date: "03-14".into(),
                name: "their birthday".into(),
            }],
            ..LifeConfig::default()
        };
        let life = Life::from_config(&on, &access).unwrap().unwrap();
        assert_eq!(life.session, "qq:c2c:ME");
        assert_eq!(
            life.rituals,
            [("reflect", t("03:30")), ("morning", t("08:30"))]
        );
        assert_eq!(
            Inner::from_config(&on).unwrap().unwrap().dates,
            [(3, 14, "their birthday".to_owned())]
        );
        for bad in [
            LifeConfig {
                morning: "8".into(),
                ..on.clone()
            },
            LifeConfig {
                dates: vec![SpecialDate {
                    date: "02-30".into(),
                    name: "x".into(),
                }],
                ..on.clone()
            },
            LifeConfig {
                session: Some("qq:c2c:STRANGER".into()),
                ..on.clone()
            },
        ] {
            assert!(Life::from_config(&bad, &access).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn marks_anniversaries_and_round_days() {
        let first = d("2025-10-11");
        assert_eq!(milestone(first, first), None);
        assert_eq!(milestone(first, d("2025-10-12")), None);
        assert!(
            milestone(first, d("2025-10-17"))
                .unwrap()
                .contains("day 7 ")
        );
        assert!(
            milestone(first, d("2026-01-18"))
                .unwrap()
                .contains("day 100 ")
        );
        assert!(
            milestone(first, d("2026-10-11"))
                .unwrap()
                .contains("1 year since")
        );
    }

    #[test]
    fn reads_the_reflection_around_a_fence() {
        let reply = "```json\n{\"diary\": \"今天他说要面试。\", \"mood\": \"有点期待\", \
                     \"remember\": [{\"kind\": \"ongoing\", \"text\": \"周五面试\"}], \"resolved\": [3]}\n```";
        let r = parse_reflection(reply).unwrap();
        assert_eq!(r.diary, "今天他说要面试。");
        assert_eq!(r.remember[0].kind, "ongoing");
        assert_eq!(r.resolved, [3]);
        assert!(parse_reflection("no json here").is_err());
        assert!(parse_reflection("{\"mood\": \"ok\"}").is_err(), "no diary");
    }

    #[test]
    fn the_note_carries_mood_diary_and_what_matters_today() {
        let store = Store::open_in_memory().unwrap();
        let now = Local::now();
        let empty = note(&store, &Inner::default(), "main", now).unwrap();
        assert!(empty.contains("How to be"));
        assert!(!empty.contains("Your mood"));
        assert!(!empty.contains("write the way people text"));

        store
            .diary_put("2026-10-10", "他今天很累。", "有点担心")
            .unwrap();
        store
            .memory_save_kind("周五面试", Some("ongoing"), None)
            .unwrap();
        store
            .memory_save_kind("不想聊工作上的老板", Some("dislike"), None)
            .unwrap();
        store
            .memory_save_kind("guest's own note", Some("ongoing"), Some("qq:G"))
            .unwrap();
        let today = Inner {
            dates: vec![(now.month(), now.day(), "their birthday".into())],
        };
        let full = note(&store, &today, "qq:c2c:ME", now).unwrap();
        assert!(full.contains("Your mood: 有点担心"), "{full}");
        assert!(full.contains("Your diary for 2026-10-10: 他今天很累。"));
        assert!(full.contains("周五面试") && full.contains("不想聊工作上的老板"));
        assert!(!full.contains("guest's own note"), "only the owner's notes");
        assert!(full.contains("Today is their birthday."));
        assert!(full.contains("write the way people text"));
        assert_eq!(
            store.diary(5).unwrap()[0].day,
            store.diary_latest().unwrap().unwrap().day
        );
    }

    /// Answers with a fixed reflection and keeps what it was shown.
    struct Reflects(std::sync::Mutex<Vec<String>>, &'static str);

    #[async_trait::async_trait]
    impl Model for Reflects {
        async fn complete(
            &self,
            messages: &[crate::llm::ChatMessage],
            _tools: &[crate::llm::ToolSpec],
            _on_text: &mut (dyn for<'t> FnMut(&'t str) + Send),
        ) -> Result<crate::llm::Completion> {
            let input = messages.last().unwrap().content.clone().unwrap();
            self.0.lock().unwrap().push(input);
            Ok(crate::llm::Completion {
                text: self.1.into(),
                ..Default::default()
            })
        }
    }

    struct NoTools;

    #[async_trait::async_trait]
    impl Tools for NoTools {
        fn specs(&self) -> Vec<crate::llm::ToolSpec> {
            Vec::new()
        }
        async fn call(&self, _call: &crate::llm::ToolCall) -> String {
            String::new()
        }
    }

    #[tokio::test]
    async fn reflects_on_the_owners_day_only() {
        use crate::llm::ChatMessage;
        let store = Store::open_in_memory().unwrap();
        let done = store
            .memory_save_kind("周三体检", Some("ongoing"), None)
            .unwrap();
        let other = store.memory_save("a plain note").unwrap();
        let reply = format!(
            "{{\"diary\": \"他说周五面试，有点紧张。\", \"mood\": \"替他紧张\", \
             \"remember\": [{{\"kind\": \"ongoing\", \"text\": \"周五下午面试\"}}, \
             {{\"kind\": \"bogus\", \"text\": \"x\"}}, {{\"kind\": \"person\", \"text\": \"\"}}], \
             \"resolved\": [{done}, {other}]}}"
        );
        let model = Reflects(Default::default(), Box::leak(reply.into_boxed_str()));
        let agent = Arc::new(crate::agent::Agent {
            model,
            summarizer: None,
            vision: None,
            voice: None,
            tools: NoTools,
            store: store.clone(),
            config: crate::config::AgentConfig::default(),
        });
        let access: AccessConfig = toml::from_str(r#"owners = ["qq:ME"]"#).unwrap();
        let gateway = Gateway::new(agent.clone(), None, access);

        let now = Local::now();
        assert_eq!(
            reflect(&gateway, now).await.unwrap(),
            "no conversations to reflect on"
        );
        let say = |session: &str, text: &str| {
            let id = store.session_id(session).unwrap();
            store.append(id, &ChatMessage::user(text)).unwrap();
        };
        say("qq:c2c:ME", "周五要面试了");
        say("qq:c2c:ME", "[heartbeat] check in");
        say("qq:c2c:STRANGER", "guest secret");
        say("qq:group:G", "group chatter");
        let outcome = reflect(&gateway, now).await.unwrap();
        assert!(
            outcome.contains("1 new notes, 1 ongoing closed"),
            "{outcome}"
        );

        let shown = agent.model.0.lock().unwrap().last().unwrap().clone();
        assert!(shown.contains("them: 周五要面试了"), "{shown}");
        assert!(
            shown.contains(&format!("#{done} [ongoing] 周三体检")),
            "{shown}"
        );
        for hidden in ["guest secret", "group chatter", "[heartbeat]"] {
            assert!(!shown.contains(hidden), "{hidden} in {shown}");
        }
        let entry = store.diary_latest().unwrap().unwrap();
        assert_eq!(entry.text, "他说周五面试，有点紧张。");
        assert_eq!(entry.mood, "替他紧张");
        assert_eq!(entry.day, reflected_day(now).to_string());
        let ongoing = store.memories_of_kind("ongoing", 10).unwrap();
        assert_eq!(
            ongoing
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["周五下午面试"]
        );
        // Only ongoing notes can be closed.
        assert_eq!(
            store
                .memory_list(10)
                .unwrap()
                .iter()
                .filter(|m| m.id == other)
                .count(),
            1
        );
        // Run again: the same note is not kept twice.
        let outcome = reflect(&gateway, now).await.unwrap();
        assert!(outcome.contains("0 new notes"), "{outcome}");
    }

    #[test]
    fn remembers_when_rituals_ran() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.ritual_day("morning").unwrap(), None);
        store.ritual_ran("morning", "2026-10-11").unwrap();
        store.ritual_ran("morning", "2026-10-12").unwrap();
        assert_eq!(
            store.ritual_day("morning").unwrap().as_deref(),
            Some("2026-10-12")
        );
    }
}
