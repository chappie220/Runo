//! How the agent is doing, for the Web UI's status page (`/api/status`):
//! today's spending, its mood and diary, what it saved about how it draws
//! and sounds, its stickers, scheduled jobs and background tasks.

use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

use crate::store::Store;

/// Cost and calls by kind (turn, summary, image, speech, …) since `since`.
fn spending(store: &Store, since: i64) -> Result<Vec<Value>> {
    let conn = store.runtime();
    let mut stmt = conn.prepare(
        "SELECT kind, COALESCE(SUM(cost), 0), count(*) FROM usage
         WHERE created_at >= ?1 GROUP BY kind ORDER BY 2 DESC",
    )?;
    let rows = stmt
        .query_map(params![since], |r| {
            Ok(json!({
                "kind": r.get::<_, String>(0)?,
                "cost": r.get::<_, f64>(1)?,
                "calls": r.get::<_, i64>(2)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Everything the status page shows, read now.
pub fn snapshot(store: &Store) -> Result<Value> {
    let identity = store.identity()?;
    let name = identity
        .as_ref()
        .map(|i| i.name.clone())
        .unwrap_or_default();
    let today = crate::limits::day_start();
    let month = crate::limits::month_start();
    let diary: Vec<Value> = store
        .diary(7)?
        .into_iter()
        .map(|d| json!({"day": d.day, "mood": d.mood, "text": d.text}))
        .collect();
    let stickers: Vec<Value> = store
        .sticker_moods(&name)?
        .into_iter()
        .map(|(mood, count)| json!({"mood": mood, "count": count}))
        .collect();
    let jobs: Vec<Value> = store
        .job_list()?
        .into_iter()
        .map(|j| {
            json!({
                "name": j.name, "schedule": j.schedule, "session": j.session,
                "next_run": j.next_run, "last_run": j.last_run, "last_status": j.last_status,
            })
        })
        .collect();
    let tasks: Vec<Value> = store
        .tasks(None, 10)?
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id, "title": t.title, "state": t.state, "open": t.open(),
                "session": t.own_session(), "created_at": t.created_at,
                "finished_at": t.finished_at,
                "result": t.result.map(|r| r.chars().take(300).collect::<String>()),
            })
        })
        .collect();
    let settings = crate::live::read(|c| {
        json!({
            "heartbeat": c.heartbeat.enabled,
            "life": c.life.enabled,
            "voice": c.speech.enabled && c.speech.model().is_some(),
            "stickers": c.stickers.enabled,
            "tasks": c.tasks.enabled,
            "memory_by_meaning": c.embedding.model().is_some(),
            "daily_limit": c.limits.daily_usd,
        })
    })
    .unwrap_or(Value::Null);
    Ok(json!({
        "identity": identity.map(|i| json!({
            "name": i.name, "creature": i.creature, "vibe": i.vibe, "emoji": i.emoji,
        })),
        "spent": {
            "today": store.spent_since(today, None)?,
            "month": store.spent_since(month, None)?,
            "today_by_kind": spending(store, today)?,
        },
        "mood": diary.first().map(|d| d["mood"].clone()),
        "diary": diary,
        "drawing_profile": store.drawing_profile(&name)?,
        "voice_profile": store.voice_profile(&name)?.map(|(voice, how)| json!({"voice": voice, "instructions": how})),
        "stickers": stickers,
        "memories": store.memory_count()?,
        "jobs": jobs,
        "tasks": tasks,
        "settings": settings,
    }))
}

impl Store {
    pub fn memory_count(&self) -> Result<i64> {
        Ok(self
            .soul()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_has_spending_mood_and_work() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_usage(
                "main",
                "image",
                Some("cli"),
                Some("x/draw"),
                &crate::llm::Usage {
                    cost: 0.04,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .diary_put("2026-10-12", "今天很开心", "happy")
            .unwrap();
        store.task_add("周报", "整理", "main", "cli", 5).unwrap();
        store.memory_save("用户喜欢猫").unwrap();
        let status = snapshot(&store).unwrap();
        assert!((status["spent"]["today"].as_f64().unwrap() - 0.04).abs() < 1e-9);
        assert_eq!(status["spent"]["today_by_kind"][0]["kind"], "image");
        assert_eq!(status["mood"], "happy");
        assert_eq!(status["tasks"][0]["title"], "周报");
        assert_eq!(status["tasks"][0]["open"], true);
        assert_eq!(status["memories"], 1);
        assert!(status["identity"].is_null());
    }
}
