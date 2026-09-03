//! VLM sidecar client: any OpenAI-compatible /v1/chat/completions endpoint
//! (llama.cpp server, Ollama with a vision model, or a cloud API). The Rust
//! core never embeds a VLM — swap backends by changing base_url/model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ReplyMessage,
}

#[derive(Debug, Deserialize)]
struct ReplyMessage {
    content: String,
}

pub struct VlmClient {
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl VlmClient {
    /// `base_url` like "http://127.0.0.1:8080/v1".
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn ask(&self, prompt: &str) -> anyhow::Result<String> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                Message {
                    role: "system",
                    content: "You are an assistant that answers where household \
                              items were last seen, using ONLY the sighting log \
                              provided by the user. If the log lacks the item, \
                              say so plainly.",
                },
                Message {
                    role: "user",
                    content: prompt,
                },
            ],
        };
        let resp: ChatResponse = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        resp.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("vlm returned no choices"))
    }
}
