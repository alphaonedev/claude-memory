// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Retroactive conversation import from Claude, `ChatGPT`, and Slack exports.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

use crate::models::MAX_CONTENT_SIZE;

// ---------------------------------------------------------------------------
// Common types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    pub messages: Vec<Message>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
}

/// Result of mining a single conversation.
#[derive(Debug)]
pub struct MinedMemory {
    pub title: String,
    pub content: String,
    pub source_format: String,
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Claude,
    ChatGpt,
    Slack,
}

impl Format {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "chatgpt" => Some(Self::ChatGpt),
            "slack" => Some(Self::Slack),
            _ => None,
        }
    }

    pub fn source_tag(self) -> &'static str {
        match self {
            Self::Claude => "mine-claude",
            Self::ChatGpt => "mine-chatgpt",
            Self::Slack => "mine-slack",
        }
    }
}

// ---------------------------------------------------------------------------
// Parse: Claude (JSONL)
// ---------------------------------------------------------------------------

/// Parse Claude's conversations.jsonl export.
/// Each line is a JSON object representing one conversation.
pub fn parse_claude(path: &Path) -> Result<Vec<Conversation>> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read Claude export: {}", path.display()))?;

    let mut conversations = Vec::new();

    for (line_num, line) in data.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("invalid JSON on line {}", line_num + 1))?;

        let conv = parse_claude_conversation(&val, line_num)?;
        if let Some(c) = conv {
            conversations.push(c);
        }
    }

    Ok(conversations)
}

#[allow(clippy::unnecessary_wraps)]
fn parse_claude_conversation(
    val: &serde_json::Value,
    line_num: usize,
) -> Result<Option<Conversation>> {
    let id = val["uuid"]
        .as_str()
        .unwrap_or(&format!("claude-{line_num}"))
        .to_string();
    let title = val["name"].as_str().map(std::string::ToString::to_string);
    let created_at = val["created_at"]
        .as_str()
        .map(std::string::ToString::to_string);

    let mut messages = Vec::new();

    // Format 1: "chat_messages" array (Claude export format)
    if let Some(msgs) = val["chat_messages"].as_array() {
        for msg in msgs {
            let role = msg["sender"]
                .as_str()
                .or_else(|| msg["role"].as_str())
                .unwrap_or("unknown")
                .to_string();
            // Map "human" -> "user"
            let role = match role.as_str() {
                "human" => "user".to_string(),
                other => other.to_string(),
            };

            let content = extract_text_content(&msg["text"])
                .or_else(|| extract_text_content(&msg["content"]))
                .unwrap_or_default();

            if !content.is_empty() {
                let timestamp = msg["created_at"]
                    .as_str()
                    .or_else(|| msg["timestamp"].as_str())
                    .map(std::string::ToString::to_string);
                messages.push(Message {
                    role,
                    content,
                    timestamp,
                });
            }
        }
    }
    // Format 2: "mapping" object (tree of message nodes)
    else if let Some(mapping) = val["mapping"].as_object() {
        let mut node_messages: Vec<(String, Message)> = Vec::new();
        for (_node_id, node) in mapping {
            if let Some(msg) = node["message"].as_object() {
                let role = msg
                    .get("role")
                    .and_then(|r| r.as_str())
                    .or_else(|| {
                        msg.get("author")
                            .and_then(|a| a.get("role"))
                            .and_then(|r| r.as_str())
                    })
                    .unwrap_or("unknown");

                if role == "system" {
                    continue;
                }

                let content = extract_message_content(msg);
                if !content.is_empty() {
                    let ts = msg
                        .get("create_time")
                        .and_then(serde_json::Value::as_i64)
                        .map(|t| {
                            chrono::DateTime::from_timestamp(t, 0)
                                .map(|dt| dt.to_rfc3339())
                                .unwrap_or_default()
                        });
                    let sort_key = msg
                        .get("create_time")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0)
                        .to_string();
                    node_messages.push((
                        sort_key,
                        Message {
                            role: role.to_string(),
                            content,
                            timestamp: ts,
                        },
                    ));
                }
            }
        }
        node_messages.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        messages = node_messages.into_iter().map(|(_, m)| m).collect();
    }

    if messages.is_empty() {
        return Ok(None);
    }

    Ok(Some(Conversation {
        id,
        title,
        messages,
        created_at,
    }))
}

// ---------------------------------------------------------------------------
// Parse: ChatGPT (JSON)
// ---------------------------------------------------------------------------

/// Parse `ChatGPT`'s conversations.json export.
pub fn parse_chatgpt(path: &Path) -> Result<Vec<Conversation>> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read ChatGPT export: {}", path.display()))?;

    let val: serde_json::Value =
        serde_json::from_str(&data).context("invalid JSON in ChatGPT export")?;

    let arr = val
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected JSON array at top level"))?;

    let mut conversations = Vec::new();

    for (idx, conv_val) in arr.iter().enumerate() {
        let id = conv_val["id"]
            .as_str()
            .unwrap_or(&format!("chatgpt-{idx}"))
            .to_string();
        let title = conv_val["title"]
            .as_str()
            .map(std::string::ToString::to_string);
        let created_at = conv_val["create_time"]
            .as_i64()
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|dt| dt.to_rfc3339());

        let mut messages = Vec::new();

        // ChatGPT uses a "mapping" tree of nodes
        if let Some(mapping) = conv_val["mapping"].as_object() {
            let mut node_msgs: Vec<(f64, Message)> = Vec::new();

            for (_node_id, node) in mapping {
                if let Some(msg) = node.get("message") {
                    let role = msg["author"]["role"].as_str().unwrap_or("unknown");
                    if role == "system" {
                        continue;
                    }

                    let content =
                        extract_message_content(msg.as_object().unwrap_or(&serde_json::Map::new()));
                    if content.is_empty() {
                        continue;
                    }

                    let ts = msg["create_time"].as_f64().unwrap_or(0.0);
                    #[allow(clippy::cast_possible_truncation)]
                    node_msgs.push((
                        ts,
                        Message {
                            role: role.to_string(),
                            content,
                            timestamp: chrono::DateTime::from_timestamp(ts as i64, 0)
                                .map(|dt| dt.to_rfc3339()),
                        },
                    ));
                }
            }

            node_msgs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            messages = node_msgs.into_iter().map(|(_, m)| m).collect();
        }

        if messages.is_empty() {
            continue;
        }

        conversations.push(Conversation {
            id,
            title,
            messages,
            created_at,
        });
    }

    Ok(conversations)
}

// ---------------------------------------------------------------------------
// Parse: Slack (directory of JSON files)
// ---------------------------------------------------------------------------

/// Parse a Slack workspace export directory.
/// Structure: channel_name/YYYY-MM-DD.json
pub fn parse_slack(path: &Path) -> Result<Vec<Conversation>> {
    if !path.is_dir() {
        bail!("Slack export path must be a directory: {}", path.display());
    }

    let mut conversations = Vec::new();

    // Each subdirectory is a channel
    let mut entries: Vec<_> = fs::read_dir(path)
        .with_context(|| format!("failed to read Slack export dir: {}", path.display()))?
        .filter_map(std::result::Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let channel_path = entry.path();
        if !channel_path.is_dir() {
            continue;
        }
        let channel_name = entry.file_name().to_string_lossy().to_string();

        // Collect all JSON files in the channel, sorted by date
        let mut json_files: Vec<_> = fs::read_dir(&channel_path)?
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        json_files.sort_by_key(std::fs::DirEntry::file_name);

        let mut all_messages = Vec::new();

        for file_entry in json_files {
            let file_path = file_entry.path();
            let data = fs::read_to_string(&file_path)?;
            let msgs: serde_json::Value = serde_json::from_str(&data)
                .with_context(|| format!("invalid JSON: {}", file_path.display()))?;

            if let Some(arr) = msgs.as_array() {
                for msg in arr {
                    let user = msg["user"]
                        .as_str()
                        .or_else(|| msg["username"].as_str())
                        .unwrap_or("unknown");
                    let text = msg["text"].as_str().unwrap_or("").to_string();
                    if text.is_empty() {
                        continue;
                    }

                    #[allow(clippy::cast_possible_truncation)]
                    let ts = msg["ts"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0))
                        .map(|dt| dt.to_rfc3339());

                    all_messages.push(Message {
                        role: user.to_string(),
                        content: text,
                        timestamp: ts.clone(),
                    });
                }
            }
        }

        if all_messages.is_empty() {
            continue;
        }

        let created_at = all_messages.first().and_then(|m| m.timestamp.clone());

        conversations.push(Conversation {
            id: format!("slack-{channel_name}"),
            title: Some(format!("#{channel_name}")),
            messages: all_messages,
            created_at,
        });
    }

    Ok(conversations)
}

// ---------------------------------------------------------------------------
// Content extraction
// ---------------------------------------------------------------------------

/// Extract text from a `serde_json::Value` that may be a string or array of parts.
fn extract_text_content(val: &serde_json::Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = val.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|p| {
                if let Some(s) = p.as_str() {
                    Some(s.to_string())
                } else {
                    p["text"].as_str().map(std::string::ToString::to_string)
                }
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    None
}

/// Extract message content from a message object (ChatGPT/Claude mapping format).
fn extract_message_content(msg: &serde_json::Map<String, serde_json::Value>) -> String {
    // Try content.parts array first (ChatGPT format)
    if let Some(content) = msg.get("content") {
        if let Some(parts) = content["parts"].as_array() {
            let text: Vec<String> = parts
                .iter()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            if !text.is_empty() {
                return text.join("\n");
            }
        }
        // Try content as string
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        // Try content.text
        if let Some(s) = content["text"].as_str() {
            return s.to_string();
        }
    }
    // Try text field directly
    if let Some(s) = msg.get("text").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Conversion to memories
// ---------------------------------------------------------------------------

/// Convert a parsed conversation into a `MinedMemory` ready for storage.
pub fn conversation_to_memory(conv: &Conversation, format: Format) -> Option<MinedMemory> {
    if conv.messages.is_empty() {
        return None;
    }

    // Title: use conversation title or first user message
    let title = conv.title.as_deref().filter(|t| !t.is_empty()).map_or_else(
        || {
            let first_user = conv
                .messages
                .iter()
                .find(|m| m.role == "user" || m.role == "human")
                .or(conv.messages.first());
            match first_user {
                Some(m) => truncate(&m.content, 100).to_string(),
                None => format!("Conversation {}", &conv.id),
            }
        },
        |t| truncate(t, 100).to_string(),
    );

    // Content: formatted message concatenation
    let mut content = String::new();
    for msg in &conv.messages {
        let line = format!("[{}]: {}\n", msg.role, msg.content);
        if content.len() + line.len() > MAX_CONTENT_SIZE {
            break;
        }
        content.push_str(&line);
    }

    if content.is_empty() {
        return None;
    }

    Some(MinedMemory {
        title,
        content,
        source_format: format.source_tag().to_string(),
        created_at: conv.created_at.clone(),
    })
}

fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_temp_file(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_parse_claude_jsonl() {
        let jsonl = r#"{"uuid":"conv1","name":"Test Chat","chat_messages":[{"sender":"human","text":"Hello"},{"sender":"assistant","text":"Hi there!"}]}"#;
        let f = make_temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("Test Chat".to_string()));
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello");
    }

    #[test]
    fn test_parse_claude_empty_lines() {
        let jsonl = "\n\n{\"uuid\":\"c1\",\"name\":\"X\",\"chat_messages\":[{\"sender\":\"human\",\"text\":\"hi\"}]}\n\n";
        let f = make_temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn test_parse_chatgpt_json() {
        let json = r#"[{"id":"conv1","title":"GPT Chat","create_time":1700000000,"mapping":{"node1":{"message":{"author":{"role":"user"},"content":{"parts":["What is Rust?"]},"create_time":1700000001}},"node2":{"message":{"author":{"role":"assistant"},"content":{"parts":["Rust is a systems programming language."]},"create_time":1700000002}}}}]"#;
        let f = make_temp_file(json);
        let convs = parse_chatgpt(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("GPT Chat".to_string()));
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].content, "What is Rust?");
    }

    #[test]
    fn test_parse_slack_dir() {
        let dir = tempfile::tempdir().unwrap();
        let channel_dir = dir.path().join("general");
        fs::create_dir(&channel_dir).unwrap();
        let msg_json = r#"[{"user":"U123","text":"Hello team!","ts":"1700000000.000000"},{"user":"U456","text":"Hey!","ts":"1700000001.000000"}]"#;
        fs::write(channel_dir.join("2024-01-01.json"), msg_json).unwrap();

        let convs = parse_slack(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("#general".to_string()));
        assert_eq!(convs[0].messages.len(), 2);
    }

    #[test]
    fn test_conversation_to_memory() {
        let conv = Conversation {
            id: "test1".to_string(),
            title: Some("My Chat".to_string()),
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                    timestamp: None,
                },
                Message {
                    role: "assistant".to_string(),
                    content: "Hi!".to_string(),
                    timestamp: None,
                },
            ],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::Claude).unwrap();
        assert_eq!(mem.title, "My Chat");
        assert!(mem.content.contains("[user]: Hello"));
        assert!(mem.content.contains("[assistant]: Hi!"));
        assert_eq!(mem.source_format, "mine-claude");
    }

    #[test]
    fn test_conversation_to_memory_no_title() {
        let conv = Conversation {
            id: "test2".to_string(),
            title: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: "What is the weather?".to_string(),
                timestamp: None,
            }],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::ChatGpt).unwrap();
        assert_eq!(mem.title, "What is the weather?");
    }

    #[test]
    fn test_conversation_to_memory_empty() {
        let conv = Conversation {
            id: "test3".to_string(),
            title: None,
            messages: vec![],
            created_at: None,
        };
        assert!(conversation_to_memory(&conv, Format::Claude).is_none());
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_format_from_str() {
        assert_eq!(Format::from_str("claude"), Some(Format::Claude));
        assert_eq!(Format::from_str("ChatGPT"), Some(Format::ChatGpt));
        assert_eq!(Format::from_str("SLACK"), Some(Format::Slack));
        assert_eq!(Format::from_str("unknown"), None);
    }
}
