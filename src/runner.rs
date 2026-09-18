use crate::{
    adapter::{self, RequestRecord},
    config::{digest, Case, Config, ExpectedCall, LoadKind, Mode},
    evaluation::{self, Score},
    metrics::{self, Episode},
    telemetry, METRIC_VERSION,
};
use anyhow::{anyhow, ensure, Context, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Shared = Arc<Mutex<Episode>>;
struct Evidence {
    answer: String,
    calls: Vec<ExpectedCall>,
    state: BTreeMap<String, Value>,
}
struct Completed {
    id: usize,
    evidence: Option<Evidence>,
}

pub fn plan(config: &Config, cases: &[Case]) -> Value {
    let tool_cases = cases.iter().filter(|c| !c.tools.is_empty()).count();
    json!({"version":1,"mode":config.mode,"episodes":config.load.requests,"warmup_episodes":config.load.warmup_requests,"unique_cases":cases.len(),"unsupported_cases":cases.iter().filter(|c|!c.supported()).count(),"tool_cases":tool_cases,"concurrency_unit":"episode","max_concurrent_http_requests":config.load.concurrency,"max_http_requests":(config.load.requests+config.load.warmup_requests)as u64*config.load.max_turns as u64,"max_output_tokens_requested":(config.load.requests+config.load.warmup_requests)as u64*config.load.max_turns as u64*config.generation.max_tokens as u64,"measurement_budget_seconds":config.load.max_seconds,"drain_budget_seconds":config.load.drain_seconds,"warmup_budget_seconds":config.load.warmup_requests as f64*config.load.episode_timeout_seconds,"token_lengths":"unknown; no tokenizer-based lengths claimed","network_requests_sent":false})
}
fn state(id: usize, case: &Case, scheduled: f64) -> Shared {
    Arc::new(Mutex::new(Episode {
        id,
        case_id: case.id.clone(),
        group: case.group.clone(),
        language: case.language.clone(),
        tags: case.tags.clone(),
        scheduled,
        dispatch: None,
        terminal: scheduled,
        status: "pending".into(),
        requests: vec![],
        score: Score::unavailable("pending"),
        tool_traces: vec![],
    }))
}
fn finish(shared: &Shared, status: &str, clock: Instant) {
    let mut e = shared.lock().unwrap();
    e.status = status.into();
    e.terminal = clock.elapsed().as_secs_f64();
}

async fn episode(
    client: reqwest::Client,
    config: Arc<Config>,
    case: Case,
    shared: Shared,
    clock: Instant,
) -> Completed {
    let id = shared.lock().unwrap().id;
    if !case.supported() {
        finish(&shared, "unsupported", clock);
        return Completed { id, evidence: None };
    }
    let start = Instant::now();
    shared.lock().unwrap().dispatch = Some(clock.elapsed().as_secs_f64());
    let mut messages = case.initial_messages();
    let tools = adapter::fixture_definitions(&case.tools);
    let mut state = case.state.clone();
    let mut observed = Vec::new();
    for turn in 0..config.load.max_turns {
        let remaining = config.load.episode_timeout_seconds - start.elapsed().as_secs_f64();
        if remaining <= 0.0 {
            finish(&shared, "episode_timeout", clock);
            break;
        }
        let mut request_cfg = (*config).clone();
        request_cfg.load.request_timeout_seconds =
            request_cfg.load.request_timeout_seconds.min(remaining);
        let scheduled = if turn == 0 {
            shared.lock().unwrap().scheduled
        } else {
            clock.elapsed().as_secs_f64()
        };
        let body = adapter::payload(&config, &messages, &tools);
        let pending = RequestRecord {
            id: format!("{id}:{turn}"),
            episode_id: id,
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
        };
        shared.lock().unwrap().requests.push(pending);
        let response =
            adapter::request(&client, &request_cfg, body, id, turn, scheduled, clock).await;
        *shared.lock().unwrap().requests.last_mut().unwrap() = response.record.clone();
        if response.record.status != "success" {
            finish(&shared, &response.record.status, clock);
            break;
        }
        if response.record.finish_reason.as_deref() == Some("length")
            && config.mode != Mode::Performance
        {
            finish(&shared, "truncated", clock);
            break;
        }
        if response.record.finish_reason.as_deref() == Some("content_filter") {
            finish(&shared, "content_filtered", clock);
            break;
        }
        if response.calls.is_empty() {
            if response.answer.is_empty() {
                finish(&shared, "empty_output", clock);
                break;
            }
            finish(&shared, "success", clock);
            return Completed {
                id,
                evidence: (config.mode != Mode::Performance).then_some(Evidence {
                    answer: response.answer,
                    calls: observed,
                    state,
                }),
            };
        }
        if observed.len() + response.calls.len() > config.load.max_tool_calls {
            finish(&shared, "tool_budget", clock);
            break;
        }
        let mut assistant = json!({"role":"assistant","content":response.answer,"tool_calls":response.calls.iter().map(|c|c.as_message()).collect::<Vec<_>>()});
        if !response.reasoning.is_empty() {
            assistant["reasoning_content"] = json!(response.reasoning);
        }
        messages.push(assistant);
        for call in response.calls {
            let Ok(parsed) = call.expected() else {
                finish(&shared, "invalid_tool_arguments", clock);
                return Completed { id, evidence: None };
            };
            let tool_start = clock.elapsed().as_secs_f64();
            let result = evaluation::execute(&parsed, &case.tools, &mut state);
            let tool_end = clock.elapsed().as_secs_f64();
            let mut trace = json!({"turn":turn,"id":call.id,"name":call.name,"arguments_sha256":digest(parsed.arguments.to_string().as_bytes()),"started":tool_start,"terminal":tool_end,"status":if result.get("error").is_some(){"tool_error"}else{"success"}});
            if config.save_content {
                trace["arguments"] = parsed.arguments.clone();
                trace["result"] = result.clone();
            }
            shared.lock().unwrap().tool_traces.push(trace);
            observed.push(parsed);
            messages
                .push(json!({"role":"tool","tool_call_id":call.id,"content":result.to_string()}));
        }
    }
    if shared.lock().unwrap().status == "pending" {
        finish(&shared, "turn_budget", clock);
    }
    Completed { id, evidence: None }
}

fn new_file(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    Ok(o.open(path)?)
}
fn json_write(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut f = File::create(path)?;
    serde_json::to_writer_pretty(&mut f, value)?;
    writeln!(f)?;
    Ok(())
}
fn append(file: &mut File, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

pub async fn run(
    config: Config,
    cases: Vec<Case>,
    dataset_hash: String,
    output: &Path,
) -> Result<Value> {
    config.validate()?;
    ensure!(!cases.is_empty(), "dataset empty");
    for case in &cases {
        case.validate()?;
        ensure!(
            config.mode == Mode::Performance || case.scorer.is_some(),
            "quality case requires scorer"
        );
        ensure!(
            config.target.adapter != "completions"
                || (case.messages.is_empty() && case.tools.is_empty()),
            "completions requires prompt-only cases"
        );
    }
    adapter::auth(&config.target.api_key_env)?;
    if let Some(t) = &config.telemetry {
        adapter::auth(&t.api_key_env)?;
    }
    let client = adapter::client(&config)?;
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(output).context("output must be a new directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    let config = Arc::new(config);
    let fingerprint = json!({"dataset_sha256":dataset_hash,"model":config.target.model,"adapter":config.target.adapter,"metadata":config.target.metadata,"generation":config.generation,"load":config.load,"mode":config.mode,"slo":config.slo});
    let mut manifest = json!({"artifact_version":1,"metric_version":METRIC_VERSION,"tool_version":env!("CARGO_PKG_VERSION"),"started_unix_seconds":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),"status":"running","config":config.manifest_config(),"dataset_sha256":dataset_hash,"comparison_identity":fingerprint,"target_url_sha256":digest(config.target.base_url.as_bytes()),"client":{"os":std::env::consts::OS,"arch":std::env::consts::ARCH},"score_replay_available":false,"evidence_note":"Dataset required for exact replay; stored scores support offline aggregation, not independent regrading."});
    manifest["executable_sha256"] = json!(std::env::current_exe()
        .ok()
        .and_then(|p| fs::read(p).ok())
        .map(|bytes| digest(&bytes)));
    manifest["dataset_cases"] = json!(cases.len());
    json_write(&output.join("manifest.json"), &manifest)?;
    let mut journal = new_file(&output.join("episodes.jsonl"))?;
    let mut warmup = new_file(&output.join("warmup.jsonl"))?;
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    for i in 0..config.load.warmup_requests {
        let case = Case {
            id: format!("warmup-{i}"),
            group: "warmup".into(),
            language: "en".into(),
            prompt: format!("Warmup {}-{i}: reply OK.", config.generation.seed),
            messages: vec![],
            tags: vec![],
            scorer: None,
            tools: vec![],
            state: BTreeMap::new(),
        };
        let clock = Instant::now();
        let shared = state(i, &case, 0.0);
        tokio::select! {
            _= &mut interrupt => {manifest["status"]=json!("interrupted_during_warmup");json_write(&output.join("manifest.json"),&manifest)?;return Err(anyhow!("interrupted during warmup; no measured results"));}
            _=episode(client.clone(),config.clone(),case,shared.clone(),clock)=>{}
        }
        append(&mut warmup, &*shared.lock().unwrap())?;
    }
    let before = if let Some(t) = &config.telemetry {
        Some(telemetry::snapshot(&client, t).await)
    } else {
        None
    };
    let clock = Instant::now();
    let mut offsets = Vec::with_capacity(config.load.requests);
    let mut rng = StdRng::seed_from_u64(config.generation.seed);
    let mut next = 0.0;
    for _ in 0..config.load.requests {
        offsets.push(next);
        next += if config.load.poisson {
            -(1.0 - rng.gen::<f64>()).ln() / config.load.rate
        } else {
            1.0 / config.load.rate
        };
    }
    let mut active = FuturesUnordered::new();
    let mut states: Vec<Shared> = Vec::new();
    let mut completed: BTreeMap<usize, Option<Evidence>> = BTreeMap::new();
    let mut index = 0;
    let mut interrupted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(config.load.max_seconds);
    let mut stop_time = 0.0;
    while index < config.load.requests {
        if clock.elapsed().as_secs_f64() >= config.load.max_seconds {
            break;
        }
        let ready = if config.load.kind == LoadKind::ClosedLoop {
            active.len() < config.load.concurrency
        } else {
            clock.elapsed().as_secs_f64() >= offsets[index]
        };
        if ready {
            let scheduled = if config.load.kind == LoadKind::ClosedLoop {
                clock.elapsed().as_secs_f64()
            } else {
                offsets[index]
            };
            let case = cases[index % cases.len()].clone();
            let shared = state(index, &case, scheduled);
            states.push(shared.clone());
            if !case.supported() {
                finish(&shared, "unsupported", clock);
                append(&mut journal, &*shared.lock().unwrap())?;
                completed.insert(index, None);
            } else if active.len() >= config.load.concurrency {
                finish(&shared, "client_rejected", clock);
                append(&mut journal, &*shared.lock().unwrap())?;
                completed.insert(index, None);
            } else {
                active.push(episode(client.clone(), config.clone(), case, shared, clock));
            }
            index += 1;
            stop_time = clock.elapsed().as_secs_f64();
            // Poll futures between arrivals, including when an arrival schedule is behind.
            tokio::task::yield_now().await;
        }
        let arrival = if index < config.load.requests && config.load.kind == LoadKind::OpenLoop {
            tokio::time::Instant::from_std(clock + Duration::from_secs_f64(offsets[index]))
        } else {
            deadline
        };
        if config.load.kind == LoadKind::ClosedLoop
            && active.len() < config.load.concurrency
            && index < config.load.requests
        {
            continue;
        }
        if index >= config.load.requests {
            break;
        }
        tokio::select! {
            biased;
            _= &mut interrupt=>{interrupted=true;break;}
            item=active.next(),if !active.is_empty()=>{if let Some(c)=item {append(&mut journal,&*states[c.id].lock().unwrap())?;completed.insert(c.id,c.evidence);}}
            _=tokio::time::sleep_until(deadline)=>{break;}
            _=tokio::time::sleep_until(arrival),if config.load.kind==LoadKind::OpenLoop=>{}
        }
    }
    if index < config.load.requests {
        stop_time = clock.elapsed().as_secs_f64();
    }
    let drain_deadline =
        tokio::time::Instant::now() + Duration::from_secs_f64(config.load.drain_seconds);
    while !active.is_empty() && !interrupted {
        tokio::select! {
            _=&mut interrupt=>{interrupted=true;}
            item=active.next()=>{if let Some(c)=item {append(&mut journal,&*states[c.id].lock().unwrap())?;completed.insert(c.id,c.evidence);}}
            _=tokio::time::sleep_until(drain_deadline)=>{break;}
        }
    }
    drop(active);
    for shared in &states {
        let mut e = shared.lock().unwrap();
        if e.status == "pending" {
            e.status = if interrupted {
                "cancelled"
            } else {
                "drain_timeout"
            }
            .into();
            e.terminal = clock.elapsed().as_secs_f64();
            for r in &mut e.requests {
                if r.status == "pending" {
                    r.status = "cancelled".into();
                    r.terminal = clock.elapsed().as_secs_f64();
                }
            }
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = completed.entry(e.id) {
            append(&mut journal, &*e)?;
            entry.insert(None);
        }
    }
    let window_start = states
        .first()
        .map(|s| s.lock().unwrap().scheduled)
        .unwrap_or(0.0);
    let window = states
        .iter()
        .map(|s| s.lock().unwrap().terminal)
        .fold(stop_time, f64::max)
        - window_start;
    let after = if let Some(t) = &config.telemetry {
        Some(telemetry::snapshot(&client, t).await)
    } else {
        None
    };
    let scoring_start = Instant::now();
    let mut scores = new_file(&output.join("scores.jsonl"))?;
    let mut episodes = Vec::new();
    for shared in &states {
        let mut e = shared.lock().unwrap().clone();
        e.score = if e.status == "unsupported" {
            Score::unavailable("unsupported")
        } else if config.mode == Mode::Performance {
            Score::unavailable("not_scored")
        } else if e.status != "success" {
            Score::failure(&e.status)
        } else if let Some(Some(ev)) = completed.get(&e.id) {
            evaluation::score(&cases[e.id % cases.len()], &ev.answer, &ev.calls, &ev.state)
        } else {
            Score::unavailable("scorer_error")
        };
        append(&mut scores, &json!({"episode_id":e.id,"score":e.score}))?;
        episodes.push(e);
    }
    let complete = index == config.load.requests
        && !interrupted
        && !episodes
            .iter()
            .any(|e| ["cancelled", "drain_timeout"].contains(&e.status.as_str()));
    manifest["status"] = json!(if complete { "complete" } else { "incomplete" });
    manifest["window_seconds"] = json!(window);
    manifest["window_start_seconds"] = json!(window_start);
    manifest["scoring_seconds"] = json!(scoring_start.elapsed().as_secs_f64());
    manifest["episodes_sha256"] = json!(digest(&fs::read(output.join("episodes.jsonl"))?));
    manifest["scores_sha256"] = json!(digest(&fs::read(output.join("scores.jsonl"))?));
    if let (Some(t), Some(b), Some(a)) = (&config.telemetry, &before, &after) {
        json_write(
            &output.join("telemetry.json"),
            &json!({"before":b,"after":a,"summary":telemetry::summarize(b,a,t)}),
        )?;
    }
    json_write(&output.join("manifest.json"), &manifest)?;
    let summary = metrics::summarize(
        &episodes,
        window,
        &config.slo,
        config.load.requests,
        complete,
        config.load.max_schedule_lag_ms,
    );
    write_reports(output, &summary, &episodes)?;
    Ok(summary)
}

pub fn report(output: &Path) -> Result<Value> {
    let manifest: Value = crate::config::read_json(&output.join("manifest.json"))?;
    ensure!(
        manifest["metric_version"] == METRIC_VERSION,
        "unsupported metric version"
    );
    for name in ["episodes", "scores"] {
        if let Some(hash) = manifest[format!("{name}_sha256")].as_str() {
            ensure!(
                digest(&fs::read(output.join(format!("{name}.jsonl")))?) == hash,
                "artifact integrity mismatch: {name}"
            );
        }
    }
    let mut episodes = Vec::new();
    for line in BufReader::new(File::open(output.join("episodes.jsonl"))?).lines() {
        episodes.push(serde_json::from_str::<Episode>(&line?)?);
    }
    let mut score_map = BTreeMap::new();
    if let Ok(file) = File::open(output.join("scores.jsonl")) {
        for line in BufReader::new(file).lines() {
            let v: Value = serde_json::from_str(&line?)?;
            let id = v["episode_id"].as_u64().context("invalid score identity")? as usize;
            ensure!(
                score_map
                    .insert(id, serde_json::from_value::<Score>(v["score"].clone())?)
                    .is_none(),
                "duplicate score identity"
            );
        }
    }
    let mut ids = std::collections::HashSet::new();
    for e in &mut episodes {
        ensure!(ids.insert(e.id), "duplicate episode identity");
        if let Some(score) = score_map.remove(&e.id) {
            e.score = score;
        }
    }
    ensure!(score_map.is_empty(), "orphan score record");
    episodes.sort_by_key(|e| e.id);
    let complete = manifest["status"] == "complete";
    let config = &manifest["config"];
    let window = manifest["window_seconds"]
        .as_f64()
        .unwrap_or_else(|| episodes.iter().map(|e| e.terminal).fold(0.0, f64::max));
    let summary = metrics::summarize(
        &episodes,
        window,
        &serde_json::from_value(config["slo"].clone())?,
        config["load"]["requests"]
            .as_u64()
            .context("missing request count")? as usize,
        complete,
        config["load"]["max_schedule_lag_ms"]
            .as_f64()
            .context("missing lag threshold")?,
    );
    write_reports(output, &summary, &episodes)?;
    Ok(summary)
}
fn write_reports(output: &Path, summary: &Value, episodes: &[Episode]) -> Result<()> {
    json_write(&output.join("summary.json"), summary)?;
    let mut requests = File::create(output.join("requests.jsonl"))?;
    let mut events = File::create(output.join("events.jsonl"))?;
    let mut traces = File::create(output.join("tool-traces.jsonl"))?;
    for e in episodes {
        for r in &e.requests {
            append(&mut requests, r)?;
            for event in &r.events {
                append(&mut events, &json!({"request_id":r.id,"event":event}))?;
            }
        }
        for trace in &e.tool_traces {
            append(&mut traces, &json!({"episode_id":e.id,"trace":trace}))?;
        }
    }
    let mut md=String::from("# Serving benchmark report\n\nClient observations; engine phase timings, when collected, are in telemetry.json.\n\n");
    md.push_str(&format!(
        "Complete: {}. Recorded episodes: {}. Window: {} seconds. Client limited: {}.\n\n",
        summary["complete"],
        summary["recorded_episodes"],
        summary["window_seconds"],
        summary["client_limited"]
    ));
    md.push_str("| Group | Episodes | Success rate | Output tok/s | TTFT P95 ms | Quality accuracy |\n| --- | ---: | ---: | ---: | ---: | ---: |\n");
    let mut csv = String::from(
        "group,episodes,success_rate,output_tokens_per_second,ttft_p95_ms,quality_accuracy\n",
    );
    if let Some(groups) = summary["groups"].as_object() {
        for (name, g) in groups {
            let safe = name.replace('|', "\\|").replace(['\n', '\r'], " ");
            md.push_str(&format!(
                "| {safe} | {} | {} | {} | {} | {} |\n",
                g["episodes"],
                g["success_rate"],
                g["output_tokens_per_second"],
                g["ttft_ms"]["p95"],
                g["quality"]["accuracy"]
            ));
            csv.push_str(&format!(
                "\"{}\",{},{},{},{},{}\n",
                name.replace('"', "\"\""),
                g["episodes"],
                g["success_rate"],
                g["output_tokens_per_second"],
                g["ttft_ms"]["p95"],
                g["quality"]["accuracy"]
            ));
        }
    }
    md.push_str("\n`null` means unavailable, not zero. Chunk intervals are not token intervals. Required-substring and schema checks do not establish general answer quality. See summary.json for coverage, errors, and all distributions.\n");
    fs::write(output.join("report.md"), md)?;
    fs::write(output.join("summary.csv"), csv)?;
    Ok(())
}

pub fn compare(left: &Path, right: &Path) -> Result<Value> {
    let a: Value = crate::config::read_json(&left.join("manifest.json"))?;
    let b: Value = crate::config::read_json(&right.join("manifest.json"))?;
    let sa = report(left)?;
    let sb = report(right)?;
    let mut differences = Vec::new();
    if a["metric_version"] != b["metric_version"] {
        differences.push("metric_version".to_string());
    }
    if let Some(identity) = a["comparison_identity"].as_object() {
        for (key, v) in identity {
            if b["comparison_identity"][key] != *v {
                differences.push(key.clone());
            }
        }
    } else {
        differences.push("missing_identity".into());
    }
    if a["status"] != "complete" || b["status"] != "complete" {
        differences.push("incomplete_run".into());
    }
    if sa["client_limited"] == true || sb["client_limited"] == true {
        differences.push("client_limited".into());
    }
    for key in [
        "engine",
        "engine_version",
        "hardware",
        "tokenizer",
        "chat_template",
        "model_revision",
        "quantization",
    ] {
        if [&a, &b].iter().any(|m| {
            m["comparison_identity"]["metadata"][key]
                .as_str()
                .is_none_or(|s| s.is_empty() || s == "unknown")
        }) {
            differences.push(format!("missing_metadata:{key}"));
        }
    }
    if [&a, &b]
        .iter()
        .any(|m| m["comparison_identity"]["generation"]["cache_policy"] == "unknown")
    {
        differences.push("unknown_cache_policy".into());
    }
    Ok(
        json!({"comparable":differences.is_empty(),"differences":differences,"left":sa,"right":sb,"conclusion":"Descriptive comparison only; no automatic causal winner or significance claim."}),
    )
}
