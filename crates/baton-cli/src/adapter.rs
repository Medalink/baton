//! Turns an agent's raw hook payload into a turn request. The payload is untrusted input: every field is
//! parsed into a typed value here, and nothing but ids, a directory and a subject leaves this module.

use baton_core::{AgentName, ThreadId};
use serde::Deserialize;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

const TRANSCRIPT_TAIL: u64 = 256 * 1024;

pub struct TurnRequest {
    pub cwd: PathBuf,
    pub thread: ThreadId,
    pub agent: AgentName,
    pub model: Option<String>,
    pub message: String,
}

#[derive(Deserialize)]
struct ClaudeStop {
    session_id: String,
    cwd: String,
    transcript_path: Option<String>,
    hook_event_name: Option<String>,
}

/// `claude` is the hook's argument (`baton hook claude`); `claude-code` is the name recorded in the turn.
pub fn parse(hook_agent: &str, payload: &[u8]) -> Option<TurnRequest> {
    match hook_agent {
        "claude" | "claude-code" => claude_code(payload),
        _ => None,
    }
}

fn claude_code(payload: &[u8]) -> Option<TurnRequest> {
    let stop: ClaudeStop = serde_json::from_slice(payload).ok()?;
    if stop.hook_event_name.as_deref().is_some_and(|e| e != "Stop") {
        return None;
    }
    let transcript = stop.transcript_path.as_deref().and_then(read_tail);
    let (message, model) = transcript.map_or((None, None), |t| latest_prompt_and_model(&t));
    Some(TurnRequest {
        cwd: PathBuf::from(stop.cwd),
        thread: ThreadId::parse(&stop.session_id).ok()?,
        agent: AgentName::parse("claude-code").ok()?,
        model,
        message: message.unwrap_or_default(),
    })
}

/// The last 256 KiB of the transcript. A path in a payload names a file the sender chose, so only a
/// `.jsonl` file is opened and only its tail is read.
fn read_tail(path: &str) -> Option<String> {
    let path = PathBuf::from(path);
    if path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TRANSCRIPT_TAIL).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // A cut in the middle of the file leaves a partial first line.
    Some(if start > 0 {
        text.split_once('\n')
            .map_or(String::new(), |(_, rest)| rest.to_owned())
    } else {
        text
    })
}

fn user_text(entry: &Value) -> Option<String> {
    if entry["isMeta"].as_bool() == Some(true) || entry["isSidechain"].as_bool() == Some(true) {
        return None;
    }
    let content = &entry["message"]["content"];
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

fn latest_prompt_and_model(tail: &str) -> (Option<String>, Option<String>) {
    let (mut prompt, mut model) = (None, None);
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry["type"].as_str() {
            Some("user") if prompt.is_none() => prompt = user_text(&entry),
            Some("assistant") if model.is_none() => {
                model = entry["message"]["model"]
                    .as_str()
                    .filter(|m| !m.starts_with('<'))
                    .map(str::to_owned);
            }
            _ => {}
        }
        if prompt.is_some() && model.is_some() {
            break;
        }
    }
    (prompt, model)
}
