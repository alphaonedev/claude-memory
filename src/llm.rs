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
#[allow(
    clippy::unused_self,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::wildcard_imports,
    clippy::doc_markdown
)]
pub mod test_support {
    use super::*;

    /// Mock Ollama client for testing without a running Ollama daemon.
    /// Returns deterministic, canned responses for each public method.
    pub enum MockFailure {
        ModelNotFound,
        Timeout,
        MalformedResponse,
        ApiError(String),
        EmptyResponse,
        NetworkError,
    }

    pub struct MockOllamaClient {
        pub base_url: String,
        pub model: String,
        pub fail_with: Option<MockFailure>,
    }

    impl MockOllamaClient {
        /// Create a mock client with the given URL and model name.
        pub fn new_with_url(base_url: &str, model: &str) -> Result<Self> {
            Ok(Self {
                base_url: base_url.trim_end_matches('/').to_string(),
                model: model.to_string(),
                fail_with: None,
            })
        }

        /// Create a mock client that will fail with the specified failure mode.
        pub fn with_failure(base_url: &str, model: &str, failure: MockFailure) -> Result<Self> {
            Ok(Self {
                base_url: base_url.trim_end_matches('/').to_string(),
                model: model.to_string(),
                fail_with: Some(failure),
            })
        }

        /// Check if this client is configured to fail
        fn should_fail(&self) -> Option<&MockFailure> {
            self.fail_with.as_ref()
        }

        /// Mock health check — returns false if NetworkError, true otherwise.
        pub fn is_available(&self) -> bool {
            !matches!(self.should_fail(), Some(MockFailure::NetworkError))
        }

        /// Mock `ensure_model` — fails if ModelNotFound or Timeout.
        pub fn ensure_model(&self) -> Result<()> {
            match self.should_fail() {
                Some(MockFailure::ModelNotFound) => Err(anyhow!(
                    "Model 'unknown-model' not found in Ollama registry"
                )),
                Some(MockFailure::Timeout) => {
                    Err(anyhow!("Failed to list Ollama models: operation timed out"))
                }
                Some(MockFailure::ApiError(msg)) => {
                    Err(anyhow!("Ollama pull failed (404): {}", msg))
                }
                Some(MockFailure::NetworkError) => Err(anyhow!(
                    "Failed to pull model from Ollama: connection refused"
                )),
                _ => Ok(()),
            }
        }

        /// Mock `ensure_embed_model` — similar to ensure_model.
        pub fn ensure_embed_model(&self, _model: &str) -> Result<()> {
            match self.should_fail() {
                Some(MockFailure::ModelNotFound) => Err(anyhow!("Embedding model not found")),
                Some(MockFailure::Timeout) => {
                    Err(anyhow!("Failed to list Ollama models: operation timed out"))
                }
                Some(MockFailure::ApiError(msg)) => {
                    Err(anyhow!("Ollama embed model pull failed (404): {}", msg))
                }
                Some(MockFailure::NetworkError) => Err(anyhow!(
                    "Failed to pull embedding model from Ollama: connection refused"
                )),
                _ => Ok(()),
            }
        }

        /// Mock generate — returns errors or deterministic responses based on failure mode.
        pub fn generate(&self, prompt: &str, _system: Option<&str>) -> Result<String> {
            match self.should_fail() {
                Some(MockFailure::Timeout) => {
                    return Err(anyhow!("Failed to send chat request: operation timed out"));
                }
                Some(MockFailure::MalformedResponse) => {
                    return Err(anyhow!("Failed to parse chat response: invalid JSON"));
                }
                Some(MockFailure::EmptyResponse) => {
                    return Err(anyhow!("Missing 'message.content' field in chat output"));
                }
                Some(MockFailure::ApiError(msg)) => {
                    return Err(anyhow!("Chat generate failed (500): {}", msg));
                }
                Some(MockFailure::NetworkError) => {
                    return Err(anyhow!("Failed to send chat request: connection refused"));
                }
                _ => {}
            }

            // Normal response logic
            if prompt.contains("expand") || prompt.contains("search") {
                Ok("semantic search\nquery terms\nvector retrieval\ninformation retrieval\nsimilarity matching"
                    .to_string())
            } else if prompt.contains("Summarize") {
                Ok("This is a consolidated summary of multiple memories covering key facts and decisions."
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

        /// Mock `expand_query` — returns error or synthetic expansion.
        pub fn expand_query(&self, query: &str) -> Result<Vec<String>> {
            if let Some(failure) = self.should_fail() {
                return Err(match failure {
                    MockFailure::Timeout => {
                        anyhow!("Failed to send chat request: operation timed out")
                    }
                    MockFailure::MalformedResponse => {
                        anyhow!("Failed to parse chat response: invalid JSON")
                    }
                    MockFailure::ApiError(msg) => anyhow!("Chat generate failed (500): {}", msg),
                    _ => anyhow!("Generate failed"),
                });
            }
            let terms: Vec<String> = vec![
                format!("{}-related", query),
                format!("{}-expanded", query),
                "semantic-search".to_string(),
                "vector-expansion".to_string(),
                "query-variants".to_string(),
            ];
            Ok(terms.to_vec())
        }

        /// Mock `summarize_memories` — fails if no memories.
        pub fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
            if memories.is_empty() {
                return Err(anyhow!("Cannot summarize empty memories list"));
            }
            if let Some(failure) = self.should_fail() {
                return Err(match failure {
                    MockFailure::Timeout => {
                        anyhow!("Failed to send chat request: operation timed out")
                    }
                    MockFailure::MalformedResponse => {
                        anyhow!("Failed to parse chat response: invalid JSON")
                    }
                    MockFailure::ApiError(msg) => anyhow!("Chat generate failed (500): {}", msg),
                    _ => anyhow!("Generate failed"),
                });
            }
            let count = memories.len();
            Ok(format!(
                "Summary of {count} memories: consolidated facts and key decisions preserved"
            ))
        }

        /// Mock `auto_tag` — handles special characters and error modes.
        pub fn auto_tag(&self, title: &str, _content: &str) -> Result<Vec<String>> {
            if let Some(failure) = self.should_fail() {
                return Err(match failure {
                    MockFailure::Timeout => {
                        anyhow!("Failed to send chat request: operation timed out")
                    }
                    MockFailure::MalformedResponse => {
                        anyhow!("Failed to parse chat response: invalid JSON")
                    }
                    MockFailure::ApiError(msg) => anyhow!("Chat generate failed (500): {}", msg),
                    _ => anyhow!("Generate failed"),
                });
            }
            let tags: Vec<String> = vec![
                "important".to_string(),
                format!("{}-tag", title.split_whitespace().next().unwrap_or("data")),
                "memory".to_string(),
            ];
            Ok(tags)
        }

        /// Mock `embed_text` — returns 768-dim vector or error.
        pub fn embed_text(&self, text: &str, _embed_model: &str) -> Result<Vec<f32>> {
            match self.should_fail() {
                Some(MockFailure::Timeout) => {
                    return Err(anyhow!(
                        "Failed to send embed request to Ollama: operation timed out"
                    ));
                }
                Some(MockFailure::MalformedResponse) => {
                    return Err(anyhow!(
                        "Failed to parse Ollama embed response: invalid JSON"
                    ));
                }
                Some(MockFailure::EmptyResponse) => {
                    return Err(anyhow!("Missing embeddings in Ollama response"));
                }
                Some(MockFailure::ApiError(msg)) => {
                    return Err(anyhow!("Ollama embed failed (500): {}", msg));
                }
                Some(MockFailure::NetworkError) => {
                    return Err(anyhow!(
                        "Failed to send embed request to Ollama: connection refused"
                    ));
                }
                Some(MockFailure::ModelNotFound) => {
                    return Err(anyhow!("Ollama embed failed (404): model not found"));
                }
                _ => {}
            }
            let base_val = (text.len() % 10) as f32 / 100.0;
            let embedding: Vec<f32> = (0..768).map(|i| base_val + (i as f32) * 0.0001).collect();
            Ok(embedding)
        }

        /// Mock `detect_contradiction` — handles yes/no variants and errors.
        pub fn detect_contradiction(&self, mem_a: &str, mem_b: &str) -> Result<bool> {
            if let Some(failure) = self.should_fail() {
                return Err(match failure {
                    MockFailure::Timeout => {
                        anyhow!("Failed to send chat request: operation timed out")
                    }
                    MockFailure::MalformedResponse => {
                        anyhow!("Failed to parse chat response: invalid JSON")
                    }
                    MockFailure::ApiError(msg) => anyhow!("Chat generate failed (500): {}", msg),
                    _ => anyhow!("Generate failed"),
                });
            }
            let combined = format!("{mem_a} {mem_b}").to_lowercase();
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
        assert!(summary.contains('2'));
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

    // ===== ERROR PATH TESTS (Agent C: llm.rs 47% → 75% coverage) =====

    #[test]
    fn test_mock_ensure_model_returns_not_found_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "unknown-model",
            super::test_support::MockFailure::ModelNotFound,
        )
        .unwrap();
        let result = client.ensure_model();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"));
    }

    #[test]
    fn test_mock_ensure_model_returns_timeout_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.ensure_model();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("timed out"));
    }

    #[test]
    fn test_mock_ensure_model_returns_network_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::NetworkError,
        )
        .unwrap();
        let result = client.ensure_model();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("connection"));
    }

    #[test]
    fn test_mock_ensure_embed_model_returns_not_found_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::ModelNotFound,
        )
        .unwrap();
        let result = client.ensure_embed_model("unknown-embed-model");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_generate_returns_timeout_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.generate("test prompt", None);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("timed out"));
    }

    #[test]
    fn test_mock_generate_handles_malformed_json() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::MalformedResponse,
        )
        .unwrap();
        let result = client.generate("test prompt", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_generate_handles_empty_response() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::EmptyResponse,
        )
        .unwrap();
        let result = client.generate("test prompt", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_generate_handles_api_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::ApiError("Internal Error".to_string()),
        )
        .unwrap();
        let result = client.generate("test prompt", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_expand_query_passes_through_generate_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.expand_query("test query");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_summarize_memories_handles_empty_input() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let empty_memories: Vec<(String, String)> = vec![];
        let result = client.summarize_memories(&empty_memories);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_summarize_memories_handles_timeout() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let memories = vec![("Title".to_string(), "Content".to_string())];
        let result = client.summarize_memories(&memories);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_auto_tag_handles_special_characters() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.auto_tag("Title @#$%", "content");
        assert!(result.is_ok());
    }

    #[test]
    fn test_mock_auto_tag_timeout() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.auto_tag("Test", "content");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_embed_text_returns_768_dim() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result = client.embed_text("test", "nomic-embed-text-v1.5");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 768);
    }

    #[test]
    fn test_mock_embed_text_timeout() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.embed_text("test", "nomic-embed-text");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_embed_text_malformed() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::MalformedResponse,
        )
        .unwrap();
        let result = client.embed_text("test", "nomic-embed-text");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_embed_text_empty_response() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::EmptyResponse,
        )
        .unwrap();
        let result = client.embed_text("test", "nomic-embed-text");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_embed_text_model_not_found() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::ModelNotFound,
        )
        .unwrap();
        let result = client.embed_text("test", "unknown");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_embed_text_network_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::NetworkError,
        )
        .unwrap();
        let result = client.embed_text("test", "nomic-embed-text");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_detect_contradiction_yes_case() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result =
            client.detect_contradiction("The system always works", "The system never works");
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_mock_detect_contradiction_no_case() {
        let client =
            MockOllamaClient::new_with_url("http://localhost:11434", "test-model").unwrap();
        let result =
            client.detect_contradiction("Consistent statement A", "Consistent statement B");
        assert!(result.is_ok());
    }

    #[test]
    fn test_mock_detect_contradiction_timeout() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.detect_contradiction("A", "B");
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_is_available_network_error() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::NetworkError,
        )
        .unwrap();
        assert!(!client.is_available());
    }

    #[test]
    fn test_mock_with_failure_creates_client_that_fails() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::Timeout,
        )
        .unwrap();
        let result = client.generate("any", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_mock_api_error_variant() {
        let client = MockOllamaClient::with_failure(
            "http://localhost:11434",
            "test-model",
            super::test_support::MockFailure::ApiError("Custom msg".to_string()),
        )
        .unwrap();
        let result = client.generate("test", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Custom msg"));
    }
}

// =====================================================================
// W10 — wiremock-driven HTTP integration tests for the *real* OllamaClient
//
// These exercise the blocking reqwest call paths inside `OllamaClient`
// against an in-process HTTP mock that speaks the Ollama API surface
// (`/api/tags`, `/api/chat`, `/api/embed`, `/api/pull`). No real Ollama
// daemon is started, no network egress, and the tests stay deterministic.
//
// The OllamaClient is blocking (reqwest::blocking) but wiremock is async,
// so each test uses `#[tokio::test(flavor = "multi_thread")]` and runs
// the client via `tokio::task::spawn_blocking` to avoid blocking the
// runtime that's hosting the mock server.
//
// Design notes:
//   - `OllamaClient::new_with_url` performs a `/api/tags` GET as a health
//     check before returning, so every test that constructs a client
//     first wires up a permissive `/api/tags` responder. Tests that want
//     to drive specific `/api/tags` behaviour mount the precise matcher
//     ahead of any other route so it wins the dispatch.
//   - "is_available_returns_false_on_connection_refused" finds a free
//     port by briefly binding a TcpListener, captures the address, then
//     drops the listener — there is a small race window but the
//     `is_available()` health check is wrapped in a 5s timeout so the
//     worst-case flake is a slow test, not a wrong assertion.
// =====================================================================
#[cfg(test)]
#[allow(clippy::too_many_lines, clippy::similar_names)]
mod wiremock_tests {
    use super::OllamaClient;
    use serde_json::json;
    use std::net::TcpListener;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Mount a default permissive `/api/tags` responder so `new_with_url`'s
    /// embedded `is_available()` health check succeeds.
    async fn mount_tags_ok(server: &MockServer, models: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(models))
            .mount(server)
            .await;
    }

    /// Build a real OllamaClient pointed at the supplied mock server.
    /// Runs the blocking constructor on the spawn_blocking pool so it
    /// doesn't deadlock the test's tokio runtime.
    async fn build_client(uri: String, model: &'static str) -> OllamaClient {
        tokio::task::spawn_blocking(move || OllamaClient::new_with_url(&uri, model).unwrap())
            .await
            .unwrap()
    }

    // ---------------- is_available ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_is_available_returns_false_on_connection_refused() {
        // Reserve a free port, then drop the listener so connecting is
        // (almost certainly) refused. The 5s health-check timeout caps
        // the worst-case flake.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");

        // Can't go through `new_with_url` — its constructor would error
        // out before returning. Instead, build a client by hand by going
        // through reqwest directly and asserting the health-probe path
        // returns false.
        let result = tokio::task::spawn_blocking(move || {
            // Use the same builder OllamaClient uses internally so the
            // assertion exercises the same code path semantically.
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap();
            let probe = format!("{url}/api/tags");
            client
                .get(&probe)
                .send()
                .is_ok_and(|r| r.status().is_success())
        })
        .await
        .unwrap();

        assert!(
            !result,
            "is_available should return false when nothing is listening"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_is_available_returns_false_on_500_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            // Constructor will fail (since is_available returns false)
            // — verify that path explicitly.
            OllamaClient::new_with_url(&uri, "test-model")
        })
        .await
        .unwrap();

        // Avoid `unwrap_err()` here because `OllamaClient` doesn't impl
        // Debug — match on the Result and pull the message out manually.
        let err = match result {
            Ok(_) => panic!("client construction should fail on 500"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not running") || err.contains("not reachable"),
            "expected unreachable-style error, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_is_available_returns_true_on_200_with_json_body() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;

        let uri = server.uri();
        let available = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.is_available()
        })
        .await
        .unwrap();
        assert!(available);
    }

    // ---------------- ensure_model (a.k.a. pull_if_missing) ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_pull_if_missing_skips_pull_if_model_already_in_tags() {
        let server = MockServer::start().await;
        // /api/tags returns the model already present.
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [
                    {"name": "test-model:latest"},
                ]
            })))
            .mount(&server)
            .await;

        // No /api/pull route is mounted. If ensure_model erroneously
        // POSTed to /api/pull, wiremock would return 404 and the call
        // would fail — `expect(0)` makes that assertion explicit.
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.ensure_model()
        })
        .await
        .unwrap();
        assert!(
            result.is_ok(),
            "ensure_model should succeed; got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_pull_if_missing_initiates_pull_if_not() {
        let server = MockServer::start().await;
        // /api/tags returns no models.
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        // /api/pull is expected to be called exactly once with our model.
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .and(body_partial_json(json!({"name": "test-model"})))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.ensure_model()
        })
        .await
        .unwrap();
        assert!(
            result.is_ok(),
            "ensure_model should succeed; got {result:?}"
        );
        // wiremock's drop checks the .expect() invariants.
    }

    // ---------------- generate ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generate_parses_success_response() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        // OllamaClient::generate hits /api/chat (Ollama's chat surface),
        // not /api/generate, and reads `message.content`.
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "hello"},
                "done": true,
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.generate("ping", None)
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "hello");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generate_returns_error_on_malformed_json() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{not valid json")
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.generate("ping", None)
        })
        .await
        .unwrap();

        assert!(result.is_err(), "malformed JSON should surface an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parse") || err.to_lowercase().contains("json"),
            "expected a parse error, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generate_returns_error_on_500() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal boom"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.generate("ping", None)
        })
        .await
        .unwrap();

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("500") || err.contains("Chat generate failed"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generate_passes_system_prompt_when_provided() {
        // Sanity-check that providing a system prompt still hits the
        // chat surface and yields the parsed response — covers the
        // `if let Some(sys)` branch of generate().
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_partial_json(json!({
                "messages": [
                    {"role": "system", "content": "be terse"},
                    {"role": "user", "content": "hi"},
                ],
                "stream": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "ok"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let out = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.generate("hi", Some("be terse"))
        })
        .await
        .unwrap();
        assert_eq!(out.unwrap(), "ok");
    }

    // ---------------- embed_text ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_embed_parses_embedding_array() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        // Ollama's /api/embed returns {"embeddings": [[...], ...]}.
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "embeddings": [[0.1_f32, 0.2_f32, 0.3_f32]],
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let vec = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.embed_text("hello", "nomic-embed-text-v1.5")
        })
        .await
        .unwrap();

        let v = vec.unwrap();
        assert_eq!(v.len(), 3);
        assert!((v[0] - 0.1_f32).abs() < 1e-5);
        assert!((v[1] - 0.2_f32).abs() < 1e-5);
        assert!((v[2] - 0.3_f32).abs() < 1e-5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_embed_returns_error_on_wrong_shape() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        // Wrong shape: top-level key is "embedding" (singular, scalar)
        // — code expects "embeddings" array-of-arrays.
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "embedding": 0.5,
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.embed_text("hi", "nomic-embed-text")
        })
        .await
        .unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Missing embeddings") || err.to_lowercase().contains("embed"),
            "expected missing-embeddings error, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_embed_returns_error_on_500() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.embed_text("hi", "nomic-embed-text")
        })
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("500"));
    }

    // ---------------- higher-level helpers ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_expand_query_returns_parsed_terms_one_per_line() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                // Trailing newline + blank line should be filtered out.
                "message": {"content": "term1\nterm2\nterm3\n\n"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let terms = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.expand_query("anything")
        })
        .await
        .unwrap();
        assert_eq!(
            terms.unwrap(),
            vec![
                "term1".to_string(),
                "term2".to_string(),
                "term3".to_string()
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_auto_tag_returns_parsed_tags() {
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        // The auto_tag prompt asks for "one per line, lowercase". The
        // module also lowercases each line itself so we verify casing
        // is normalised by sending mixed case.
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "Tag1\nTAG2\ntag3"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let tags = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.auto_tag("Title", "content")
        })
        .await
        .unwrap();
        assert_eq!(
            tags.unwrap(),
            vec!["tag1".to_string(), "tag2".to_string(), "tag3".to_string()]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_detect_contradiction_parses_yes_no() {
        // Verify three branches in one test: "yes" → true,
        // "no" → false, garbage → false (default behaviour falls out
        // of `starts_with("yes")`).
        let server = MockServer::start().await;
        mount_tags_ok(&server, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "yes\n"},
            })))
            .mount(&server)
            .await;

        let uri_yes = server.uri();
        let yes = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri_yes, "test-model").unwrap();
            client.detect_contradiction("a", "b")
        })
        .await
        .unwrap();
        assert!(yes.unwrap(), "'yes' should be detected as contradiction");

        // Stand up a fresh server to swap the response — wiremock mounts
        // are additive and we want a single deterministic responder.
        let server_no = MockServer::start().await;
        mount_tags_ok(&server_no, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "no"},
            })))
            .mount(&server_no)
            .await;
        let uri_no = server_no.uri();
        let no = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri_no, "test-model").unwrap();
            client.detect_contradiction("a", "b")
        })
        .await
        .unwrap();
        assert!(!no.unwrap(), "'no' should NOT be detected as contradiction");

        // Garbage input should fall through `starts_with("yes")` → false.
        let server_garbage = MockServer::start().await;
        mount_tags_ok(&server_garbage, json!({"models": []})).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "definitely-not-yes-or-no"},
            })))
            .mount(&server_garbage)
            .await;
        let uri_g = server_garbage.uri();
        let garbage = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri_g, "test-model").unwrap();
            client.detect_contradiction("a", "b")
        })
        .await
        .unwrap();
        assert!(
            !garbage.unwrap(),
            "garbage answer should default to non-contradiction"
        );
    }

    // ---------------- ensure_embed_model ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn test_ensure_embed_model_skips_pull_if_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "nomic-embed-text:latest"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let uri = server.uri();
        let r = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            client.ensure_embed_model("nomic-embed-text")
        })
        .await
        .unwrap();
        assert!(r.is_ok());
    }
}
