//! `voice_send`: text to speech with one of OpenRouter's speech models
//! (`/audio/speech`), sent as a voice message where the chat has them.
//!
//! The ChatGPT plan has no speech endpoint that works with its sign-in, so
//! speaking needs an OpenRouter key. Each model has its own voices; the
//! list comes from OpenRouter's catalog (`supported_voices`).

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::onboard::ModelInfo;

/// Where spoken messages are saved, relative to the workspace.
pub const DIR: &str = "voice";

pub struct Speaker {
    http: reqwest::Client,
    base: String,
    key: String,
    pub model: String,
    /// The voice used without a voice profile.
    default_voice: Option<String>,
    pub max_chars: usize,
    /// The model's voices and price per input unit, read once.
    catalog: Mutex<Option<Catalog>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub voices: Vec<String>,
    /// US dollars per input character (OpenRouter's `pricing.prompt`).
    pub price: Option<f64>,
}

/// A spoken message.
#[derive(Debug)]
pub struct Speech {
    pub mime: &'static str,
    pub data: Vec<u8>,
    /// US dollars, estimated from the model's listed price.
    pub cost: Option<f64>,
    pub voice: String,
}

impl Speaker {
    /// `None` when speaking is off, no model is set, or there is no
    /// OpenRouter key for it.
    pub fn new(config: &crate::config::Config, api_key: &str) -> Result<Option<Self>> {
        let Some(model) = config.speech.model() else {
            return Ok(None);
        };
        if api_key.is_empty() {
            eprintln!(
                "speech: {model} runs on OpenRouter, which needs an OpenRouter key; voice_send is off"
            );
            return Ok(None);
        }
        let http = crate::net::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(config.speech.timeout_secs.max(10)))
            .build()
            .context("cannot build HTTP client for speech")?;
        Ok(Some(Self {
            http,
            base: config
                .openrouter_model()
                .base_url
                .trim_end_matches('/')
                .to_owned(),
            key: api_key.to_owned(),
            model: model.to_owned(),
            default_voice: config
                .speech
                .voice
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
            max_chars: config.speech.max_chars.max(1),
            catalog: Mutex::new(None),
        }))
    }

    /// The model's voices and price, read from OpenRouter's catalog once;
    /// empty when the catalog cannot be read now (asked again next time).
    pub async fn catalog(&self) -> Catalog {
        if let Some(catalog) = self
            .catalog
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return catalog;
        }
        match speech_models(&self.base).await {
            Ok(models) => {
                let found = models
                    .into_iter()
                    .find(|(m, _)| m.id == self.model)
                    .map(|(m, voices)| Catalog {
                        voices,
                        price: m.price.map(|(input, _)| input / 1_000_000.0),
                    })
                    .unwrap_or_default();
                *self.catalog.lock().unwrap_or_else(|p| p.into_inner()) = Some(found.clone());
                found
            }
            Err(err) => {
                eprintln!("speech: cannot read the voices of {}: {err:#}", self.model);
                Catalog::default()
            }
        }
    }

    /// The voice to speak with: `wanted` when the model has it (or its
    /// voices are unknown), else `speech.voice`, else the model's first.
    pub async fn pick_voice(&self, wanted: Option<&str>) -> Result<String> {
        let catalog = self.catalog().await;
        let known = |v: &str| {
            catalog.voices.is_empty() || catalog.voices.iter().any(|k| k.eq_ignore_ascii_case(v))
        };
        for choice in [wanted, self.default_voice.as_deref()]
            .into_iter()
            .flatten()
        {
            let choice = choice.trim();
            if !choice.is_empty() && known(choice) {
                return Ok(catalog
                    .voices
                    .iter()
                    .find(|k| k.eq_ignore_ascii_case(choice))
                    .cloned()
                    .unwrap_or_else(|| choice.to_owned()));
            }
        }
        catalog
            .voices
            .first()
            .cloned()
            .with_context(|| format!("{} lists no voices; set speech.voice", self.model))
    }

    /// Speaks `text` as mp3 with `voice`, following `instructions` (tone,
    /// pace, emotion) on models that take them.
    pub async fn speak(
        &self,
        text: &str,
        voice: &str,
        instructions: Option<&str>,
    ) -> Result<Speech> {
        let text = text.trim();
        if text.is_empty() {
            bail!("nothing to say");
        }
        if text.chars().count() > self.max_chars {
            bail!(
                "longer than speech.max_chars ({}); say less, or split it",
                self.max_chars
            );
        }
        let response = self
            .http
            .post(format!("{}/audio/speech", self.base))
            .bearer_auth(&self.key)
            .header("X-Title", "Runo")
            .json(&request_body(&self.model, text, voice, instructions))
            .send()
            .await
            .context("cannot reach OpenRouter's speech service")?;
        let status = response.status();
        let kind = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let data = response
            .bytes()
            .await
            .context("speech interrupted")?
            .to_vec();
        if !status.is_success() || kind.starts_with("application/json") {
            let body: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map_or_else(
                    || String::from_utf8_lossy(&data).chars().take(300).collect(),
                    str::to_owned,
                );
            bail!("{} answered HTTP {status}: {message}", self.model);
        }
        if data.len() < 64 {
            bail!("{} returned no audio", self.model);
        }
        let price = self.catalog().await.price;
        Ok(Speech {
            mime: "audio/mpeg",
            data,
            cost: price.map(|p| p * text.chars().count() as f64),
            voice: voice.to_owned(),
        })
    }
}

fn request_body(model: &str, text: &str, voice: &str, instructions: Option<&str>) -> Value {
    let mut body = json!({
        "model": model,
        "input": text,
        "voice": voice,
        "response_format": "mp3",
    });
    if let Some(instructions) = instructions.map(str::trim).filter(|i| !i.is_empty()) {
        body["instructions"] = json!(instructions);
    }
    body
}

/// Saves `speech` under the workspace's `voice/`; returns the relative path.
pub async fn save(workspace: &Path, speech: &Speech) -> Result<String> {
    tokio::fs::create_dir_all(workspace.join(DIR)).await?;
    let relative = format!(
        "{DIR}/{}.mp3",
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
    );
    tokio::fs::write(workspace.join(&relative), &speech.data)
        .await
        .with_context(|| format!("cannot save {relative}"))?;
    Ok(relative)
}

/// The models of an OpenRouter catalog answer whose output is `output`,
/// each with its `supported_voices`.
pub fn parse_catalog(body: &Value, output: &str) -> Vec<(ModelInfo, Vec<String>)> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| {
            m.pointer("/architecture/output_modalities")
                .and_then(Value::as_array)
                .is_none_or(|out| out.iter().any(|v| v.as_str() == Some(output)))
        })
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let price = |key: &str| {
                m.pointer(&format!("/pricing/{key}"))
                    .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_f64()))
            };
            let voices = m
                .get("supported_voices")
                .and_then(Value::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some((
                ModelInfo {
                    name: m
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned(),
                    context: m.get("context_length").and_then(Value::as_u64).unwrap_or(0),
                    price: price("prompt").map(|input| {
                        (
                            input * 1_000_000.0,
                            price("completion").unwrap_or(0.0) * 1_000_000.0,
                        )
                    }),
                    id,
                    ..ModelInfo::default()
                },
                voices,
            ))
        })
        .collect()
}

/// OpenRouter's speech models with their voices.
pub async fn speech_models(base_url: &str) -> Result<Vec<(ModelInfo, Vec<String>)>> {
    let base = base_url.trim_end_matches('/');
    let http = crate::net::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let body: Value = http
        .get(format!("{base}/models?output_modalities=speech"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let models = parse_catalog(&body, "speech");
    if models.is_empty() {
        bail!("OpenRouter lists no speech models");
    }
    Ok(models)
}

/// OpenRouter's speech models, for the model list in `config`.
pub async fn openrouter_models(base_url: &str) -> Result<Vec<ModelInfo>> {
    Ok(speech_models(base_url)
        .await?
        .into_iter()
        .map(|(mut m, voices)| {
            if !voices.is_empty() {
                let shown: Vec<&str> = voices.iter().take(6).map(String::as_str).collect();
                let more = if voices.len() > 6 { ", …" } else { "" };
                m.name = format!("{} · {}{more}", m.name, shown.join(", "));
            }
            m
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_gives_voices_and_prices() {
        let body = json!({"data": [
            {"id": "x-ai/grok-voice-tts-1.0", "name": "Grok Voice",
             "architecture": {"output_modalities": ["speech"]},
             "pricing": {"prompt": "0.000015", "completion": "0"},
             "supported_voices": ["eve", "ara"]},
            {"id": "a/chat", "architecture": {"output_modalities": ["text"]}},
        ]});
        let models = parse_catalog(&body, "speech");
        assert_eq!(models.len(), 1);
        let (model, voices) = &models[0];
        assert_eq!(model.id, "x-ai/grok-voice-tts-1.0");
        assert_eq!(voices, &["eve", "ara"]);
        let (input, _) = model.price.unwrap();
        assert!((input - 15.0).abs() < 1e-9);
    }

    #[test]
    fn requests_ask_for_mp3_and_pass_instructions() {
        let body = request_body("m", "你好", "eve", Some(" 温柔、轻声 "));
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["voice"], "eve");
        assert_eq!(body["instructions"], "温柔、轻声");
        assert!(
            request_body("m", "hi", "eve", Some(" "))
                .get("instructions")
                .is_none()
        );
    }

    /// A stand-in for OpenRouter's catalog and speech endpoint.
    async fn server() -> String {
        use axum::{Json, Router, routing::get, routing::post};
        let app = Router::new()
            .route(
                "/models",
                get(|| async {
                    Json(json!({"data": [{"id": "t/tts", "architecture": {"output_modalities": ["speech"]},
                        "pricing": {"prompt": "0.00001"}, "supported_voices": ["eve", "ara"]}]}))
                }),
            )
            .route(
                "/audio/speech",
                post(|Json(body): Json<Value>| async move {
                    if body["voice"] == "nobody" {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            br#"{"error":{"message":"unknown voice"}}"#.to_vec(),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "audio/mpeg")],
                        vec![0xFF; 128],
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn speaks_with_a_voice_the_model_has() {
        let mut config = crate::config::Config::default();
        config.model.base_url = server().await;
        config.speech.model = Some("t/tts".into());
        let speaker = Speaker::new(&config, "k").unwrap().unwrap();
        assert_eq!(speaker.pick_voice(Some("ARA")).await.unwrap(), "ara");
        assert_eq!(speaker.pick_voice(Some("someone")).await.unwrap(), "eve");
        assert_eq!(speaker.pick_voice(None).await.unwrap(), "eve");
        let speech = speaker.speak("你好呀", "ara", None).await.unwrap();
        assert_eq!(speech.mime, "audio/mpeg");
        assert_eq!(speech.data.len(), 128);
        assert!((speech.cost.unwrap() - 0.00003).abs() < 1e-9);
        let err = speaker.speak("hi", "nobody", None).await.unwrap_err();
        assert!(format!("{err:#}").contains("unknown voice"), "{err:#}");
        assert!(speaker.speak(&"字".repeat(601), "eve", None).await.is_err());
        assert!(Speaker::new(&config, "").unwrap().is_none());
    }
}
