//! Background tasks: long work ("look into this for a week's worth of
//! sources and write it up") that runs on its own while the conversation
//! goes on. `task_start` queues it; the Gateway's scheduler runs it in its
//! own session (`task:<id>`, readable in the Web UI) with the permissions
//! of whoever asked and more steps than a turn gets, then the agent tells
//! the conversation where it was asked, in its own words.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::store::{Store, now};

/// How the prompt of a task's own turn begins.
pub const PROMPT_PREFIX: &str = "[background task ";
/// How the note that brings a result back begins.
pub const DONE_PREFIX: &str = "[background task finished ";
/// Longest result passed back to the conversation; the rest stays in the
/// task's session.
pub const MAX_RESULT_CHARS: usize = 6000;

/// `[tasks]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksConfig {
    /// Off: no task tools.
    pub enabled: bool,
    /// Model calls one task may make (a turn gets `agent.max_steps`).
    pub max_steps: usize,
    /// Tasks running at once.
    pub concurrent: usize,
    /// Tasks one person may have waiting or running.
    pub max_open: usize,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_steps: 80,
            concurrent: 2,
            max_open: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub prompt: String,
    /// Where it was asked; the result goes back there.
    pub session: String,
    pub created_by: String,
    /// pending, running, cancelling, done, failed, cancelled
    pub state: String,
    pub result: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

impl Task {
    /// The session the task's own turn runs in.
    pub fn own_session(&self) -> String {
        format!("task:{}", self.id)
    }

    pub fn open(&self) -> bool {
        matches!(self.state.as_str(), "pending" | "running" | "cancelling")
    }
}

const COLUMNS: &str =
    "id, title, prompt, session, created_by, state, result, created_at, finished_at";

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        title: row.get(1)?,
        prompt: row.get(2)?,
        session: row.get(3)?,
        created_by: row.get(4)?,
        state: row.get(5)?,
        result: row.get(6)?,
        created_at: row.get(7)?,
        finished_at: row.get(8)?,
    })
}

impl Store {
    /// Queues a task asked for in `session` by `created_by`, unless they
    /// already have `max_open` waiting or running.
    pub fn task_add(
        &self,
        title: &str,
        prompt: &str,
        session: &str,
        created_by: &str,
        max_open: usize,
    ) -> Result<Task> {
        let (title, prompt) = (title.trim(), prompt.trim());
        if title.is_empty() || prompt.is_empty() {
            bail!("a task needs a title and what to do");
        }
        let conn = self.runtime();
        let open: i64 = conn.query_row(
            "SELECT count(*) FROM tasks WHERE created_by = ?1
             AND state IN ('pending', 'running', 'cancelling')",
            [created_by],
            |r| r.get(0),
        )?;
        if open as usize >= max_open.max(1) {
            bail!(
                "{open} tasks are already waiting or running; wait for one to finish, or cancel one"
            );
        }
        conn.execute(
            "INSERT INTO tasks (title, prompt, session, created_by, state, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![title, prompt, session, created_by, now()],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        Ok(self.task(id)?.expect("just inserted"))
    }

    pub fn task(&self, id: i64) -> Result<Option<Task>> {
        Ok(self
            .runtime()
            .query_row(
                &format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"),
                [id],
                row,
            )
            .optional()?)
    }

    /// Tasks `scope` created (`None`: everyone's), newest first.
    pub fn tasks(&self, scope: Option<&str>, limit: usize) -> Result<Vec<Task>> {
        let conn = self.runtime();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE ?1 IS NULL OR created_by = ?1
             ORDER BY id DESC LIMIT ?2"
        ))?;
        let found = stmt
            .query_map(params![scope, limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    /// Marks up to `room` waiting tasks running and returns them, oldest first.
    pub fn task_claim(&self, room: usize) -> Result<Vec<Task>> {
        if room == 0 {
            return Ok(Vec::new());
        }
        let conn = self.runtime();
        let mut stmt = conn.prepare(&format!(
            "UPDATE tasks SET state = 'running', started_at = ?1
             WHERE id IN (SELECT id FROM tasks WHERE state = 'pending' ORDER BY id LIMIT ?2)
             RETURNING {COLUMNS}"
        ))?;
        let mut found: Vec<Task> = stmt
            .query_map(params![now(), room as i64], row)?
            .collect::<rusqlite::Result<_>>()?;
        found.sort_by_key(|t| t.id);
        Ok(found)
    }

    pub fn tasks_running(&self) -> Result<usize> {
        let n: i64 = self.runtime().query_row(
            "SELECT count(*) FROM tasks WHERE state IN ('running', 'cancelling')",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Tasks someone asked to stop while they run.
    pub fn tasks_cancelling(&self) -> Result<Vec<Task>> {
        let conn = self.runtime();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE state = 'cancelling'"
        ))?;
        let found = stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    pub fn task_finish(&self, id: i64, state: &str, result: &str) -> Result<()> {
        self.runtime().execute(
            "UPDATE tasks SET state = ?2, result = ?3, finished_at = ?4 WHERE id = ?1",
            params![id, state, result, now()],
        )?;
        Ok(())
    }

    /// Stops task `id` if `scope` (`None`: anyone) created it: a waiting one
    /// is cancelled at once, a running one when the Gateway next looks.
    /// `None` when there is no such open task.
    pub fn task_cancel(&self, id: i64, scope: Option<&str>) -> Result<Option<String>> {
        let conn = self.runtime();
        let changed = conn.execute(
            "UPDATE tasks SET state = CASE state WHEN 'pending' THEN 'cancelled' ELSE 'cancelling' END,
             finished_at = CASE state WHEN 'pending' THEN ?3 ELSE finished_at END
             WHERE id = ?1 AND state IN ('pending', 'running') AND (?2 IS NULL OR created_by = ?2)",
            params![id, scope, now()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row("SELECT state FROM tasks WHERE id = ?1", [id], |r| r.get(0))
            .optional()?)
    }

    /// Tasks left running by a process that ended: they did not finish.
    pub fn tasks_interrupted(&self) -> Result<Vec<Task>> {
        let conn = self.runtime();
        let mut stmt = conn.prepare(&format!(
            "UPDATE tasks SET state = 'failed', result = 'interrupted: the Gateway stopped while it ran',
             finished_at = ?1 WHERE state IN ('running', 'cancelling') RETURNING {COLUMNS}"
        ))?;
        let found = stmt
            .query_map([now()], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }
}

/// The prompt of the task's own turn.
pub fn work_prompt(task: &Task) -> String {
    format!(
        "{PROMPT_PREFIX}#{} {:?}] Work on this on your own, in the background: nobody is \
         reading along and nobody can answer questions, so decide yourself and note what you \
         assumed. Use as many steps as it needs. Save long results (documents, tables) as files \
         in the workspace. End with the result in full: it is passed on to the conversation \
         where this was asked.\n\n{}",
        task.id, task.title, task.prompt
    )
}

/// The note that brings a finished task back to the conversation.
pub fn done_prompt(task: &Task, state: &str, result: &str) -> String {
    let mut shown: String = result.chars().take(MAX_RESULT_CHARS).collect();
    if result.chars().count() > MAX_RESULT_CHARS {
        shown.push_str(&format!(
            "\n[… cut; the whole result is in the conversation {}]",
            task.own_session()
        ));
    }
    let how = match state {
        "done" => "finished",
        "cancelled" => "was cancelled",
        _ => "failed",
    };
    format!(
        "{DONE_PREFIX}#{} {:?}] The background task they asked for {how}. Tell them now, in \
         your own words and briefly: what came out (or what went wrong), and where any files \
         are. Do not start it again unless they ask.\n\n{shown}",
        task.id, task.title
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_are_queued_claimed_finished_and_cancelled() {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .task_add("周报", "整理一周资料", "qq:c2c:ME", "qq:ME", 2)
            .unwrap();
        assert_eq!(a.state, "pending");
        assert_eq!(a.own_session(), format!("task:{}", a.id));
        let b = store.task_add("b", "do b", "main", "qq:ME", 2).unwrap();
        assert!(store.task_add("c", "do c", "main", "qq:ME", 2).is_err());
        assert!(store.task_add(" ", "x", "main", "cli", 2).is_err());
        let claimed = store.task_claim(1).unwrap();
        assert_eq!(claimed.iter().map(|t| t.id).collect::<Vec<_>>(), [a.id]);
        assert_eq!(store.tasks_running().unwrap(), 1);
        // Waiting: cancelled at once; running: when the Gateway looks.
        assert_eq!(
            store.task_cancel(b.id, Some("qq:ME")).unwrap().as_deref(),
            Some("cancelled")
        );
        assert_eq!(store.task_cancel(a.id, Some("qq:OTHER")).unwrap(), None);
        assert_eq!(
            store.task_cancel(a.id, None).unwrap().as_deref(),
            Some("cancelling")
        );
        assert_eq!(store.tasks_cancelling().unwrap().len(), 1);
        store.task_finish(a.id, "cancelled", "stopped").unwrap();
        assert!(!store.task(a.id).unwrap().unwrap().open());
        assert_eq!(store.tasks(Some("qq:ME"), 10).unwrap().len(), 2);
        assert!(store.task_claim(5).unwrap().is_empty());
    }

    #[test]
    fn a_restart_fails_what_was_running() {
        let store = Store::open_in_memory().unwrap();
        let a = store.task_add("a", "x", "main", "cli", 5).unwrap();
        store.task_claim(1).unwrap();
        let interrupted = store.tasks_interrupted().unwrap();
        assert_eq!(interrupted[0].id, a.id);
        assert_eq!(store.task(a.id).unwrap().unwrap().state, "failed");
    }

    #[test]
    fn long_results_are_cut_when_passed_back() {
        let store = Store::open_in_memory().unwrap();
        let task = store.task_add("a", "x", "main", "cli", 5).unwrap();
        let note = done_prompt(&task, "done", &"字".repeat(MAX_RESULT_CHARS + 10));
        assert!(note.starts_with(DONE_PREFIX));
        assert!(note.contains(&format!("in the conversation task:{}", task.id)));
        assert!(work_prompt(&task).starts_with(PROMPT_PREFIX));
    }
}
