use crate::config::{Config, ExpectedCall};
use anyhow::{anyhow, bail, ensure, Result};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentEvent {
    pub seconds: f64,
    pub channel: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub episode_id: usize,
    pub scheduled: f64,
    pub dispatch: f64,
    pub terminal: f64,
    pub status: String,
    pub http_status: Option<u16>,
    pub finish_reason: Option<String>,
    pub stream: bool,
    pub events: Vec<ContentEvent>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub token_source: String,
    pub answer: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
#[derive(Clone, Debug)]
pub struct Response {
    pub record: RequestRecord,
    pub answer: String,
    pub reasoning: String,
    pub calls: Vec<ToolCall>,
}
impl ToolCall {
    pub fn expected(&self) -> Result<ExpectedCall> {
        let arguments: Value =
            serde_json::from_str(&self.arguments).map_err(|_| anyhow!("invalid_tool_arguments"))?;
        ensure!(arguments.is_object(), "invalid_tool_arguments");
        Ok(ExpectedCall {
            name: self.name.clone(),
            arguments,
        })
    }
    pub fn as_message(&self) -> Value {
        json!({"id":self.id,"type":"function","function":{"name":self.name,"arguments":self.arguments}})
    }
}

pub fn client(config: &Config) -> Result<Client> {
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(config.load.concurrency);
    if let Some(path) = &config.target.ca_file {
        builder =
            builder.add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
    }
    Ok(builder.build()?)
}
pub fn auth(env: &Option<String>) -> Result<Option<String>> {
    env.as_ref()
        .map(|key| {
            std::env::var(key)
                .map_err(|_| anyhow!("configured credential environment variable is missing"))
        })
        .transpose()
}

pub fn payload(config: &Config, messages: &[Value], tools: &[Value]) -> Value {
    let g = &config.generation;
    let mut body = json!({"model":config.target.model,"stream":g.stream,"max_tokens":g.max_tokens,"temperature":g.temperature,"seed":g.seed,"n":1});
    if config.target.adapter == "completions" {
        body["prompt"] = messages[0]["content"].clone();
    } else {
        body["messages"] = json!(messages);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    if g.stream && g.include_usage {
        body["stream_options"] = json!({"include_usage":true});
    }
    for (k, v) in &g.extra {
        body[k] = v.clone();
    }
    body
}

pub async fn request(
    client: &Client,
    config: &Config,
    body: Value,
    episode_id: usize,
    turn: usize,
    scheduled: f64,
    clock: Instant,
) -> Response {
    let mut out = Response {
        record: RequestRecord {
            id: format!("{episode_id}:{turn}"),
            episode_id,
            scheduled,
            dispatch: clock.elapsed().as_secs_f64(),
            terminal: 0.0,
            status: "pending".into(),
            http_status: None,
            finish_reason: None,
            stream: config.generation.stream,
            events: vec![],
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            token_source: "unknown".into(),
            answer: None,
        },
        answer: String::new(),
        reasoning: String::new(),
        calls: vec![],
    };
    let result = tokio::time::timeout(
        Duration::from_secs_f64(config.load.request_timeout_seconds),
        receive(client, config, body, clock, &mut out),
    )
    .await;
    out.record.status = match result {
        Err(_) => "timeout".into(),
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(())) => "success".into(),
    };
    out.record.terminal = clock.elapsed().as_secs_f64();
    if config.save_content {
        out.record.answer = Some(out.answer.clone());
    }
    out
}

async fn receive(
    client: &Client,
    config: &Config,
    body: Value,
    clock: Instant,
    out: &mut Response,
) -> Result<()> {
    let path = if config.target.adapter == "completions" {
        "completions"
    } else {
        "chat/completions"
    };
    let mut req = client
        .post(format!(
            "{}/{path}",
            config.target.base_url.trim_end_matches('/')
        ))
        .json(&body);
    if let Some(key) = auth(&config.target.api_key_env)? {
        req = req.bearer_auth(key);
    }
    let res = req.send().await.map_err(|_| anyhow!("transport_error"))?;
    out.record.http_status = Some(res.status().as_u16());
    if res.status() == StatusCode::TOO_MANY_REQUESTS {
        bail!("rate_limited");
    }
    if !res.status().is_success() {
        bail!("http_error");
    }
    let limit = config.load.max_response_bytes;
    let bytes =
        res.bytes_stream()
            .map_err(std::io::Error::other)
            .scan(0usize, move |total, bytes| {
                let result = bytes.and_then(|bytes| {
                    *total += bytes.len();
                    if *total > limit {
                        Err(std::io::Error::other("response_limit"))
                    } else {
                        Ok(bytes)
                    }
                });
                futures_util::future::ready(Some(result))
            });
    if config.generation.stream {
        let mut stream = Box::pin(bytes.eventsource());
        let mut done = false;
        while let Some(event) = stream.next().await {
            let event = event.map_err(|_| anyhow!("stream_error_or_size_limit"))?;
            if event.data == "[DONE]" {
                done = true;
                break;
            }
            if event.data.trim().is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(&event.data).map_err(|_| anyhow!("invalid_stream_json"))?;
            parse(&value, true, clock.elapsed().as_secs_f64(), out)?;
        }
        ensure!(
            done && out.record.finish_reason.is_some(),
            "incomplete_stream"
        );
    } else {
        let chunks: Vec<_> = bytes
            .try_collect()
            .await
            .map_err(|_| anyhow!("body_error_or_size_limit"))?;
        let body: Vec<u8> = chunks.iter().flat_map(|b| b.iter().copied()).collect();
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| anyhow!("invalid_response_json"))?;
        parse(&value, false, clock.elapsed().as_secs_f64(), out)?;
        ensure!(out.record.finish_reason.is_some(), "missing_finish_reason");
    }
    if !out.calls.is_empty() {
        ensure!(
            out.record.finish_reason.as_deref() == Some("tool_calls"),
            "incomplete_tool_call"
        );
        let mut ids = std::collections::HashSet::new();
        for c in &out.calls {
            ensure!(
                !c.id.is_empty() && !c.name.is_empty() && ids.insert(&c.id),
                "invalid_tool_identity"
            );
        }
    }
    ensure!(
        out.record.finish_reason.as_deref() != Some("tool_calls") || !out.calls.is_empty(),
        "missing_tool_calls"
    );
    if out.record.output_tokens == Some(0)
        && (!out.answer.is_empty() || !out.calls.is_empty() || !out.reasoning.is_empty())
    {
        out.record.output_tokens = None;
        out.record.token_source = "invalid_usage".into();
    }
    Ok(())
}

pub fn parse(value: &Value, streaming: bool, now: f64, out: &mut Response) -> Result<()> {
    ensure!(value.get("error").is_none(), "server_error");
    if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
        out.record.input_tokens = usage["prompt_tokens"].as_u64();
        out.record.output_tokens = usage["completion_tokens"].as_u64();
        out.record.reasoning_tokens =
            usage["completion_tokens_details"]["reasoning_tokens"].as_u64();
        if out.record.output_tokens.is_some() {
            out.record.token_source = "server_reported".into();
        }
    }
    let choices = value["choices"]
        .as_array()
        .ok_or_else(|| anyhow!("missing_choices"))?;
    ensure!(choices.len() <= 1, "multiple_choices_not_supported");
    let Some(choice) = choices.first() else {
        return Ok(());
    };
    if let Some(index) = choice["index"].as_u64() {
        ensure!(index == 0, "unexpected_choice_index");
    }
    let finish = choice["finish_reason"].as_str();
    let content = if streaming {
        &choice["delta"]
    } else {
        &choice["message"]
    };
    let text = content["content"]
        .as_str()
        .or_else(|| choice["text"].as_str())
        .unwrap_or("");
    let reasoning = content["reasoning_content"]
        .as_str()
        .or_else(|| content["reasoning"].as_str())
        .unwrap_or("");
    if !text.is_empty() {
        add_event(out, streaming, now, "answer");
        out.answer.push_str(text);
    }
    if !reasoning.is_empty() {
        add_event(out, streaming, now, "reasoning");
        out.reasoning.push_str(reasoning);
    }
    if let Some(calls) = content["tool_calls"].as_array() {
        let mut touched = false;
        for (position, call) in calls.iter().enumerate() {
            let index = if streaming {
                call["index"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("missing_tool_index"))? as usize
            } else {
                position
            };
            ensure!(index < 1024, "tool_index_limit");
            while out.calls.len() <= index {
                out.calls.push(ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
            }
            let slot = &mut out.calls[index];
            if let Some(id) = call["id"].as_str() {
                if slot.id.is_empty() {
                    slot.id = id.into();
                } else {
                    ensure!(slot.id == id, "conflicting_tool_id");
                }
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.name.push_str(name);
                touched |= !name.is_empty();
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                slot.arguments.push_str(args);
                touched |= !args.is_empty();
            }
        }
        if touched {
            add_event(out, streaming, now, "tool");
        }
    }
    if let Some(reason) = finish {
        out.record.finish_reason = Some(reason.to_string());
    }
    Ok(())
}
fn add_event(out: &mut Response, streaming: bool, now: f64, channel: &str) {
    if streaming {
        out.record.events.push(ContentEvent {
            seconds: now,
            channel: channel.into(),
        });
    }
}

pub fn fixture_definitions(names: &[String]) -> Vec<Value> {
    let definitions: BTreeMap<&str, Value> = [
        ("add", json!({"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"],"additionalProperties":false})),
        ("kv_get", json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false})),
        ("kv_set", json!({"type":"object","properties":{"key":{"type":"string"},"value":{}},"required":["key","value"],"additionalProperties":false})),
    ].into();
    names.iter().map(|name| json!({"type":"function","function":{"name":name,"description":format!("Deterministic test fixture {name}"),"parameters":definitions[name.as_str()]}})).collect()
}
