//! How hard the main model thinks (`model.reasoning_effort`). By default
//! (`auto`) the program finds the levels the model offers and takes the
//! lowest that every tool works with: `low` where it exists, since
//! `minimal` rules out ChatGPT's web search and `none` does not think.

use std::path::Path;

use crate::config::Config;

/// Every level, from no thinking to the most.
pub const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// Levels a reasoning model on OpenRouter accepts (it maps them for models
/// that take a token budget instead).
const OPENROUTER_LEVELS: &[&str] = &["minimal", "low", "medium", "high"];

/// The lowest of `levels` from `low` up; below that only when nothing
/// higher is offered, and never `none`.
pub fn lowest(levels: &[String]) -> Option<String> {
    let rank = |level: &str| LEVELS.iter().position(|l| l.eq_ignore_ascii_case(level));
    let low = rank("low").unwrap_or(0);
    let mut known: Vec<usize> = levels.iter().filter_map(|l| rank(l.trim())).collect();
    known.sort_unstable();
    known
        .iter()
        .find(|r| **r >= low)
        .or_else(|| known.iter().rev().find(|r| **r > 0))
        .map(|r| LEVELS[*r].to_owned())
}

/// What ChatGPT's models offer when its list cannot be read.
fn chatgpt_fallback(model: &str) -> Vec<String> {
    let model = model.to_ascii_lowercase();
    let levels: &[&str] = if model.contains("codex") {
        &["low", "medium", "high"]
    } else if model.starts_with("gpt-5") || model.starts_with('o') {
        &["minimal", "low", "medium", "high"]
    } else {
        &[]
    };
    levels.iter().map(|l| (*l).to_owned()).collect()
}

/// What `model.reasoning_effort` stands for now: a level to send, or `None`
/// to send none (the model's own default, or a model that does not think).
pub async fn effort(config: &Config, state: &Path) -> Option<String> {
    let set = config
        .model
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .unwrap_or("auto")
        .to_ascii_lowercase();
    match set.as_str() {
        "off" => None,
        "" | "auto" => lowest(&supported(config, state).await),
        level => Some(level.to_owned()),
    }
}

/// The levels the main model offers; empty when it does not think or it
/// is not known.
pub async fn supported(config: &Config, state: &Path) -> Vec<String> {
    let model = config.model.model.trim();
    if config.chatgpt() {
        let auth =
            crate::chatgpt::Auth::shared(&crate::chatgpt::auth_path(state), crate::chatgpt::ISSUER);
        let base = crate::llm::chatgpt_base(&config.model);
        let listed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::chatgpt::reasoning_levels(&auth, base, model),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
        return listed.unwrap_or_else(|| chatgpt_fallback(model));
    }
    match crate::onboard::reasons(config, state).await {
        Some(true) => OPENROUTER_LEVELS.iter().map(|l| (*l).to_owned()).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(list: &[&str]) -> Vec<String> {
        list.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn takes_the_lowest_level_from_low_up() {
        assert_eq!(
            lowest(&levels(&["high", "medium", "low"])).as_deref(),
            Some("low")
        );
        assert_eq!(
            lowest(&levels(&["minimal", "low", "medium", "high"])).as_deref(),
            Some("low")
        );
        assert_eq!(
            lowest(&levels(&["none", "low", "high"])).as_deref(),
            Some("low")
        );
        assert_eq!(
            lowest(&levels(&["medium", "xhigh"])).as_deref(),
            Some("medium")
        );
        assert_eq!(
            lowest(&levels(&["none", "minimal"])).as_deref(),
            Some("minimal")
        );
        assert_eq!(lowest(&levels(&["none"])), None);
        assert_eq!(lowest(&[]), None);
        assert_eq!(lowest(&levels(&["LOW", "turbo"])).as_deref(), Some("low"));
        assert_eq!(
            lowest(&chatgpt_fallback("gpt-5-codex")).as_deref(),
            Some("low")
        );
        assert_eq!(lowest(&chatgpt_fallback("gpt-4o")), None);
    }
}
