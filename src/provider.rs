use crate::config::{check_url, Provider};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

pub fn ready(provider: &Provider) -> bool {
    std::env::var(&provider.api_key_env).is_ok()
        || reqwest::Url::parse(&provider.base_url)
            .ok()
            .is_some_and(|u| matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
}

pub async fn completion(
    provider: &Provider,
    messages: &[Value],
    tools: Vec<Value>,
    cancel: CancellationToken,
    mut on_delta: impl FnMut(&str),
) -> Result<Value> {
    check_url(&provider.base_url)?;
    if !ready(provider) {
        bail!(
            "Set {} or configure a local OpenAI-compatible provider with `mutlink model set`",
            provider.api_key_env
        );
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;
    let mut request = client
        .post(format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        ))
        .json(&json!({
            "model": provider.model, "messages": messages, "tools": tools,
            "stream": true, "max_tokens": 8192
        }));
    if let Ok(key) = std::env::var(&provider.api_key_env) {
        request = request.bearer_auth(key);
    }
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("Cancelled"),
        result = request.send() => result.context("Could not reach the model provider")?,
    };
    if !response.status().is_success() {
        // Provider error bodies may contain submitted content. Surface the status only.
        bail!("Model provider returned HTTP {}", response.status());
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut content = String::new();
    let mut calls: BTreeMap<usize, Value> = BTreeMap::new();
    let mut finished = false;
    let mut total = 0usize;
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => bail!("Cancelled"),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk?;
        total += chunk.len();
        if total > 4 * 1024 * 1024 {
            bail!("Model response exceeded the limit");
        }
        buffer.extend_from_slice(&chunk);
        while let Some(index) = buffer.iter().position(|c| *c == b'\n') {
            let line: Vec<u8> = buffer.drain(..=index).collect();
            let line = std::str::from_utf8(&line)?.trim_end();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                finished = true;
                break;
            }
            let value: Value = serde_json::from_str(data).context("Malformed model stream")?;
            if value.get("error").is_some() {
                bail!("Model provider reported a streaming error");
            }
            let Some(choice) = value["choices"].as_array().and_then(|v| v.first()) else {
                continue;
            };
            if !choice["finish_reason"].is_null() {
                if matches!(
                    choice["finish_reason"].as_str(),
                    Some("length" | "content_filter")
                ) {
                    bail!("Model response was truncated or filtered; no partial tool call was executed");
                }
                finished = true;
            }
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str() {
                content.push_str(text);
                on_delta(text);
            }
            if let Some(parts) = delta["tool_calls"].as_array() {
                for part in parts {
                    let index = part["index"].as_u64().context("Missing tool call index")? as usize;
                    if index > 32 {
                        bail!("Too many tool calls in a turn");
                    }
                    let call = calls.entry(index).or_insert_with(|| {
                        json!({
                            "id": "", "type": "function", "function": {"name": "", "arguments": ""}
                        })
                    });
                    for (destination, source) in [
                        ("/id", &part["id"]),
                        ("/function/name", &part["function"]["name"]),
                        ("/function/arguments", &part["function"]["arguments"]),
                    ] {
                        if let Some(text) = source.as_str() {
                            let target = call.pointer_mut(destination).unwrap();
                            *target =
                                Value::String(format!("{}{text}", target.as_str().unwrap_or("")));
                        }
                    }
                }
            }
        }
        if finished {
            break;
        }
    }
    if !finished {
        bail!("Model stream disconnected before completion");
    }
    let mut message = json!({"role":"assistant", "content": content});
    if !calls.is_empty() {
        let values: Vec<Value> = calls.into_values().collect();
        for call in &values {
            if call["id"].as_str().unwrap_or("").is_empty()
                || call["function"]["name"].as_str().unwrap_or("").is_empty()
            {
                bail!("Incomplete tool call");
            }
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap_or(""))?;
        }
        message["tool_calls"] = json!(values);
    }
    Ok(message)
}
