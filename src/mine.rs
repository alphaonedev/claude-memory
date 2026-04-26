// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Retroactive conversation import from Claude, `ChatGPT`, and Slack exports.

use anyhow::{Context, Result, bail};
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

#[test]
fn mine_handles_empty_namespace() {
    // Empty namespace string should still parse and convert to memory.
    let conv = Conversation {
        id: "test-empty-ns".to_string(),
        title: Some("Empty Namespace Test".to_string()),
        messages: vec![Message {
            role: "user".to_string(),
            content: "Test message with substantial content for conversion".to_string(),
            timestamp: None,
        }],
        created_at: None,
    };
    let mem = conversation_to_memory(&conv, Format::Claude);
    assert!(mem.is_some());
    let m = mem.unwrap();
    assert_eq!(m.source_format, "mine-claude");
}

#[test]
fn mine_skips_archived_memories() {
    // A conversation with no messages returns None (archived state).
    let conv = Conversation {
        id: "empty".to_string(),
        title: Some("Should Skip".to_string()),
        messages: vec![], // Empty — treated as archived
        created_at: None,
    };
    assert!(conversation_to_memory(&conv, Format::Claude).is_none());
}

#[test]
fn mine_with_zero_limit_returns_empty() {
    // When mining with zero messages, conversation_to_memory returns None.
    let conv = Conversation {
        id: "zero-limit".to_string(),
        title: None,
        messages: vec![], // No messages
        created_at: None,
    };
    let mem = conversation_to_memory(&conv, Format::ChatGpt);
    assert!(mem.is_none());
}

// ---------------------------------------------------------------------------
// W12-D: parser branch coverage (Claude mapping format, ChatGPT edge cases,
// Slack error paths, content extraction variants, converter limits).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests_w12d {
    use super::*;
    use std::fs;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    fn temp_file(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    // ---- Format::source_tag — exercise all variants -----------------------
    #[test]
    fn source_tag_all_variants() {
        assert_eq!(Format::Claude.source_tag(), "mine-claude");
        assert_eq!(Format::ChatGpt.source_tag(), "mine-chatgpt");
        assert_eq!(Format::Slack.source_tag(), "mine-slack");
    }

    // ---- parse_claude — error paths ---------------------------------------
    #[test]
    fn parse_claude_missing_file_errors() {
        let p = std::path::Path::new("/nonexistent/path/to/claude_does_not_exist.jsonl");
        let err = parse_claude(p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to read Claude export"),
            "expected read-failure context, got: {msg}"
        );
    }

    #[test]
    fn parse_claude_invalid_json_line_errors() {
        // Second line is malformed; first line is fine.
        let jsonl = "{\"uuid\":\"a\",\"chat_messages\":[{\"sender\":\"human\",\"text\":\"hi\"}]}\nNOT JSON\n";
        let f = temp_file(jsonl);
        let err = parse_claude(f.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid JSON on line 2"),
            "want line 2 hint, got: {msg}"
        );
    }

    #[test]
    fn parse_claude_skips_conversations_with_no_messages() {
        // Conversation with empty chat_messages should be filtered (None branch).
        let jsonl = r#"{"uuid":"empty","name":"Empty","chat_messages":[]}
{"uuid":"good","name":"Good","chat_messages":[{"sender":"human","text":"hi"}]}"#;
        let f = temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1, "empty conv should be skipped");
        assert_eq!(convs[0].id, "good");
    }

    #[test]
    fn parse_claude_skips_messages_without_content() {
        // Messages with empty/missing text should be skipped, but conv kept if any survive.
        let jsonl = r#"{"uuid":"c1","chat_messages":[{"sender":"human","text":""},{"sender":"assistant","text":"hello"}]}"#;
        let f = temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "assistant");
    }

    #[test]
    fn parse_claude_uses_role_fallback_and_timestamps() {
        // Use `role` instead of `sender`; `content` instead of `text`; `timestamp` instead of `created_at`.
        let jsonl = r#"{"uuid":"c1","chat_messages":[{"role":"assistant","content":"reply","timestamp":"2024-01-01T00:00:00Z"}]}"#;
        let f = temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages[0].role, "assistant");
        assert_eq!(convs[0].messages[0].content, "reply");
        assert_eq!(
            convs[0].messages[0].timestamp.as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
    }

    // ---- parse_claude — mapping format (Format 2) -------------------------
    #[test]
    fn parse_claude_mapping_format() {
        // No chat_messages — falls through to mapping branch.
        let jsonl = r#"{"uuid":"map1","name":"Mapping Conv","mapping":{"n1":{"message":{"role":"user","content":{"parts":["first"]},"create_time":1700000001}},"n2":{"message":{"author":{"role":"assistant"},"content":{"parts":["second"]},"create_time":1700000002}},"n3":{"message":{"role":"system","content":{"parts":["ignored"]}}}}}"#;
        let f = temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.title.as_deref(), Some("Mapping Conv"));
        // System message dropped; user+assistant retained, ordered by create_time.
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].content, "first");
        assert_eq!(conv.messages[1].content, "second");
        // create_time -> RFC3339 timestamp present
        assert!(conv.messages[0].timestamp.is_some());
    }

    #[test]
    fn parse_claude_mapping_skips_empty_content_nodes() {
        // Mapping with one node whose content is empty (no parts/text) should be dropped.
        let jsonl = r#"{"uuid":"map2","mapping":{"n1":{"message":{"role":"user","content":{"parts":[]}}},"n2":{"message":{"role":"user","content":{"parts":["kept"]},"create_time":1700000005}}}}"#;
        let f = temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "kept");
    }

    #[test]
    fn parse_claude_mapping_uuid_fallback_and_no_messages() {
        // No uuid -> fallback id; mapping with only system messages -> filtered as None.
        let jsonl = r#"{"mapping":{"n1":{"message":{"role":"system","content":{"parts":["only system"]}}}}}"#;
        let f = temp_file(jsonl);
        let convs = parse_claude(f.path()).unwrap();
        assert_eq!(convs.len(), 0, "system-only conversation is dropped");
    }

    // ---- parse_chatgpt — error & edge cases -------------------------------
    #[test]
    fn parse_chatgpt_missing_file_errors() {
        let p = std::path::Path::new("/nonexistent/chatgpt.json");
        let err = parse_chatgpt(p).unwrap_err();
        assert!(format!("{err:#}").contains("failed to read ChatGPT export"));
    }

    #[test]
    fn parse_chatgpt_invalid_json_errors() {
        let f = temp_file("not really json");
        let err = parse_chatgpt(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON in ChatGPT export"));
    }

    #[test]
    fn parse_chatgpt_top_level_object_errors() {
        let f = temp_file(r#"{"not":"an array"}"#);
        let err = parse_chatgpt(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("expected JSON array"));
    }

    #[test]
    fn parse_chatgpt_skips_system_and_empty_messages() {
        // System role skipped; empty-content message skipped; final conv has 1 message.
        let json = r#"[{"id":"c1","title":"T","create_time":1700000000,"mapping":{
            "n1":{"message":{"author":{"role":"system"},"content":{"parts":["sys ignored"]},"create_time":1700000001}},
            "n2":{"message":{"author":{"role":"user"},"content":{"parts":[]},"create_time":1700000002}},
            "n3":{"message":{"author":{"role":"user"},"content":{"parts":["kept"]},"create_time":1700000003}}
        }}]"#;
        let f = temp_file(json);
        let convs = parse_chatgpt(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "kept");
        assert!(convs[0].messages[0].timestamp.is_some());
    }

    #[test]
    fn parse_chatgpt_drops_conversations_with_no_messages() {
        // Mapping containing only system messages -> conv filtered out.
        let json = r#"[{"id":"only-sys","mapping":{
            "n1":{"message":{"author":{"role":"system"},"content":{"parts":["x"]}}}
        }}]"#;
        let f = temp_file(json);
        let convs = parse_chatgpt(f.path()).unwrap();
        assert!(convs.is_empty());
    }

    #[test]
    fn parse_chatgpt_id_fallback_when_missing() {
        // Conv missing both id and mapping -> falls through with no messages -> dropped.
        // But if there ARE messages, the fallback id chatgpt-N path is exercised.
        let json = r#"[{"mapping":{"n1":{"message":{"author":{"role":"user"},"content":{"parts":["hello"]},"create_time":1700000010}}}}]"#;
        let f = temp_file(json);
        let convs = parse_chatgpt(f.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].id, "chatgpt-0");
    }

    #[test]
    fn parse_chatgpt_empty_array() {
        let f = temp_file("[]");
        let convs = parse_chatgpt(f.path()).unwrap();
        assert!(convs.is_empty());
    }

    // ---- parse_slack — error and edge cases -------------------------------
    #[test]
    fn parse_slack_path_must_be_directory() {
        let f = temp_file("not a dir");
        let err = parse_slack(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("must be a directory"));
    }

    #[test]
    fn parse_slack_skips_non_directory_entries_in_root() {
        // A loose file at the export root should be skipped (only subdirs are channels).
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.txt"), "hello").unwrap();
        let channel = dir.path().join("general");
        fs::create_dir(&channel).unwrap();
        fs::write(
            channel.join("2024-01-01.json"),
            r#"[{"user":"U1","text":"hi","ts":"1700000000.0"}]"#,
        )
        .unwrap();
        let convs = parse_slack(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn parse_slack_skips_non_json_files_and_empty_text() {
        let dir = tempfile::tempdir().unwrap();
        let channel = dir.path().join("random");
        fs::create_dir(&channel).unwrap();
        // Non-JSON file (should be skipped via extension filter).
        fs::write(channel.join("note.txt"), "ignored").unwrap();
        // JSON with one valid + one empty-text message.
        let json = r#"[{"user":"U1","text":"","ts":"1700000000.0"},{"username":"bot","text":"hello","ts":"1700000001.0"}]"#;
        fs::write(channel.join("2024-01-02.json"), json).unwrap();
        let convs = parse_slack(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        // username fallback exercised
        assert_eq!(convs[0].messages[0].role, "bot");
    }

    #[test]
    fn parse_slack_invalid_json_in_channel_errors() {
        let dir = tempfile::tempdir().unwrap();
        let channel = dir.path().join("oops");
        fs::create_dir(&channel).unwrap();
        fs::write(channel.join("2024-01-01.json"), "not json").unwrap();
        let err = parse_slack(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON"));
    }

    #[test]
    fn parse_slack_drops_channels_with_no_messages() {
        // Channel with only empty-text messages -> dropped from output.
        let dir = tempfile::tempdir().unwrap();
        let empty_chan = dir.path().join("silent");
        fs::create_dir(&empty_chan).unwrap();
        fs::write(
            empty_chan.join("2024-01-01.json"),
            r#"[{"user":"U1","text":"","ts":"1700000000.0"}]"#,
        )
        .unwrap();
        let live_chan = dir.path().join("alive");
        fs::create_dir(&live_chan).unwrap();
        fs::write(
            live_chan.join("2024-01-01.json"),
            r#"[{"user":"U2","text":"hi","ts":"1700000001.0"}]"#,
        )
        .unwrap();
        let convs = parse_slack(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].id, "slack-alive");
    }

    #[test]
    fn parse_slack_handles_missing_timestamp() {
        // Message with no `ts` -> timestamp None branch.
        let dir = tempfile::tempdir().unwrap();
        let channel = dir.path().join("notime");
        fs::create_dir(&channel).unwrap();
        fs::write(
            channel.join("2024-01-01.json"),
            r#"[{"user":"U1","text":"hi"}]"#,
        )
        .unwrap();
        let convs = parse_slack(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert!(convs[0].messages[0].timestamp.is_none());
    }

    #[test]
    fn parse_slack_skips_non_array_top_level() {
        // JSON file that is an object (not an array) -> the `if let Some(arr)` branch is
        // simply skipped; channel ends up with no messages and is dropped.
        let dir = tempfile::tempdir().unwrap();
        let channel = dir.path().join("weird");
        fs::create_dir(&channel).unwrap();
        fs::write(channel.join("2024-01-01.json"), r#"{"not":"an array"}"#).unwrap();
        let convs = parse_slack(dir.path()).unwrap();
        assert!(convs.is_empty());
    }

    // ---- extract_text_content — array branches ----------------------------
    #[test]
    fn extract_text_content_array_of_strings() {
        let v = serde_json::json!(["one", "two"]);
        assert_eq!(extract_text_content(&v).as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn extract_text_content_array_of_text_objects() {
        // Claude tool-use / text-block format: array of {"type":"text","text":"..."} objects.
        let v = serde_json::json!([
            {"type":"text","text":"alpha"},
            {"type":"text","text":"beta"}
        ]);
        assert_eq!(extract_text_content(&v).as_deref(), Some("alpha\nbeta"));
    }

    #[test]
    fn extract_text_content_empty_and_non_text() {
        // Empty array -> None
        assert!(extract_text_content(&serde_json::json!([])).is_none());
        // Array of objects with no "text" field -> None (parts vec ends up empty).
        let v = serde_json::json!([{"type":"image","url":"x"}]);
        assert!(extract_text_content(&v).is_none());
        // Null -> None
        assert!(extract_text_content(&serde_json::Value::Null).is_none());
    }

    // ---- extract_message_content — branch coverage ------------------------
    #[test]
    fn extract_message_content_string_form() {
        // content is a plain string (not parts array, not object with text).
        let mut m = serde_json::Map::new();
        m.insert("content".into(), serde_json::json!("plain text"));
        assert_eq!(extract_message_content(&m), "plain text");
    }

    #[test]
    fn extract_message_content_text_field_under_content() {
        // content is an object with a "text" field but no "parts".
        let mut m = serde_json::Map::new();
        m.insert("content".into(), serde_json::json!({"text":"nested-text"}));
        assert_eq!(extract_message_content(&m), "nested-text");
    }

    #[test]
    fn extract_message_content_top_level_text_field() {
        // No content at all; falls through to top-level text.
        let mut m = serde_json::Map::new();
        m.insert("text".into(), serde_json::json!("top-text"));
        assert_eq!(extract_message_content(&m), "top-text");
    }

    #[test]
    fn extract_message_content_returns_empty_when_unparseable() {
        // No useful fields.
        let m = serde_json::Map::new();
        assert!(extract_message_content(&m).is_empty());
    }

    #[test]
    fn extract_message_content_parts_array_skips_non_strings() {
        // parts mixing strings and non-strings: only strings are joined.
        let mut m = serde_json::Map::new();
        m.insert(
            "content".into(),
            serde_json::json!({"parts":["good", {"img":1}, "also-good"]}),
        );
        assert_eq!(extract_message_content(&m), "good\nalso-good");
    }

    // ---- conversation_to_memory — title & content branches ----------------
    #[test]
    fn conversation_to_memory_empty_title_falls_back_to_first_user() {
        // title is Some("") -> filter rejects empty -> first-user path.
        let conv = Conversation {
            id: "c".into(),
            title: Some(String::new()),
            messages: vec![
                Message {
                    role: "assistant".into(),
                    content: "hello back".into(),
                    timestamp: None,
                },
                Message {
                    role: "user".into(),
                    content: "hello".into(),
                    timestamp: None,
                },
            ],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::Slack).unwrap();
        assert_eq!(mem.title, "hello");
        assert_eq!(mem.source_format, "mine-slack");
    }

    #[test]
    fn conversation_to_memory_no_user_uses_first_message() {
        // No user/human role -> first message used.
        let conv = Conversation {
            id: "c".into(),
            title: None,
            messages: vec![
                Message {
                    role: "assistant".into(),
                    content: "only assistant".into(),
                    timestamp: None,
                },
                Message {
                    role: "tool".into(),
                    content: "tool-out".into(),
                    timestamp: None,
                },
            ],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::ChatGpt).unwrap();
        assert_eq!(mem.title, "only assistant");
    }

    #[test]
    fn conversation_to_memory_title_truncates_to_100_chars() {
        let long_title = "x".repeat(250);
        let conv = Conversation {
            id: "c".into(),
            title: Some(long_title),
            messages: vec![Message {
                role: "user".into(),
                content: "body".into(),
                timestamp: None,
            }],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::Claude).unwrap();
        assert_eq!(mem.title.len(), 100);
    }

    #[test]
    fn conversation_to_memory_first_user_content_truncates() {
        // No title, first user content very long -> truncated to 100 chars.
        let long_msg = "y".repeat(200);
        let conv = Conversation {
            id: "c".into(),
            title: None,
            messages: vec![Message {
                role: "user".into(),
                content: long_msg,
                timestamp: None,
            }],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::Claude).unwrap();
        assert_eq!(mem.title.len(), 100);
    }

    #[test]
    fn conversation_to_memory_stops_at_max_content_size() {
        // Build a single huge message exceeding MAX_CONTENT_SIZE so the loop
        // breaks before appending it. With the very first message rejected,
        // content stays empty and the function returns None.
        let big = "z".repeat(MAX_CONTENT_SIZE + 10);
        let conv = Conversation {
            id: "c".into(),
            title: Some("t".into()),
            messages: vec![Message {
                role: "user".into(),
                content: big,
                timestamp: None,
            }],
            created_at: None,
        };
        // First (and only) message exceeds the cap -> content empty -> None.
        assert!(conversation_to_memory(&conv, Format::Claude).is_none());
    }

    #[test]
    fn conversation_to_memory_truncates_on_second_message() {
        // First small message accepted; second huge message rejected by size cap.
        let big = "z".repeat(MAX_CONTENT_SIZE);
        let conv = Conversation {
            id: "c".into(),
            title: Some("t".into()),
            messages: vec![
                Message {
                    role: "user".into(),
                    content: "small".into(),
                    timestamp: None,
                },
                Message {
                    role: "assistant".into(),
                    content: big,
                    timestamp: None,
                },
            ],
            created_at: None,
        };
        let mem = conversation_to_memory(&conv, Format::Claude).unwrap();
        assert!(mem.content.contains("small"));
        // The huge one was skipped due to size cap.
        assert!(!mem.content.contains(&"z".repeat(100)));
    }

    // ---- truncate — char boundary loop ------------------------------------
    #[test]
    fn truncate_respects_char_boundary() {
        // "héllo" — multi-byte char at index 1; truncating at byte 2 must back off.
        let s = "héllo";
        // Byte length of "h" + 2 bytes for é = 3. Asking for 2 bytes must back off to 1 ("h").
        let out = truncate(s, 2);
        assert_eq!(out, "h");
    }

    #[test]
    fn truncate_at_exact_boundary_returns_unchanged() {
        let s = "abcdef";
        assert_eq!(truncate(s, 6), "abcdef");
    }

    #[test]
    fn truncate_zero_max_returns_empty() {
        // max_chars = 0 -> while loop exits immediately, slice is "".
        let s = "héllo";
        assert_eq!(truncate(s, 0), "");
    }
}
