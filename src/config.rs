use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub target: Target,
    pub dataset: PathBuf,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub generation: Generation,
    #[serde(default)]
    pub load: Load,
    #[serde(default)]
    pub slo: Slo,
    #[serde(default)]
    pub save_content: bool,
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Performance,
    Quality,
    QualityUnderLoad,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub base_url: String,
    pub model: String,
    #[serde(default = "chat")]
    pub adapter: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}
fn chat() -> String {
    "chat-completions".into()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Generation {
    pub stream: bool,
    pub include_usage: bool,
    pub max_tokens: u32,
    pub temperature: f64,
    pub seed: u64,
    pub cache_policy: String,
    pub extra: BTreeMap<String, Value>,
}
impl Default for Generation {
    fn default() -> Self {
        Self {
            stream: true,
            include_usage: true,
            max_tokens: 256,
            temperature: 0.0,
            seed: 42,
            cache_policy: "unknown".into(),
            extra: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Load {
    pub kind: LoadKind,
    pub concurrency: usize,
    pub requests: usize,
    pub warmup_requests: usize,
    pub rate: f64,
    pub poisson: bool,
    pub request_timeout_seconds: f64,
    pub episode_timeout_seconds: f64,
    pub max_seconds: f64,
    pub drain_seconds: f64,
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub max_response_bytes: usize,
    pub max_schedule_lag_ms: f64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadKind {
    #[default]
    ClosedLoop,
    OpenLoop,
}
impl Default for Load {
    fn default() -> Self {
        Self {
            kind: LoadKind::ClosedLoop,
            concurrency: 1,
            requests: 20,
            warmup_requests: 0,
            rate: 1.0,
            poisson: true,
            request_timeout_seconds: 60.0,
            episode_timeout_seconds: 120.0,
            max_seconds: 300.0,
            drain_seconds: 60.0,
            max_turns: 8,
            max_tool_calls: 16,
            max_response_bytes: 4 * 1024 * 1024,
            max_schedule_lag_ms: 50.0,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Slo {
    pub ttft_ms: Option<f64>,
    pub e2e_ms: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    pub url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub histograms: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub exclusive: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub group: String,
    pub language: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub scorer: Option<Scorer>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub state: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scorer {
    Exact {
        answers: Vec<String>,
        #[serde(default)]
        case_sensitive: bool,
    },
    Numeric {
        expected: f64,
        #[serde(default)]
        tolerance: f64,
    },
    Contains {
        required: Vec<String>,
    },
    JsonSchema {
        schema: Value,
        #[serde(default)]
        expected: Option<Value>,
    },
    Tools {
        calls: Vec<ExpectedCall>,
        #[serde(default)]
        unordered: bool,
        #[serde(default)]
        final_answers: Vec<String>,
        #[serde(default)]
        final_state: BTreeMap<String, Value>,
    },
    Unsupported {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCall {
    pub name: String,
    pub arguments: Value,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .map_err(|e| {
            anyhow::anyhow!(
                "invalid JSON in {} at line {} column {}",
                path.display(),
                e.line(),
                e.column()
            )
        })
}
fn check_url(raw: &str) -> Result<()> {
    let u = reqwest::Url::parse(raw).context("invalid endpoint URL")?;
    ensure!(
        ["http", "https"].contains(&u.scheme()) && u.host_str().is_some(),
        "endpoint must use HTTP(S)"
    );
    ensure!(
        u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "endpoint URL must not contain credentials, query or fragment"
    );
    Ok(())
}
impl Config {
    pub fn load(path: &Path) -> Result<(Self, Vec<Case>, String)> {
        let mut cfg: Self = read_json(path)?;
        let base = path.parent().unwrap_or(Path::new("."));
        cfg.dataset = base.join(&cfg.dataset);
        if let Some(ca) = &cfg.target.ca_file {
            cfg.target.ca_file = Some(base.join(ca));
        }
        cfg.validate()?;
        let bytes = fs::read(&cfg.dataset).context("cannot read dataset")?;
        ensure!(bytes.len() <= 64 * 1024 * 1024, "dataset exceeds 64 MiB");
        let mut cases = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for (i, line) in std::str::from_utf8(&bytes)?.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let case: Case = serde_json::from_str(line)
                .map_err(|_| anyhow::anyhow!("invalid dataset record at line {}", i + 1))?;
            case.validate()
                .with_context(|| format!("dataset line {}", i + 1))?;
            ensure!(ids.insert(case.id.clone()), "duplicate case id");
            if cfg.target.adapter == "completions" {
                ensure!(
                    case.messages.is_empty() && case.tools.is_empty(),
                    "completions requires prompt-only cases without tools"
                );
            }
            if cfg.mode != Mode::Performance {
                ensure!(case.scorer.is_some(), "quality cases require a scorer");
            }
            cases.push(case);
        }
        ensure!(!cases.is_empty(), "dataset is empty");
        Ok((cfg, cases, digest(&bytes)))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "only config version 1 is supported");
        check_url(&self.target.base_url)?;
        ensure!(!self.target.model.trim().is_empty(), "model is required");
        ensure!(
            ["chat-completions", "completions"].contains(&self.target.adapter.as_str()),
            "unsupported adapter"
        );
        let l = &self.load;
        ensure!(
            (1..=4096).contains(&l.concurrency) && (1..=1_000_000).contains(&l.requests),
            "invalid concurrency or request budget"
        );
        ensure!(
            l.warmup_requests <= 10000
                && (1..=128).contains(&l.max_turns)
                && (1..=1024).contains(&l.max_tool_calls),
            "invalid warmup/episode budget"
        );
        ensure!(
            (1024..=64 * 1024 * 1024).contains(&l.max_response_bytes),
            "invalid response byte limit"
        );
        for v in [
            l.rate,
            l.request_timeout_seconds,
            l.episode_timeout_seconds,
            l.max_seconds,
            l.drain_seconds,
            l.max_schedule_lag_ms,
        ] {
            ensure!(
                v.is_finite() && v > 0.0 && v <= 86400.0,
                "load values must be finite, positive and <=86400"
            );
        }
        ensure!(
            self.generation.max_tokens > 0 && self.generation.max_tokens <= 1_000_000,
            "invalid output token limit"
        );
        ensure!(
            self.generation.temperature.is_finite()
                && (0.0..=2.0).contains(&self.generation.temperature),
            "invalid temperature"
        );
        for k in self.generation.extra.keys() {
            ensure!(
                [
                    "top_p",
                    "top_k",
                    "min_p",
                    "repetition_penalty",
                    "presence_penalty",
                    "frequency_penalty",
                    "stop",
                    "chat_template_kwargs",
                    "reasoning_effort",
                    "response_format",
                    "ignore_eos"
                ]
                .contains(&k.as_str()),
                "unsupported extra generation parameter: {k}"
            );
        }
        for v in [self.slo.ttft_ms, self.slo.e2e_ms].into_iter().flatten() {
            ensure!(v.is_finite() && v > 0.0, "SLO must be finite and positive");
        }
        if self.mode == Mode::Quality {
            ensure!(l.concurrency == 1 && l.kind == LoadKind::ClosedLoop, "quality baseline requires closed-loop concurrency 1; use quality_under_load otherwise");
        }
        if let Some(t) = &self.telemetry {
            check_url(&t.url)?;
            ensure!(
                !t.histograms.is_empty(),
                "telemetry histogram mapping is empty"
            );
            for name in t.histograms.values() {
                ensure!(
                    name.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                    "invalid metric name"
                );
            }
        }
        Ok(())
    }
    pub fn manifest_config(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("finite validated config");
        v["target"]["base_url"] = Value::String("redacted; identity hashed separately".into());
        v["dataset"] = Value::String("redacted; content hash recorded".into());
        if let Some(t) = v.get_mut("telemetry").and_then(Value::as_object_mut) {
            t.insert("url".into(), Value::String("redacted".into()));
        }
        v
    }
}
impl Case {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty() && !self.group.is_empty() && !self.language.is_empty(),
            "case id/group/language required"
        );
        ensure!(
            self.prompt.is_empty() != self.messages.is_empty(),
            "provide exactly one of prompt or messages"
        );
        for m in &self.messages {
            ensure!(
                m["role"]
                    .as_str()
                    .is_some_and(|r| ["system", "user", "assistant"].contains(&r))
                    && m["content"].is_string(),
                "messages require a supported role and string content"
            );
        }
        for name in &self.tools {
            ensure!(
                ["add", "kv_get", "kv_set"].contains(&name.as_str()),
                "unknown fixture tool"
            );
        }
        match &self.scorer {
            Some(Scorer::Exact { answers, .. }) => ensure!(!answers.is_empty(), "empty answer set"),
            Some(Scorer::Numeric {
                expected,
                tolerance,
            }) => ensure!(
                expected.is_finite() && tolerance.is_finite() && *tolerance >= 0.0,
                "invalid numeric oracle"
            ),
            Some(Scorer::Contains { required }) => ensure!(
                !required.is_empty() && required.iter().all(|s| !s.is_empty()),
                "empty required text"
            ),
            Some(Scorer::JsonSchema { schema, .. }) => {
                jsonschema::validator_for(schema).map_err(|_| {
                    anyhow::anyhow!("invalid/unsupported JSON schema; remote references disabled")
                })?;
            }
            Some(Scorer::Tools { calls, .. }) => {
                ensure!(!self.tools.is_empty(), "tool scorer requires tools");
                for c in calls {
                    ensure!(
                        self.tools.contains(&c.name) && c.arguments.is_object(),
                        "invalid expected tool call"
                    );
                }
            }
            _ => {}
        }
        if !self.tools.is_empty() && !matches!(self.scorer, Some(Scorer::Tools { .. })) {
            bail!("tool episodes require a tools scorer");
        }
        Ok(())
    }
    pub fn supported(&self) -> bool {
        !matches!(self.scorer, Some(Scorer::Unsupported { .. }))
    }
    pub fn initial_messages(&self) -> Vec<Value> {
        if self.messages.is_empty() {
            vec![serde_json::json!({"role":"user","content":self.prompt})]
        } else {
            self.messages.clone()
        }
    }
}
