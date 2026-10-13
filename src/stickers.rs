//! The identity's own sticker library. A sticker is drawn once (with
//! `image_generate`, in one consistent style), saved with `sticker_save`
//! under a mood, and sent again with `sticker_send` whenever that mood
//! comes up: cheaper than drawing each time, and always the same face.
//! Stickers belong to the identity that saved them.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::store::{Store, now};

/// Where stickers are kept, relative to the workspace.
pub const DIR: &str = "stickers";
/// Longest mood name.
const MAX_MOOD_CHARS: usize = 24;
/// Longest description.
const MAX_DESCRIPTION_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Sticker {
    pub id: i64,
    pub mood: String,
    pub description: String,
    /// Relative to the workspace.
    pub path: String,
    pub uses: i64,
}

/// A mood as stickers are filed under it: trimmed and lower case.
pub fn mood_key(mood: &str) -> Result<String> {
    let mood = mood.trim().to_lowercase();
    if mood.is_empty() {
        bail!("the mood is empty");
    }
    if mood.chars().count() > MAX_MOOD_CHARS {
        bail!("name the mood in a word or two");
    }
    Ok(mood)
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Sticker> {
    Ok(Sticker {
        id: row.get(0)?,
        mood: row.get(1)?,
        description: row.get(2)?,
        path: row.get(3)?,
        uses: row.get(4)?,
    })
}

impl Store {
    pub fn image_asset_add(&self, path: &str, actor: &str, data: &[u8]) -> Result<()> {
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
        self.runtime().execute(
            "INSERT OR REPLACE INTO image_assets(path, created_by, digest) VALUES (?1, ?2, ?3)",
            params![path, actor, digest.as_ref()],
        )?;
        Ok(())
    }

    pub fn image_asset_digest(&self, path: &str, actor: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .runtime()
            .query_row(
                "SELECT digest FROM image_assets WHERE path = ?1 AND created_by = ?2",
                params![path, actor],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Files `path` (already in the library) as a sticker of `identity`.
    pub fn sticker_add(
        &self,
        identity: &str,
        mood: &str,
        description: &str,
        path: &str,
    ) -> Result<i64> {
        let description = description.trim();
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            bail!("keep the description under {MAX_DESCRIPTION_CHARS} characters");
        }
        let conn = self.soul();
        conn.execute(
            "INSERT INTO stickers (identity, mood, description, path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![identity.trim(), mood_key(mood)?, description, path, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The stickers of `identity` for `mood`, least used first.
    pub fn stickers_for(&self, identity: &str, mood: &str) -> Result<Vec<Sticker>> {
        let conn = self.soul();
        let mut stmt = conn.prepare(
            "SELECT id, mood, description, path, uses FROM stickers
             WHERE identity = ?1 AND mood = ?2
             ORDER BY uses, COALESCE(last_used, 0), id",
        )?;
        let found = stmt
            .query_map(params![identity.trim(), mood_key(mood)?], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    /// The moods `identity` has stickers for, with how many each.
    pub fn sticker_moods(&self, identity: &str) -> Result<Vec<(String, i64)>> {
        let conn = self.soul();
        let mut stmt = conn.prepare(
            "SELECT mood, count(*) FROM stickers WHERE identity = ?1 GROUP BY mood ORDER BY mood",
        )?;
        let found = stmt
            .query_map(params![identity.trim()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    pub fn sticker_used(&self, id: i64) -> Result<()> {
        self.soul().execute(
            "UPDATE stickers SET uses = uses + 1, last_used = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    /// Forgets sticker `id`; its path, so the file can go too.
    pub fn sticker_remove(&self, id: i64) -> Result<Option<String>> {
        let conn = self.soul();
        let path: Option<String> = conn
            .query_row("SELECT path FROM stickers WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        conn.execute("DELETE FROM stickers WHERE id = ?1", [id])?;
        Ok(path)
    }
}

/// Copies the authorized picture bytes into the library and files it; drops the
/// least used stickers of that mood beyond `per_mood`. Returns the new
/// sticker's id and path.
pub fn save(
    store: &Store,
    workspace: &Path,
    identity: &str,
    mood: &str,
    description: &str,
    data: &[u8],
    per_mood: usize,
) -> Result<(i64, String)> {
    let mood = mood_key(mood)?;
    let mime = crate::imagegen::sniff(data);
    if !looks_like_image(data) {
        bail!("sticker source is not a picture");
    }
    let extension = match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };
    std::fs::create_dir_all(workspace.join(DIR))?;
    let relative = format!(
        "{DIR}/{}-{}.{extension}",
        file_safe(&mood),
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
    );
    std::fs::write(workspace.join(&relative), data)
        .with_context(|| format!("cannot save {relative}"))?;
    let id = store.sticker_add(identity, &mood, description, &relative)?;
    // Keep the library small: the newest stays, the least used go.
    let all = store.stickers_for(identity, &mood)?;
    let extra = all.len().saturating_sub(per_mood.max(1));
    for old in all.iter().filter(|s| s.id != id).take(extra) {
        if let Some(path) = store.sticker_remove(old.id)? {
            let _ = std::fs::remove_file(workspace.join(path));
        }
    }
    Ok((id, relative))
}

fn looks_like_image(data: &[u8]) -> bool {
    data.starts_with(&[0x89, b'P', b'N', b'G'])
        || data.starts_with(&[0xFF, 0xD8, 0xFF])
        || data.starts_with(b"GIF8")
        || (data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP"))
}

/// A mood as part of a file name.
fn file_safe(mood: &str) -> String {
    let safe: String = mood
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    safe.trim_matches('-').to_owned()
}

/// The one to send for `mood`: the least used, so a mood with several
/// stickers takes turns.
pub fn pick(store: &Store, identity: &str, mood: &str) -> Result<Option<Sticker>> {
    Ok(store.stickers_for(identity, mood)?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    #[test]
    fn stickers_are_kept_per_identity_and_mood_and_take_turns() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, path) = save(&store, dir.path(), "小白", " Happy ", "跳起来笑", PNG, 2).unwrap();
        assert!(path.starts_with("stickers/happy-"), "{path}");
        assert_eq!(std::fs::read(dir.path().join(&path)).unwrap(), PNG);
        let (b, _) = save(&store, dir.path(), "小白", "happy", "比耶", PNG, 2).unwrap();
        assert_eq!(pick(&store, "小白", "HAPPY").unwrap().unwrap().id, a);
        store.sticker_used(a).unwrap();
        assert_eq!(pick(&store, "小白", "happy").unwrap().unwrap().id, b);
        assert!(pick(&store, "小黑", "happy").unwrap().is_none());
        assert_eq!(
            store.sticker_moods("小白").unwrap(),
            vec![("happy".to_owned(), 2)]
        );
        // A third beyond per_mood replaces the least used (b), file and all.
        let b_path = store.stickers_for("小白", "happy").unwrap()[0].path.clone();
        let (c, _) = save(&store, dir.path(), "小白", "happy", "转圈", PNG, 2).unwrap();
        let ids: Vec<i64> = store
            .stickers_for("小白", "happy")
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&c) && ids.contains(&a) && !ids.contains(&b));
        assert!(!dir.path().join(b_path).exists());
    }

    #[test]
    fn only_pictures_and_short_moods_are_saved() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(save(&store, dir.path(), "x", "happy", "", b"hello", 3).is_err());
        assert!(save(&store, dir.path(), "x", " ", "", PNG, 3).is_err());
        assert!(save(&store, dir.path(), "x", &"m".repeat(30), "", PNG, 3).is_err());
        assert_eq!(file_safe("开心/笑"), "开心-笑");
    }
}
