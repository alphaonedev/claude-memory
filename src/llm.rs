// Copyright (c) 2026 AlphaOne LLC. All rights reserved.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

const GENERATE_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_TIMEOUT: Duration = Duration::from_secs(120);

const QUERY_EXPANSION_PROMPT: &str = r#"You are a search query expander. Given a search query, generate 5-8 additional search terms that are semantically related. Return ONLY the terms, one per line, no numbering or explanation.

Query: {query}"#;

const SUMMARIZE_PROMPT: &str = r#"Summarize the following memories into a single concise paragraph. Preserve all key facts, decisions, and technical details.

{memories}"#;

const AUTO_TAG_PROMPT: &str = r#"Generate 3-5 short tags for categorizing this memory. Return ONLY the tags, one per line, lowercase, no symbols.

Title: {title}
Content: {content}"#;

const CONTRADICTION_PROMPT: &str = r#"Do these two statements contradict each other? Answer ONLY "yes" or "no".

Statement A: {a}
Statement B: {b}"#;

pub struct OllamaClient {
    base_url: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl OllamaClient {
    /// Creates a new OllamaClient with the default Ollama URL (http://localhost:11434).
    /// Checks that Ollama is reachable before returning.
    pub fn new(model: &str) -> Result<Self> {
        Self::new_with_url(DEFAULT_OLLAMA_URL, model)
    }

    /// Creates a new OllamaClient with a custom base URL.
    /// Checks that Ollama is reachable before returning.
    pub fn new_with_url(base_url: &str, model: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(GENERATE_TIMEOUT)
            .build()
            .context("Failed to build HTTP client")?;

        let instance = Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client,
        };

        if !instance.is_available() {
            return Err(anyhow!(
                "Ollama is not running or not reachable at {}. \
                 Start it with: ollama serve",
                instance.base_url
            ));
        }

        Ok(instance)
    }

    /// Quick health check -- returns true if Ollama responds to GET /api/tags.
    pub fn is_available(&self) -> bool {
        let url = format!("{}/api/tags", self.base_url);
        self.client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Checks if the configured model is already pulled. If not, pulls it.
    pub fn ensure_model(&self) -> Result<()> {
        // Check if model exists by listing tags
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .context("Failed to list Ollama models")?;

        let body: Value = resp.json().context("Failed to parse /api/tags response")?;

        let model_exists = body["models"]
            .as_array()
            .map(|models| {
                models.iter().any(|m| {
                    let name = m["name"].as_str().unwrap_or("");
                    // Match "model" or "model:tag" against our model string
                    name == self.model
                        || name.starts_with(&format!("{}:", self.model))
                        || self.model == name.split(':').next().unwrap_or("")
                })
            })
            .unwrap_or(false);

        if model_exists {
            return Ok(());
        }

        // Pull the model
        tracing::info!("Pulling Ollama model '{}' (this may take a while)...", self.model);

        let pull_url = format!("{}/api/pull", self.base_url);
        let pull_client = reqwest::blocking::Client::builder()
            .timeout(PULL_TIMEOUT)
            .build()
            .context("Failed to build pull client")?;

        let resp = pull_client
            .post(&pull_url)
            .json(&json!({ "name": self.model }))
            .send()
            .context("Failed to pull model from Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow!("Ollama pull failed ({}): {}", status, text));
        }

        tracing::info!("Model '{}' pulled successfully", self.model);
        Ok(())
    }

    /// Generates a completion from Ollama. Uses stream: false for simplicity.
    /// Returns the response text.
    pub fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);

        let mut payload = json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
        });

        if let Some(sys) = system {
            payload["system"] = Value::String(sys.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .timeout(GENERATE_TIMEOUT)
            .json(&payload)
            .send()
            .context("Failed to send generate request to Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow!(
                "Ollama generate failed ({}): {}",
                status,
                text
            ));
        }

        let body: Value = resp
            .json()
            .context("Failed to parse Ollama generate response")?;

        let response_text = body["response"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'response' field in Ollama output"))?
            .to_string();

        Ok(response_text)
    }

    /// Uses the LLM to expand a search query into additional search terms.
    pub fn expand_query(&self, query: &str) -> Result<Vec<String>> {
        let prompt = QUERY_EXPANSION_PROMPT.replace("{query}", query);
        let response = self.generate(&prompt, None)?;

        let terms: Vec<String> = response
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();

        Ok(terms)
    }

    /// Takes (title, content) pairs and returns a consolidated summary.
    pub fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
        let formatted = memories
            .iter()
            .enumerate()
            .map(|(i, (title, content))| {
                format!("--- Memory {} ---\nTitle: {}\n{}", i + 1, title, content)
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = SUMMARIZE_PROMPT.replace("{memories}", &formatted);
        let response = self.generate(&prompt, None)?;

        Ok(response.trim().to_string())
    }

    /// Generates suggested tags for a memory.
    pub fn auto_tag(&self, title: &str, content: &str) -> Result<Vec<String>> {
        let prompt = AUTO_TAG_PROMPT
            .replace("{title}", title)
            .replace("{content}", content);

        let response = self.generate(&prompt, None)?;

        let tags: Vec<String> = response
            .lines()
            .map(|line| line.trim().to_lowercase())
            .filter(|line| !line.is_empty())
            .collect();

        Ok(tags)
    }

    /// Returns true if two memory contents contradict each other.
    pub fn detect_contradiction(&self, mem_a: &str, mem_b: &str) -> Result<bool> {
        let prompt = CONTRADICTION_PROMPT
            .replace("{a}", mem_a)
            .replace("{b}", mem_b);

        let response = self.generate(&prompt, None)?;
        let answer = response.trim().to_lowercase();

        Ok(answer.starts_with("yes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_templates_have_placeholders() {
        assert!(QUERY_EXPANSION_PROMPT.contains("{query}"));
        assert!(SUMMARIZE_PROMPT.contains("{memories}"));
        assert!(AUTO_TAG_PROMPT.contains("{title}"));
        assert!(AUTO_TAG_PROMPT.contains("{content}"));
        assert!(CONTRADICTION_PROMPT.contains("{a}"));
        assert!(CONTRADICTION_PROMPT.contains("{b}"));
    }

    #[test]
    fn test_default_url() {
        assert_eq!(DEFAULT_OLLAMA_URL, "http://localhost:11434");
    }
}
