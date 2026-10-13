//! Memories found by meaning. Each memory is embedded once, with the model
//! in `[embedding]`, and its vector kept in `soul.sqlite`; recall and
//! `memory_search` embed the query and mix cosine similarity with the
//! keyword (trigram) index, so "我最近睡不好" finds "失眠" and an exact
//! name still wins.
//!
//! Without a model, or when the embedding service fails, search is the
//! keyword index alone, as before.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde_json::{Value, json};

use crate::llm::Usage;
use crate::memory::Memory;
use crate::onboard::ModelInfo;
use crate::store::Store;

/// Memories embedded per request when catching up.
const BATCH: usize = 32;

pub struct Embedder {
    http: reqwest::Client,
    base: String,
    key: String,
    pub model: String,
    /// Cosine similarity a memory needs to be recalled by meaning alone.
    pub min_similarity: f32,
}

static CURRENT: RwLock<Option<Arc<Embedder>>> = RwLock::new(None);

/// The embedder this process uses, when one is set up.
pub fn current() -> Option<Arc<Embedder>> {
    CURRENT.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Sets (or, with `None`, clears) the embedder this process uses.
pub fn set(embedder: Option<Embedder>) {
    *CURRENT.write().unwrap_or_else(|p| p.into_inner()) = embedder.map(Arc::new);
}

impl Embedder {
    /// `None` without a model, or without the OpenRouter key it needs.
    pub fn new(config: &crate::config::Config, api_key: &str) -> Result<Option<Self>> {
        let Some(model) = config.embedding.model() else {
            return Ok(None);
        };
        if api_key.is_empty() {
            eprintln!(
                "embedding: {model} runs on OpenRouter, which needs an OpenRouter key; memories are searched by keyword only"
            );
            return Ok(None);
        }
        let http = crate::net::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(config.embedding.timeout_secs.max(5)))
            .build()
            .context("cannot build HTTP client for embeddings")?;
        Ok(Some(Self {
            http,
            base: config
                .openrouter_model()
                .base_url
                .trim_end_matches('/')
                .to_owned(),
            key: api_key.to_owned(),
            model: model.to_owned(),
            min_similarity: config.embedding.min_similarity,
        }))
    }

    /// One vector per text, in order, and what the call cost.
    pub async fn embed(&self, texts: &[String]) -> Result<(Vec<Vec<f32>>, Option<Usage>)> {
        if texts.is_empty() {
            return Ok((Vec::new(), None));
        }
        let response = self
            .http
            .post(format!("{}/embeddings", self.base))
            .bearer_auth(&self.key)
            .header("X-Title", "Runo")
            .json(&json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .context("embedding request failed")?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() || body.get("error").is_some() {
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map_or_else(|| body.to_string(), str::to_owned);
            bail!("{} answered HTTP {status}: {message}", self.model);
        }
        let vectors = parse_vectors(&body, texts.len())?;
        let usage = body
            .get("usage")
            .filter(|u| u.is_object())
            .map(Usage::parse);
        Ok((vectors, usage))
    }

    async fn embed_one(&self, text: &str) -> Result<(Vec<f32>, Option<Usage>)> {
        let (mut vectors, usage) = self.embed(&[text.to_owned()]).await?;
        Ok((vectors.pop().context("no embedding came back")?, usage))
    }
}

/// `data[].embedding` of an embeddings answer, ordered by `index`.
fn parse_vectors(body: &Value, expected: usize) -> Result<Vec<Vec<f32>>> {
    let mut items: Vec<(usize, Vec<f32>)> = body
        .get("data")
        .and_then(Value::as_array)
        .context("the embedding answer has no data")?
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .map_or(position, |i| i as usize);
            let vector = item
                .get("embedding")
                .and_then(Value::as_array)
                .context("an embedding is missing")?
                .iter()
                .map(|v| v.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
                .context("an embedding is not a list of numbers")?;
            Ok((index, vector))
        })
        .collect::<Result<_>>()?;
    items.sort_by_key(|(index, _)| *index);
    if items.len() != expected {
        bail!("asked for {expected} embeddings, got {}", items.len());
    }
    Ok(items.into_iter().map(|(_, v)| v).collect())
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

impl Store {
    /// Memories with no vector from `model` yet, oldest first.
    pub fn memories_unembedded(&self, model: &str, limit: usize) -> Result<Vec<(i64, String)>> {
        let conn = self.soul();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.content FROM memories m
             LEFT JOIN memory_vectors v ON v.memory_id = m.id AND v.model = ?1
             WHERE v.memory_id IS NULL ORDER BY m.id LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![model, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Whether any memory has a vector from `model`.
    pub fn memory_vectors_any(&self, model: &str) -> Result<bool> {
        Ok(self.soul().query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_vectors WHERE model = ?1)",
            [model],
            |r| r.get(0),
        )?)
    }

    pub fn memory_vector_set(&self, id: i64, model: &str, vector: &[f32]) -> Result<()> {
        self.soul().execute(
            "INSERT INTO memory_vectors(memory_id, model, vector) VALUES (?1, ?2, ?3)
             ON CONFLICT(memory_id) DO UPDATE SET model = excluded.model, vector = excluded.vector",
            params![id, model, to_blob(vector)],
        )?;
        Ok(())
    }

    /// Memories within `scope` with a vector from `model`.
    pub fn memory_vectors(
        &self,
        model: &str,
        scope: Option<&str>,
    ) -> Result<Vec<(Memory, Vec<f32>)>> {
        let conn = self.soul();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.content, m.created_at, v.vector FROM memories m
             JOIN memory_vectors v ON v.memory_id = m.id
             WHERE v.model = ?1 AND (?2 IS NULL OR m.created_by = ?2)",
        )?;
        let rows = stmt
            .query_map(params![model, scope], |r| {
                Ok((
                    Memory {
                        id: r.get(0)?,
                        content: r.get(1)?,
                        created_at: r.get(2)?,
                    },
                    from_blob(&r.get::<_, Vec<u8>>(3)?),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

/// Reciprocal-rank fusion of the keyword hits (best first) and the memories
/// close in meaning (with their similarity): a memory found both ways
/// ranks above one found either way. Meaning alone needs `min_similarity`.
pub fn fuse(
    keyword: Vec<Memory>,
    by_meaning: Vec<(Memory, f32)>,
    min_similarity: f32,
    limit: usize,
) -> Vec<Memory> {
    const K: f32 = 60.0;
    let mut scored: Vec<(f32, Memory)> = Vec::new();
    for (rank, memory) in keyword.into_iter().enumerate() {
        scored.push((1.0 / (K + rank as f32), memory));
    }
    let mut close = by_meaning;
    close.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (rank, (memory, similarity)) in close.into_iter().enumerate() {
        let score = 1.0 / (K + rank as f32);
        match scored.iter_mut().find(|(_, m)| m.id == memory.id) {
            Some(found) => found.0 += score,
            None if similarity >= min_similarity => scored.push((score, memory)),
            None => {}
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().take(limit).map(|(_, m)| m).collect()
}

/// The memories within `scope` closest in meaning to `vector`, best first.
fn nearest(
    store: &Store,
    model: &str,
    vector: &[f32],
    scope: Option<&str>,
    limit: usize,
) -> Result<Vec<(Memory, f32)>> {
    let mut all: Vec<(Memory, f32)> = store
        .memory_vectors(model, scope)?
        .into_iter()
        .map(|(m, v)| {
            let similarity = cosine(vector, &v);
            (m, similarity)
        })
        .collect();
    all.sort_by(|a, b| b.1.total_cmp(&a.1));
    all.truncate(limit);
    Ok(all)
}

/// Records what an embedding call cost under the current session.
fn record(store: &Store, embedder: &Embedder, usage: Option<Usage>) {
    let Some(usage) = usage else {
        return;
    };
    let session = crate::agent::current_session().unwrap_or_default();
    let actor = crate::access::current().map(|a| a.id);
    if let Err(err) = store.record_usage(
        &session,
        "embedding",
        actor.as_deref(),
        Some(&embedder.model),
        &usage,
    ) {
        eprintln!("usage: cannot record an embedding: {err:#}");
    }
}

/// Keyword hits fused with the memories close in meaning to `query`. With
/// no embedder, or when it fails, the keyword hits alone.
pub async fn search(
    store: &Store,
    query: &str,
    keyword: Vec<Memory>,
    scope: Option<&str>,
    limit: usize,
) -> Vec<Memory> {
    let Some(embedder) = current() else {
        return keyword.into_iter().take(limit).collect();
    };
    // Nothing to compare with: no call.
    if !store.memory_vectors_any(&embedder.model).unwrap_or(false) {
        return keyword.into_iter().take(limit).collect();
    }
    match embedder.embed_one(query).await {
        Ok((vector, usage)) => {
            record(store, &embedder, usage);
            match nearest(store, &embedder.model, &vector, scope, limit * 2) {
                Ok(close) => fuse(keyword, close, embedder.min_similarity, limit),
                Err(err) => {
                    eprintln!("embedding: cannot read memory vectors: {err:#}");
                    keyword.into_iter().take(limit).collect()
                }
            }
        }
        Err(err) => {
            eprintln!("embedding: cannot embed the query: {err:#}");
            keyword.into_iter().take(limit).collect()
        }
    }
}

/// Embeds every memory without a vector from the current model, a batch at
/// a time; stops at the first failure and tries again next time. One runs
/// at a time; a call while one runs returns at once.
pub async fn catch_up(store: &Store, embedder: &Embedder) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            RUNNING.store(false, Ordering::Release);
        }
    }
    let _done = Done;
    loop {
        let pending = match store.memories_unembedded(&embedder.model, BATCH) {
            Ok(pending) if !pending.is_empty() => pending,
            Ok(_) => return,
            Err(err) => {
                eprintln!("embedding: cannot list memories: {err:#}");
                return;
            }
        };
        let texts: Vec<String> = pending.iter().map(|(_, t)| t.clone()).collect();
        match embedder.embed(&texts).await {
            Ok((vectors, usage)) => {
                record(store, embedder, usage);
                for ((id, _), vector) in pending.iter().zip(vectors) {
                    if let Err(err) = store.memory_vector_set(*id, &embedder.model, &vector) {
                        eprintln!("embedding: cannot save the vector of #{id}: {err:#}");
                        return;
                    }
                }
                if pending.len() < BATCH {
                    return;
                }
            }
            Err(err) => {
                eprintln!("embedding: cannot embed memories: {err:#}");
                return;
            }
        }
    }
}

/// OpenRouter's embedding models: `embeddings/models`, else the catalog
/// filtered to embedding output.
pub async fn openrouter_models(base_url: &str) -> Result<Vec<ModelInfo>> {
    let base = base_url.trim_end_matches('/');
    let http = crate::net::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let mut last = None;
    for url in [
        format!("{base}/embeddings/models"),
        format!("{base}/models?output_modalities=embeddings"),
    ] {
        let body = async {
            let value: Value = http
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::Ok(value)
        }
        .await;
        match body {
            Ok(body) => {
                let models = crate::tts::parse_catalog(&body, "embeddings");
                if !models.is_empty() {
                    return Ok(models.into_iter().map(|(m, _)| m).collect());
                }
            }
            Err(err) => last = Some(err),
        }
    }
    match last {
        Some(err) => Err(err),
        None => bail!("OpenRouter lists no embedding models"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: i64) -> Memory {
        Memory {
            id,
            content: format!("m{id}"),
            created_at: 0,
        }
    }

    #[test]
    fn vectors_round_trip_and_compare() {
        let v = vec![0.5, -1.25, 3.0];
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn answers_are_read_in_index_order() {
        let body = json!({"data": [
            {"index": 1, "embedding": [0.0, 1.0]},
            {"index": 0, "embedding": [1.0, 0.0]},
        ]});
        assert_eq!(
            parse_vectors(&body, 2).unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        assert!(parse_vectors(&body, 3).is_err());
        assert!(parse_vectors(&json!({"error": {}}), 1).is_err());
    }

    #[test]
    fn fusion_ranks_memories_found_both_ways_first() {
        // 1: keyword only; 2: both; 3: meaning only, close; 4: meaning only, far.
        let fused = fuse(
            vec![memory(1), memory(2)],
            vec![(memory(3), 0.8), (memory(2), 0.7), (memory(4), 0.2)],
            0.5,
            10,
        );
        let ids: Vec<i64> = fused.iter().map(|m| m.id).collect();
        assert_eq!(ids[0], 2);
        assert!(ids.contains(&1) && ids.contains(&3));
        assert!(!ids.contains(&4));
        assert_eq!(fuse(vec![memory(1)], vec![], 0.5, 10).len(), 1);
    }

    #[test]
    fn vectors_belong_to_their_model_and_go_with_the_memory() {
        let store = Store::open_in_memory().unwrap();
        let a = store.memory_save("失眠了好几天").unwrap();
        let b = store.memory_save_by("访客喜欢猫", Some("qq:X")).unwrap();
        assert_eq!(store.memories_unembedded("m1", 10).unwrap().len(), 2);
        store.memory_vector_set(a, "m1", &[1.0, 0.0]).unwrap();
        assert_eq!(
            store.memories_unembedded("m1", 10).unwrap(),
            vec![(b, "访客喜欢猫".to_owned())]
        );
        // Another model needs its own.
        assert_eq!(store.memories_unembedded("m2", 10).unwrap().len(), 2);
        store.memory_vector_set(b, "m1", &[0.0, 1.0]).unwrap();
        assert_eq!(store.memory_vectors("m1", None).unwrap().len(), 2);
        assert_eq!(store.memory_vectors("m1", Some("qq:X")).unwrap().len(), 1);
        let close = nearest(&store, "m1", &[0.9, 0.1], None, 1).unwrap();
        assert_eq!(close[0].0.id, a);
        store.memory_delete(a).unwrap();
        assert_eq!(store.memory_vectors("m1", None).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memories_are_embedded_and_found_by_meaning() {
        use axum::{Json, Router, routing::post};
        // Texts about sleep point one way, everything else the other.
        let app = Router::new().route(
            "/embeddings",
            post(|Json(body): Json<Value>| async move {
                let data: Vec<Value> = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let t = t.as_str().unwrap();
                        let sleep = t.contains("睡") || t.contains("失眠");
                        json!({"index": i, "embedding": if sleep { [1.0, 0.1] } else { [0.1, 1.0] }})
                    })
                    .collect();
                Json(json!({"data": data, "usage": {"prompt_tokens": 3, "cost": 0.000001}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = crate::config::Config::default();
        config.model.base_url = format!("http://{addr}");
        config.embedding.model = Some("test/embed".into());
        let embedder = Embedder::new(&config, "k").unwrap().unwrap();
        let store = Store::open_in_memory().unwrap();
        let insomnia = store.memory_save("用户最近失眠").unwrap();
        store.memory_save("用户喜欢乌龙茶").unwrap();
        catch_up(&store, &embedder).await;
        assert!(
            store
                .memories_unembedded("test/embed", 10)
                .unwrap()
                .is_empty()
        );
        let (vector, _) = embedder.embed_one("我最近睡不好").await.unwrap();
        let close = nearest(&store, "test/embed", &vector, None, 5).unwrap();
        let found = fuse(Vec::new(), close, 0.5, 5);
        assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), [insomnia]);
    }
}
