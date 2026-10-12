//! `image_generate`: draws pictures with a ChatGPT plan's image model
//! (the Codex backend's own image endpoints) or one of OpenRouter's image
//! models.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::config::Config;
use crate::onboard::ModelInfo;

/// The image model of a ChatGPT plan; the Codex backend offers no other.
pub const CHATGPT_MODEL: &str = "gpt-image-2";

/// Pictures the ChatGPT image endpoint takes along with a prompt.
const MAX_REFERENCES: usize = 5;

/// Where generated pictures are saved, relative to the workspace.
pub const DIR: &str = "images";

pub struct ImageGen {
    http: reqwest::Client,
    backend: Backend,
}

enum Backend {
    Chatgpt {
        auth: Arc<crate::chatgpt::Auth>,
        base: String,
    },
    OpenRouter {
        base: String,
        model: String,
        key: String,
    },
}

/// What to draw.
#[derive(Debug, Default)]
pub struct Request {
    pub prompt: String,
    /// Such as `1:1`, `16:9`, `9:16`.
    pub aspect_ratio: Option<String>,
    pub transparent: bool,
    /// Pictures to edit or draw from: MIME type and bytes.
    pub references: Vec<(String, Vec<u8>)>,
}

/// A picture that came back.
pub struct Image {
    pub mime: String,
    pub data: Vec<u8>,
    /// US dollars, when the provider says.
    pub cost: Option<f64>,
}

impl Image {
    pub fn extension(&self) -> &'static str {
        match self.mime.as_str() {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "png",
        }
    }
}

impl ImageGen {
    /// `None` when no image model is set or the one set cannot be reached
    /// from this setup (an OpenRouter model without an OpenRouter key).
    pub fn new(config: &Config, api_key: &str) -> Result<Option<Self>> {
        let Some(model) = config.image_model() else {
            return Ok(None);
        };
        let backend = if model.contains('/') {
            if api_key.is_empty() {
                eprintln!(
                    "image: {model} runs on OpenRouter, which needs an OpenRouter key; image_generate is off"
                );
                return Ok(None);
            }
            Backend::OpenRouter {
                base: config
                    .openrouter_model()
                    .base_url
                    .trim_end_matches('/')
                    .to_owned(),
                model,
                key: api_key.to_owned(),
            }
        } else {
            let base = if config.chatgpt() {
                crate::llm::chatgpt_base(&config.model)
            } else {
                crate::chatgpt::ENDPOINT
            };
            Backend::Chatgpt {
                auth: crate::chatgpt::Auth::shared(
                    &crate::chatgpt::auth_path(&crate::config::state_dir()?),
                    crate::chatgpt::ISSUER,
                ),
                base: base.trim_end_matches('/').to_owned(),
            }
        };
        let http = crate::net::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(config.image.timeout_secs.max(30)))
            .build()
            .context("cannot build HTTP client for image generation")?;
        Ok(Some(Self { http, backend }))
    }

    /// The model, as the tool description names it.
    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::Chatgpt { .. } => format!("{CHATGPT_MODEL} on the ChatGPT plan"),
            Backend::OpenRouter { model, .. } => format!("{model} on OpenRouter"),
        }
    }

    pub fn model(&self) -> &str {
        match &self.backend {
            Backend::Chatgpt { .. } => CHATGPT_MODEL,
            Backend::OpenRouter { model, .. } => model,
        }
    }

    pub async fn generate(&self, request: &Request) -> Result<Image> {
        match &self.backend {
            Backend::Chatgpt { auth, base } => self.chatgpt(auth, base, request).await,
            Backend::OpenRouter { base, model, key } => {
                self.openrouter(base, model, key, request).await
            }
        }
    }

    /// The Codex backend's `images/generations`, or `images/edits` with
    /// reference pictures. It decides size and quality itself, so the
    /// aspect ratio goes into the prompt.
    async fn chatgpt(
        &self,
        auth: &crate::chatgpt::Auth,
        base: &str,
        request: &Request,
    ) -> Result<Image> {
        let body = chatgpt_body(request);
        let path = if request.references.is_empty() {
            "images/generations"
        } else {
            "images/edits"
        };
        let mut force = false;
        loop {
            let (token, account) = auth.access(force).await?;
            let mut call = self
                .http
                .post(format!("{base}/{path}"))
                .bearer_auth(token)
                .header("originator", "codex_cli_rs")
                .json(&body);
            if let Some(account) = account {
                call = call.header("chatgpt-account-id", account);
            }
            let response = call
                .send()
                .await
                .context("cannot reach ChatGPT's image service")?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.as_u16() == 401 && !force {
                force = true;
                continue;
            }
            if !status.is_success() {
                let (message, _) = crate::chatgpt::error_message(status, &text);
                bail!(message);
            }
            let value: Value =
                serde_json::from_str(&text).context("ChatGPT's image answer is not JSON")?;
            return first_image(&value, None).await;
        }
    }

    /// OpenRouter's `images` endpoint.
    async fn openrouter(
        &self,
        base: &str,
        model: &str,
        key: &str,
        request: &Request,
    ) -> Result<Image> {
        let response = self
            .http
            .post(format!("{base}/images"))
            .bearer_auth(key)
            .json(&openrouter_body(model, request))
            .send()
            .await
            .context("cannot reach OpenRouter's image service")?;
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map_or_else(|| value.to_string(), str::to_owned);
            bail!("image request failed with HTTP {status}: {message}");
        }
        first_image(&value, Some(&self.http)).await
    }
}

fn data_url(mime: &str, data: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    )
}

fn chatgpt_body(request: &Request) -> Value {
    let prompt = match request.aspect_ratio.as_deref().map(str::trim) {
        Some(ratio) if !ratio.is_empty() => {
            format!("Aspect ratio {ratio}.\n\n{}", request.prompt)
        }
        _ => request.prompt.clone(),
    };
    let mut body = json!({
        "prompt": prompt,
        "model": CHATGPT_MODEL,
        "size": "auto",
        "quality": "auto",
        "background": if request.transparent { "transparent" } else { "auto" },
    });
    if !request.references.is_empty() {
        body["images"] = request
            .references
            .iter()
            .take(MAX_REFERENCES)
            .map(|(mime, data)| json!({ "image_url": data_url(mime, data) }))
            .collect();
    }
    body
}

fn openrouter_body(model: &str, request: &Request) -> Value {
    let mut body = json!({ "model": model, "prompt": request.prompt });
    if let Some(ratio) = request
        .aspect_ratio
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        body["aspect_ratio"] = json!(ratio);
    }
    if request.transparent {
        body["background"] = json!("transparent");
    }
    if !request.references.is_empty() {
        body["input_references"] = request
            .references
            .iter()
            .map(|(mime, data)| json!(data_url(mime, data)))
            .collect();
    }
    body
}

/// The first picture of an images answer: `data[0].b64_json`, or its `url`
/// when `http` may download it.
async fn first_image(value: &Value, http: Option<&reqwest::Client>) -> Result<Image> {
    let item = value
        .pointer("/data/0")
        .context("the image service returned no picture")?;
    let cost = value.pointer("/usage/cost").and_then(Value::as_f64);
    let declared = item
        .get("media_type")
        .or_else(|| item.get("mime_type"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("the picture is not valid base64")?;
        let mime = declared.unwrap_or_else(|| sniff(&data).to_owned());
        return Ok(Image { mime, data, cost });
    }
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .context("the image service returned no picture")?;
    if let Some(rest) = url.strip_prefix("data:") {
        let (head, b64) = rest.split_once(',').context("bad data URL")?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("the picture is not valid base64")?;
        let mime = head
            .split(';')
            .next()
            .filter(|m| m.starts_with("image/"))
            .map_or_else(|| sniff(&data).to_owned(), str::to_owned);
        return Ok(Image { mime, data, cost });
    }
    let http = http.context("the image service returned a link instead of a picture")?;
    let data = http
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("cannot download the picture")?
        .bytes()
        .await?
        .to_vec();
    let mime = declared.unwrap_or_else(|| sniff(&data).to_owned());
    Ok(Image { mime, data, cost })
}

/// The image type, from the first bytes.
pub fn sniff(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "image/png"
    }
}

/// Saves `image` under the workspace's `images/`; returns the relative path.
pub async fn save(workspace: &Path, image: &Image) -> Result<String> {
    tokio::fs::create_dir_all(workspace.join(DIR)).await?;
    let relative = format!(
        "{DIR}/{}.{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f"),
        image.extension()
    );
    tokio::fs::write(workspace.join(&relative), &image.data)
        .await
        .with_context(|| format!("cannot save {relative}"))?;
    Ok(relative)
}

/// OpenRouter's image models: `images/models`, else the catalog filtered to
/// image output.
pub async fn openrouter_models(base_url: &str) -> Result<Vec<ModelInfo>> {
    let base = base_url.trim_end_matches('/');
    let http = crate::net::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let get = |url: String| {
        let http = http.clone();
        async move {
            let value: Value = http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::Ok(value)
        }
    };
    let mut models = match get(format!("{base}/images/models")).await {
        Ok(body) => parse_models(&body),
        Err(_) => Vec::new(),
    };
    if models.is_empty() {
        let body = get(format!("{base}/models?output_modalities=image")).await?;
        models = parse_models(&body);
    }
    if models.is_empty() {
        bail!("OpenRouter lists no image models");
    }
    Ok(models)
}

fn parse_models(body: &Value) -> Vec<ModelInfo> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| {
            // The general catalog also holds models that only read images.
            m.pointer("/architecture/output_modalities")
                .and_then(Value::as_array)
                .is_none_or(|out| out.iter().any(|v| v.as_str() == Some("image")))
        })
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            Some(ModelInfo {
                name: m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                images: m
                    .pointer("/architecture/input_modalities")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("image"))),
                id,
                ..ModelInfo::default()
            })
        })
        .collect()
}

/// The ChatGPT plan's image model, as the model list shows it.
pub fn chatgpt_entry() -> ModelInfo {
    ModelInfo {
        id: CHATGPT_MODEL.into(),
        name: format!("ChatGPT · {CHATGPT_MODEL}"),
        images: true,
        ..ModelInfo::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_puts_the_aspect_ratio_in_the_prompt_and_edits_with_references() {
        let request = Request {
            prompt: "a cat".into(),
            aspect_ratio: Some("16:9".into()),
            transparent: true,
            references: vec![("image/png".into(), vec![1, 2, 3])],
        };
        let body = chatgpt_body(&request);
        assert_eq!(body["model"], CHATGPT_MODEL);
        assert_eq!(body["prompt"], "Aspect ratio 16:9.\n\na cat");
        assert_eq!(body["background"], "transparent");
        assert_eq!(body["images"][0]["image_url"], "data:image/png;base64,AQID");
        let plain = chatgpt_body(&Request {
            prompt: "a dog".into(),
            ..Request::default()
        });
        assert_eq!(plain["prompt"], "a dog");
        assert_eq!(plain["background"], "auto");
        assert!(plain.get("images").is_none());
    }

    #[test]
    fn openrouter_takes_the_ratio_and_references_as_fields() {
        let request = Request {
            prompt: "a cat".into(),
            aspect_ratio: Some("9:16".into()),
            transparent: false,
            references: vec![("image/jpeg".into(), vec![1, 2, 3])],
        };
        let body = openrouter_body("google/x-image", &request);
        assert_eq!(body["model"], "google/x-image");
        assert_eq!(body["aspect_ratio"], "9:16");
        assert!(body.get("background").is_none());
        assert_eq!(body["input_references"][0], "data:image/jpeg;base64,AQID");
    }

    #[tokio::test]
    async fn the_first_picture_is_read_from_base64_or_a_data_url() {
        let png = json!({"data": [{"b64_json": "iVBORw0KGgo="}], "usage": {"cost": 0.04}});
        let image = first_image(&png, None).await.unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.extension(), "png");
        assert_eq!(image.cost, Some(0.04));
        let jpeg = json!({"data": [{"url": "data:image/jpeg;base64,/9j/4A=="}]});
        let image = first_image(&jpeg, None).await.unwrap();
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(image.extension(), "jpg");
        assert!(first_image(&json!({"data": []}), None).await.is_err());
    }

    #[test]
    fn image_models_come_from_either_list() {
        let own = json!({"data": [
            {"id": "google/gemini-image", "name": "Gemini Image",
             "architecture": {"input_modalities": ["text", "image"]}},
        ]});
        let models = parse_models(&own);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "google/gemini-image");
        assert!(models[0].images);
        let catalog = json!({"data": [
            {"id": "a/draws", "architecture": {"output_modalities": ["image", "text"]}},
            {"id": "b/reads", "architecture": {"output_modalities": ["text"]}},
        ]});
        let ids: Vec<String> = parse_models(&catalog).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["a/draws"]);
    }
}
