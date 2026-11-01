//! Turns an agent's raw hook payload into a turn request. The payload is untrusted input: every field is
//! parsed into a typed value here, and nothing but ids, a directory and a subject leaves this module.
//! A built-in adapter only maps the fields its agent documents into `baton.turnreq.v1`; a payload it
//! cannot map is dropped, and `baton turn` records the turn instead.

use baton_core::{AgentName, ThreadId, TurnReq};
use serde::Deserialize;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

const TRANSCRIPT_TAIL: u64 = 256 * 1024;
const SUBJECT_CHARS: usize = 200;

fn request(
    cwd: String,
    session: &str,
    agent: &str,
    model: Option<String>,
    subject: String,
) -> Option<TurnReq> {
    Some(TurnReq {
        cwd: PathBuf::from(cwd),
        thread: ThreadId::parse(session).ok()?,
        subject,
        agent: AgentName::parse(agent).ok()?,
        model: model.filter(|m| !m.is_empty()),
        checks: Vec::new(),
    })
}

/// `claude` is the hook's argument (`baton hook claude`); `claude-code` is the name recorded in the turn.
/// `None` means the payload is not the end of a turn this adapter can read.
pub fn parse(hook_agent: &str, payload: &[u8]) -> Option<TurnReq> {
    match hook_agent {
        "claude" | "claude-code" => claude_code(payload),
        "codex" => codex(payload),
        "opencode" => opencode(payload),
        "pi" => pi(payload),
        _ => None,
    }
}

#[derive(Deserialize)]
struct ClaudeStop {
    session_id: String,
    cwd: String,
    transcript_path: Option<String>,
    hook_event_name: Option<String>,
}

fn claude_code(payload: &[u8]) -> Option<TurnReq> {
    let stop: ClaudeStop = serde_json::from_slice(payload).ok()?;
    if stop.hook_event_name.as_deref().is_some_and(|e| e != "Stop") {
        return None;
    }
    let transcript = stop.transcript_path.as_deref().and_then(read_tail);
    let (message, model) = transcript.map_or((None, None), |t| latest_prompt_and_model(&t));
    request(
        stop.cwd,
        &stop.session_id,
        "claude-code",
        model,
        message.unwrap_or_default(),
    )
}

/// Codex runs a hook command with one JSON object on stdin
/// (<https://developers.openai.com/codex/hooks>). Its `Stop` event carries `session_id`, `cwd`,
/// `hook_event_name`, `model`, `turn_id`, `transcript_path` and `last_assistant_message`, and no prompt,
/// so the subject is the start of the agent's last message.
#[derive(Deserialize)]
struct CodexStop {
    session_id: String,
    cwd: String,
    hook_event_name: Option<String>,
    model: Option<String>,
    last_assistant_message: Option<String>,
}

fn codex(payload: &[u8]) -> Option<TurnReq> {
    let stop: CodexStop = serde_json::from_slice(payload).ok()?;
    if stop.hook_event_name.as_deref().is_some_and(|e| e != "Stop") {
        return None;
    }
    let subject = stop.last_assistant_message.as_deref().map(first_line);
    request(
        stop.cwd,
        &stop.session_id,
        "codex",
        stop.model,
        subject.unwrap_or_default(),
    )
}

/// opencode has no command hooks. Its plug-ins receive `{ directory, worktree, ... }` and an event
/// `{ type: "session.idle", properties: { sessionID } }` (`@opencode-ai/plugin`, `@opencode-ai/sdk`
/// `types.gen.d.ts`). The glue in `docs/design.md` pipes that event, with the plug-in's `directory`
/// added, into `baton hook opencode`.
#[derive(Deserialize)]
struct OpencodeIdle {
    #[serde(rename = "type")]
    event: String,
    properties: OpencodeProperties,
    directory: String,
    model: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct OpencodeProperties {
    #[serde(rename = "sessionID")]
    session: String,
}

fn opencode(payload: &[u8]) -> Option<TurnReq> {
    let idle: OpencodeIdle = serde_json::from_slice(payload).ok()?;
    if idle.event != "session.idle" {
        return None;
    }
    let subject = idle.message.as_deref().map(first_line);
    request(
        idle.directory,
        &idle.properties.session,
        "opencode",
        idle.model,
        subject.unwrap_or_default(),
    )
}

/// Pi has no command hooks either. An extension's `agent_settled` handler has `ctx.cwd`,
/// `ctx.sessionManager.getSessionId()` and `ctx.model` (<https://pi.dev/docs/latest/extensions>), which
/// the glue in `docs/design.md` pipes into `baton hook pi` as `session_id`, `cwd` and `model`.
/// The docs do not show the model's field names, so the glue passes `ctx.model.id` unconfirmed.
#[derive(Deserialize)]
struct PiSettled {
    #[serde(rename = "type")]
    event: Option<String>,
    session_id: String,
    cwd: String,
    model: Option<String>,
    message: Option<String>,
}

fn pi(payload: &[u8]) -> Option<TurnReq> {
    let settled: PiSettled = serde_json::from_slice(payload).ok()?;
    if settled
        .event
        .as_deref()
        .is_some_and(|e| e != "agent_settled")
    {
        return None;
    }
    let subject = settled.message.as_deref().map(first_line);
    request(
        settled.cwd,
        &settled.session_id,
        "pi",
        settled.model,
        subject.unwrap_or_default(),
    )
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .chars()
        .take(SUBJECT_CHARS)
        .collect()
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
