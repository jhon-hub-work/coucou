// OpenCode Go client — OpenAI-compatible chat completions with multi-turn
// history; text/code files are inlined and images go as image_url data URLs.
//
// Everything happens here rather than in the island: the API key never leaves
// the Rust side, and file bytes never cross the IPC boundary.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{platform, secrets};

const ENDPOINT: &str = "https://opencode.ai/zen/go/v1/chat/completions";
const USER_AGENT: &str = concat!("Boo/", env!("CARGO_PKG_VERSION"));
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;
const NO_KEY: &str = "No OpenCode Go key. Log in to OpenCode Go or paste a key in Settings.";

pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

const SYSTEM_PROMPT: &str = "You are Boo, a friendly little ghost assistant living at the top of the user's screen. \
You can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

pub struct Chat {
    /// Full multi-turn history (OpenAI-style messages, system prompt excluded).
    messages: Mutex<Vec<Value>>,
    /// Sent as x-opencode-session; the API refuses requests without one.
    session: Mutex<String>,
}

impl Default for Chat {
    fn default() -> Self {
        Self { messages: Mutex::default(), session: Mutex::new(new_session_id()) }
    }
}

fn new_session_id() -> String {
    // RandomState is seeded from the OS per instance — enough for an opaque id.
    let r = || RandomState::new().build_hasher().finish();
    format!("boo-{:016x}{:016x}", r(), r())
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
        *self.session.lock().unwrap() = new_session_id();
    }

    fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    fn push(&self, message: Value) {
        self.messages.lock().unwrap().push(message);
    }

    fn pop(&self) {
        self.messages.lock().unwrap().pop();
    }

    fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }

    fn session(&self) -> String {
        self.session.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

/// Where OpenCode's own login keeps its keys.
fn auth_json_path() -> std::path::PathBuf {
    platform::home_dir().join(".local").join("share").join("opencode").join("auth.json")
}

/// `{"opencode-go": {"type": "api", "key": "..."}}` → the key.
fn parse_auth_json(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let key = v.get("opencode-go")?.get("key")?.as_str()?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// Read-only, at call time, never stored.
fn opencode_login_key() -> Option<String> {
    parse_auth_json(&std::fs::read_to_string(auth_json_path()).ok()?)
}

/// Saved key first, then the OpenCode login.
fn api_key() -> Option<String> {
    secrets::get("opencode-go-api-key").or_else(opencode_login_key)
}

/// True when the OpenCode login file holds a key (the settings window uses it
/// to show green without a saved key).
pub fn login_available() -> bool {
    opencode_login_key().is_some()
}

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let key = api_key().ok_or_else(|| NO_KEY.to_string())?;
    // A settings.json from the Claude days still names a Claude model.
    let model = if model.starts_with("claude") { DEFAULT_MODEL } else { model };

    let mut content: Vec<Value> = Vec::new();
    let mut has_image = false;

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path)? {
                    has_image = block.get("type").and_then(Value::as_str) == Some("image_url");
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));

    chat.push(json!({ "role": "user", "content": content }));

    let mut messages = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    messages.extend(chat.snapshot());
    let body = json!({ "model": model, "max_tokens": MAX_TOKENS, "messages": messages });

    let response = match call(&key, &chat.session(), &body).await {
        Ok(v) => v,
        Err(err) => {
            chat.pop(); // keep the history consistent with what the model saw
            return Err(if has_image && err.contains("400") {
                format!("{err}\nThis model may not read images. Pick a vision model (kimi-k3, qwen3.8-flash) in Settings.")
            } else {
                err
            });
        }
    };

    // deepseek also returns reasoning_content; only the answer matters.
    let text = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    if text.is_empty() {
        chat.pop();
        return Err("No response text.".into());
    }
    chat.push(json!({ "role": "assistant", "content": text }));
    Ok(ChatReply { text })
}

async fn call(key: &str, session: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .post(ENDPOINT)
        .bearer_auth(key)
        .header("x-opencode-session", session)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("OpenCode Go {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// image → image_url data URL, text/code → inline text. PDFs are refused with a
/// clear message; `Ok(None)` means the file is skipped (too big or unreadable).
fn file_block(path: &str) -> Result<Option<Value>, String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media = match ext.as_str() {
        "pdf" => return Err("This file type isn't supported in chat yet (PDF).".into()),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };

    if let Some(media) = media {
        let Ok(bytes) = std::fs::read(path) else { return Ok(None) };
        return Ok(Some(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{media};base64,{}", base64(&bytes)) },
        })));
    }

    let Some(len) = std::fs::metadata(path).ok().map(|m| m.len()) else { return Ok(None) };
    if len > MAX_INLINE_TEXT {
        return Ok(None);
    }
    Ok(std::fs::read_to_string(path)
        .ok()
        .map(|text| json!({ "type": "text", "text": format!("File contents:\n{text}") })))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, parse_auth_json};

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn auth_json_key_parsing() {
        let ok = r#"{"other":{"type":"api","key":"x"},"opencode-go":{"type":"api","key":" sk-test "}}"#;
        assert_eq!(parse_auth_json(ok).as_deref(), Some("sk-test"));
        assert_eq!(parse_auth_json(r#"{"opencode-go":{"type":"api","key":""}}"#), None);
        assert_eq!(parse_auth_json(r#"{"opencode":{"key":"x"}}"#), None);
        assert_eq!(parse_auth_json("not json"), None);
    }

    /// Real call to OpenCode Go — run with `cargo test -p boo live -- --ignored`.
    #[test]
    #[ignore]
    fn live_roundtrip() {
        let chat = super::Chat::default();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let ask = |q: &str| rt.block_on(super::send(&chat, super::DEFAULT_MODEL, q.into(), None));
        assert!(!ask("Reply with the single word: pong").expect("turn 1").text.is_empty());
        // Second turn proves multi-turn history is accepted.
        assert!(!ask("What did I ask you to say?").expect("turn 2").text.is_empty());
    }
}
