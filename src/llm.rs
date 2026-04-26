// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

const GENERATE_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_TIMEOUT: Duration = Duration::from_secs(120);

const QUERY_EXPANSION_PROMPT: &str = r"You are a search query expander. Given a search query, generate 5-8 additional search terms that are semantically related. Return ONLY the terms, one per line, no numbering or explanation.

Query: {query}";

const SUMMARIZE_PROMPT: &str = r"Summarize the following memories into a single concise paragraph. Preserve all key facts, decisions, and technical details.

{memories}";

const AUTO_TAG_PROMPT: &str = r"Generate 3-5 short tags for categorizing this memory. Return ONLY the tags, one per line, lowercase, no symbols.

Title: {title}
Content: {content}";

const CONTRADICTION_PROMPT: &str = r#"Do these two statements contradict each other? Answer ONLY "yes" or "no".

Statement A: {a}
Statement B: {b}"#;

pub struct OllamaClient {
    base_url: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl OllamaClient {
    /// Creates a new `OllamaClient` with the default Ollama URL (<http://localhost:11434>).
    /// Checks that Ollama is reachable before returning.
    #[allow(dead_code)]
    pub fn new(model: &str) -> Result<Self> {
        Self::new_with_url(DEFAULT_OLLAMA_URL, model)
    }

    /// Creates a new `OllamaClient` with a custom base URL.
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
            .is_ok_and(|r| r.status().is_success())
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

        let model_exists = body["models"].as_array().is_some_and(|models| {
            models.iter().any(|m| {
                let name = m["name"].as_str().unwrap_or("");
                // Match "model" or "model:tag" against our model string
                // Also match when our model base (before ':') matches the served name
                let our_base = self.model.split(':').next().unwrap_or(&self.model);
                name == self.model
                    || name.starts_with(&format!("{}:", self.model))
                    || self.model == name.split(':').next().unwrap_or("")
                    || name == our_base
            })
        });

        if model_exists {
            return Ok(());
        }

        // Pull the model
        tracing::info!(
            "Pulling Ollama model '{}' (this may take a while)...",
            self.model
        );

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
            return Err(anyhow!("Ollama pull failed ({status}): {text}"));
        }

        tracing::info!("Model '{}' pulled successfully", self.model);
        Ok(())
    }

    /// Generates a completion using the /api/chat endpoint (Ollama chat format).
    /// This is compatible with both Ollama and vMLX/OpenAI-compatible servers.
    /// Returns the response text.
    pub fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let url = format!("{}/api/chat", self.base_url);

        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(json!({"role": "system", "content": sys}));
        }
        messages.push(json!({"role": "user", "content": prompt}));

        let payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });

        let resp = self
            .client
            .post(&url)
            .timeout(GENERATE_TIMEOUT)
            .json(&payload)
            .send()
            .context("Failed to send chat request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow!("Chat generate failed ({status}): {text}"));
        }

        let body: Value = resp.json().context("Failed to parse chat response")?;

        // Ollama /api/chat returns {"message": {"content": "..."}}
        let response_text = body["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'message.content' field in chat output"))?
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

    /// Generate an embedding vector via Ollama's /api/embed endpoint.
    ///
    /// Used for nomic-embed-text-v1.5 on smart/autonomous tiers.
    pub fn embed_text(&self, text: &str, embed_model: &str) -> Result<Vec<f32>> {
        let url = format!("{}/api/embed", self.base_url);
        let payload = json!({
            "model": embed_model,
            "input": text,
        });

        let resp = self
            .client
            .post(&url)
            .timeout(GENERATE_TIMEOUT)
            .json(&payload)
            .send()
            .context("Failed to send embed request to Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow!("Ollama embed failed ({status}): {text}"));
        }

        let body: Value = resp
            .json()
            .context("Failed to parse Ollama embed response")?;

        // Ollama returns {"embeddings": [[...], ...]} — take the first one
        let embedding = body["embeddings"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("Missing embeddings in Ollama response"))?;

        #[allow(clippy::cast_possible_truncation)]
        let floats: Vec<f32> = embedding
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        if floats.is_empty() {
            return Err(anyhow!("Empty embedding returned from Ollama"));
        }

        Ok(floats)
    }

    /// Ensure an embedding model is pulled in Ollama.
    pub fn ensure_embed_model(&self, model: &str) -> Result<()> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .context("Failed to list Ollama models")?;

        let body: Value = resp.json().context("Failed to parse /api/tags response")?;
        let model_exists = body["models"].as_array().is_some_and(|models| {
            models.iter().any(|m| {
                let name = m["name"].as_str().unwrap_or("");
                name == model
                    || name.starts_with(&format!("{model}:"))
                    || model == name.split(':').next().unwrap_or("")
            })
        });

        if model_exists {
            return Ok(());
        }

        tracing::info!("Pulling Ollama embedding model '{}'...", model);
        let pull_url = format!("{}/api/pull", self.base_url);
        let pull_client = reqwest::blocking::Client::builder()
            .timeout(PULL_TIMEOUT)
            .build()
            .context("Failed to build pull client")?;
        let resp = pull_client
            .post(&pull_url)
            .json(&json!({ "name": model }))
            .send()
            .context("Failed to pull embedding model from Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow!("Ollama embed model pull failed ({status}): {text}"));
        }

        tracing::info!("Embedding model '{}' pulled successfully", model);
        Ok(())
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

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// Mock Ollama client for testing without a running Ollama daemon.
    /// Returns deterministic, canned responses for each public method.
    pub struct MockOllamaClient {
        pub base_url: String,
        pub model: String,
    }

    impl MockOllamaClient {
        /// Create a mock client with the given URL and model name.
        pub fn new_with_url(base_url: &str, model: &str) -> Result<Self> {
            Ok(Self {
                base_url: base_url.trim_end_matches('/').to_string(),
                model: model.to_string(),
            })
        }

        /// Mock health check — always returns true.
        pub fn is_available(&self) -> bool {
            true
        }

        /// Mock ensure_model — always succeeds.
        pub fn ensure_model(&self) -> Result<()> {
            Ok(())
        }

        /// Mock ensure_embed_model — always succeeds.
        pub fn ensure_embed_model(&self, _model: &str) -> Result<()> {
            Ok(())
        }

        /// Mock generate — returns deterministic responses based on prompt content.
        pub fn generate(&self, prompt: &str, _system: Option<&str>) -> Result<String> {
            if prompt.contains("expand") || prompt.contains("search") {
                Ok("semantic search\nquery terms\nvector retrieval\n\
                    information retrieval\nsimilarity matching"
                    .to_string())
            } else if prompt.contains("Summarize") {
                Ok("This is a consolidated summary of multiple memories \
                    covering key facts and decisions."
                    .to_string())
            } else if prompt.contains("tags") {
                Ok("important\nkey-fact\nstatus-update\ntechnical".to_string())
            } else if prompt.contains("contradict") {
                if prompt.contains("yes") || prompt.contains("true") {
                    Ok("yes".to_string())
                } else {
                    Ok("no".to_string())
                }
            } else {
                Ok("Mock response for: ".to_string() + &prompt[..prompt.len().min(50)])
            }
        }

        /// Mock expand_query — returns synthetic query expansion terms.
        pub fn expand_query(&self, query: &str) -> Result<Vec<String>> {
            let terms: Vec<String> = vec![
                format!("{}-related", query),
                format!("{}-expanded", query),
                "semantic-search".to_string(),
                "vector-expansion".to_string(),
                "query-variants".to_string(),
            ];
            Ok(terms.iter().map(|s| s.to_string()).collect())
        }

        /// Mock summarize_memories — returns a canned summary.
        pub fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
            let count = memories.len();
            Ok(format!(
                "Summary of {} memories: consolidated facts and key decisions preserved",
                count
            ))
        }

        /// Mock auto_tag — returns predictable tags.
        pub fn auto_tag(&self, title: &str, _content: &str) -> Result<Vec<String>> {
            let tags: Vec<String> = vec![
                "important".to_string(),
                format!("{}-tag", title.split_whitespace().next().unwrap_or("data")),
                "memory".to_string(),
            ];
            Ok(tags)
        }

        /// Mock embed_text — returns a fixed 768-dim vector (nomic standard).
        pub fn embed_text(&self, text: &str, _embed_model: &str) -> Result<Vec<f32>> {
            let base_val = (text.len() % 10) as f32 / 100.0;
            let embedding: Vec<f32> = (0..768).map(|i| base_val + (i as f32) * 0.0001).collect();
            Ok(embedding)
        }

        /// Mock detect_contradiction — simple heuristic based on keyword presence.
        pub fn detect_contradiction(&self, mem_a: &str, mem_b: &str) -> Result<bool> {
            let combined = format!("{} {}", mem_a, mem_b).to_lowercase();
            let contradictory_keywords = &["not", "never", "always", "contradiction", "opposite"];
            let count = contradictory_keywords
                .iter()
                .filter(|&&kw| combined.contains(kw))
                .count();
            Ok(count > 1)
        }
    }
}

#[cfg(test)]
mod mock_tests {
    use super::test_support::MockOllamaClient;
    use super::{AUTO_TAG_PROMPT, CONTRADICTION_PROMPT, QUERY_EXPANSION_PROMPT, SUMMARIZE_PROMPT};

    #[test]
    fn test_mock_new_with_url() {
        let client = MockOllamaClient::new_with_url("http://localhost:11434", "test-model");
        assert!(client.is_ok());
        let client = client.unwrap();
        assert_eq!(client.base_url, "http://localhost:11434");
        assert_eq!(client.model, "test-model");
    }

    #[test]
    fn test_mock_new_with_url_trailing_slash() {
        let client = MockOllamaClient::new_with_url("http://localhost:11434/", "test-model");
        assert!(client.is_ok());
        let client = client.unwrap();
        assert_eq!(client.base_url, "http://localhost:11434");
    }

    #[test]
    fn test_mock_is_available() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        assert!(client.is_available());
    }

    #[test]
    fn test_mock_ensure_model() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        assert!(client.ensure_model().is_ok());
    }

    #[test]
    fn test_mock_ensure_embed_model() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        assert!(client.ensure_embed_model("nomic-embed-text").is_ok());
    }

    #[test]
    fn test_mock_generate_query_expansion() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let prompt = QUERY_EXPANSION_PROMPT.replace("{query}", "search test");
        let result = client.generate(&prompt, None);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(!response.is_empty());
    }

    #[test]
    fn test_mock_expand_query() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.expand_query("test query");
        assert!(result.is_ok());
        let terms = result.unwrap();
        assert!(!terms.is_empty());
        assert!(terms.len() >= 3);
    }

    #[test]
    fn test_mock_summarize_memories() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let memories = vec![
            ("Title 1".to_string(), "Content 1".to_string()),
            ("Title 2".to_string(), "Content 2".to_string()),
        ];
        let result = client.summarize_memories(&memories);
        assert!(result.is_ok());
        let summary = result.unwrap();
        assert!(summary.contains("2"));
    }

    #[test]
    fn test_mock_auto_tag() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.auto_tag("Test Title", "test content");
        assert!(result.is_ok());
        let tags = result.unwrap();
        assert!(!tags.is_empty());
        assert!(tags.len() >= 2);
    }

    #[test]
    fn test_mock_embed_text() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.embed_text("test text", "nomic-embed-text");
        assert!(result.is_ok());
        let embedding = result.unwrap();
        assert_eq!(embedding.len(), 768);
        assert!(embedding.iter().all(|&x| x >= 0.0));
    }

    #[test]
    fn test_mock_embed_text_deterministic() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result1 = client.embed_text("same text", "nomic-embed-text");
        let result2 = client.embed_text("same text", "nomic-embed-text");
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap(), result2.unwrap());
    }

    #[test]
    fn test_mock_detect_contradiction_true() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.detect_contradiction(
            "The system always works",
            "The system never works correctly",
        );
        assert!(result.is_ok());
        let is_contradiction = result.unwrap();
        assert!(is_contradiction);
    }

    #[test]
    fn test_mock_detect_contradiction_false() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.detect_contradiction(
            "The memory is about search",
            "Additional details about the same search",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_mock_generate_summarize_prompt() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let prompt = SUMMARIZE_PROMPT.replace(
            "{memories}",
            "--- Memory 1 ---\nTitle: Test\nThis is a test",
        );
        let result = client.generate(&prompt, None);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.contains("summary") || response.contains("Summary"));
    }

    #[test]
    fn test_mock_generate_auto_tag_prompt() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let prompt = AUTO_TAG_PROMPT
            .replace("{title}", "Important Update")
            .replace("{content}", "Some content");
        let result = client.generate(&prompt, None);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(!response.is_empty());
    }

    #[test]
    fn test_mock_generate_contradiction_prompt() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let prompt = CONTRADICTION_PROMPT
            .replace("{a}", "Statement A")
            .replace("{b}", "Statement B");
        let result = client.generate(&prompt, None);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(!response.is_empty());
    }
}
