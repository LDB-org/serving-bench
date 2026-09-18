use serde_json::{json, Value};
use serving_bench::{
    adapter,
    config::{Case, Config},
    runner,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

struct Reply {
    status: u16,
    body: String,
    delay_ms: u64,
    fragment: usize,
}
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(handler: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let records = requests.clone();
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let records = records.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut block = [0u8; 1024];
                let mut expected = None;
                loop {
                    let n = socket.read(&mut block).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&block[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&buf[..pos]);
                        let len = header
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        expected = Some((pos + 4, len));
                    }
                    if let Some((start, len)) = expected {
                        if buf.len() >= start + len {
                            break;
                        }
                    }
                    assert!(buf.len() < 1024 * 1024);
                }
                let (start, len) = expected.unwrap();
                let payload: Value = serde_json::from_slice(&buf[start..start + len]).unwrap();
                records.lock().unwrap().push(payload.clone());
                let reply = handler(&payload);
                tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
                let header=format!("HTTP/1.1 {} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.status,reply.body.len());
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                for chunk in reply.body.as_bytes().chunks(reply.fragment.max(1)) {
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            });
        }
    });
    Server {
        url,
        requests,
        task,
    }
}
fn completion(answer: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":answer},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"completion_tokens_details":{"reasoning_tokens":0}}})
    )
}
fn config(url: &str) -> Config {
    serde_json::from_value(json!({"version":1,"target":{"base_url":url,"model":"fixture"},"dataset":"unused","mode":"quality_under_load","load":{"concurrency":2,"requests":4,"warmup_requests":0,"max_seconds":3,"drain_seconds":1,"max_schedule_lag_ms":1000}})).unwrap()
}
fn case() -> Case {
    serde_json::from_value(json!({"id":"test","group":"qa","language":"zh","prompt":"private prompt","scorer":{"kind":"exact","answers":["你好"]}})).unwrap()
}
#[tokio::test]
async fn fragmented_utf8_stream_and_offline_report_round_trip() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 0,
        fragment: 1,
    })
    .await;
    let cfg = config(&server.url);
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join("run");
    let summary = runner::run(cfg, vec![case()], "dataset-id".into(), &out)
        .await
        .unwrap();
    assert_eq!(summary["overall"]["quality"]["accuracy"], 1.0);
    assert_eq!(summary["recorded_episodes"], 4);
    assert_eq!(summary, runner::report(&out).unwrap());
    let journal = std::fs::read_to_string(out.join("episodes.jsonl")).unwrap();
    assert!(!journal.contains("private prompt"));
    assert!(!journal.contains("你好"));
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    std::fs::write(out.join("episodes.jsonl"), "{}\n").unwrap();
    assert!(runner::report(&out).is_err());
}
#[tokio::test]
async fn missing_done_is_failure_not_success() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好").replace("data: [DONE]\n\n", ""),
        delay_ms: 0,
        fragment: 1000,
    })
    .await;
    let cfg = config(&server.url);
    let client = adapter::client(&cfg).unwrap();
    let r = adapter::request(
        &client,
        &cfg,
        adapter::payload(&cfg, &case().initial_messages(), &[]),
        0,
        0,
        0.0,
        Instant::now(),
    )
    .await;
    assert_eq!(r.record.status, "incomplete_stream");
}
#[tokio::test]
async fn rate_limit_is_not_retried() {
    let server = server(|_| Reply {
        status: 429,
        body: "private error".into(),
        delay_ms: 0,
        fragment: 100,
    })
    .await;
    let cfg = config(&server.url);
    let client = adapter::client(&cfg).unwrap();
    let r = adapter::request(&client, &cfg, json!({}), 0, 0, 0.0, Instant::now()).await;
    assert_eq!(r.record.status, "rate_limited");
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn open_loop_rejects_overload_without_serializing_arrivals() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 150,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.kind = serving_bench::config::LoadKind::OpenLoop;
    cfg.load.concurrency = 1;
    cfg.load.rate = 200.0;
    cfg.load.poisson = false;
    cfg.load.requests = 6;
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["overall"]["statuses"]["client_rejected"], 5);
    assert_eq!(summary["overall"]["successful_episodes"], 1);
    assert_eq!(summary["overall"]["quality"]["scored"], 6);
    assert!(summary["window_seconds"].as_f64().unwrap() >= 0.15);
}
#[tokio::test]
async fn drain_timeout_preserves_in_flight_terminal_record() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 500,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.requests = 1;
    cfg.load.drain_seconds = 0.03;
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["overall"]["statuses"]["drain_timeout"], 1);
    assert_eq!(summary["overall"]["http_requests"], 1);
}
#[tokio::test]
async fn multi_turn_tools_execute_and_verify_final_answer() {
    let server=server(|body| {
        let messages=body["messages"].as_array().unwrap();
        let body=if messages.iter().any(|m|m["role"]=="tool") {completion("5")}
        else {format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"add","arguments":"{\"a\":2,\"b\":3}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":8,"completion_tokens":10}}))};
        Reply{status:200,body,delay_ms:0,fragment:7}
    }).await;
    let mut cfg = config(&server.url);
    cfg.load.requests = 1;
    let case:Case=serde_json::from_value(json!({"id":"tools","group":"tools","language":"en","prompt":"Use add to add 2 and 3; answer only the number","tools":["add"],"scorer":{"kind":"tools","calls":[{"name":"add","arguments":{"a":2,"b":3}}],"final_answers":["5"]}})).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["overall"]["quality"]["accuracy"], 1.0);
    assert_eq!(summary["overall"]["http_requests"], 2);
    assert_eq!(
        server.requests.lock().unwrap()[1]["messages"][2]["role"],
        "tool"
    );
}
#[tokio::test]
async fn overall_budget_records_unscheduled_work_as_incomplete() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 100,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.concurrency = 1;
    cfg.load.requests = 10;
    cfg.load.max_seconds = 0.03;
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["not_scheduled"], 9);
    assert_eq!(summary["overall"]["successful_episodes"], 1);
}
#[tokio::test]
async fn non_streaming_completions_preserve_usage_without_ttft() {
    let server=server(|body|{assert!(body["prompt"].is_string());Reply{status:200,body:json!({"choices":[{"text":"你好","finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2}}).to_string(),delay_ms:0,fragment:100}}).await;
    let mut cfg = config(&server.url);
    cfg.target.adapter = "completions".into();
    cfg.generation.stream = false;
    cfg.load.requests = 1;
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["overall"]["ttft_ms"]["n"], 0);
    assert_eq!(summary["overall"]["output_tokens"], 2);
}
#[tokio::test]
async fn output_directories_are_never_overwritten() {
    let cfg = config("http://127.0.0.1:1/v1");
    let temp = tempfile::tempdir().unwrap();
    assert!(runner::run(cfg, vec![case()], "x".into(), temp.path())
        .await
        .is_err());
}

#[tokio::test]
async fn length_capped_performance_is_valid_but_quality_is_truncated() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好").replace("stop", "length"),
        delay_ms: 0,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.requests = 1;
    cfg.mode = serving_bench::config::Mode::Performance;
    let temp = tempfile::tempdir().unwrap();
    let performance = runner::run(
        cfg.clone(),
        vec![case()],
        "x".into(),
        &temp.path().join("perf"),
    )
    .await
    .unwrap();
    assert_eq!(performance["overall"]["statuses"]["success"], 1);
    cfg.mode = serving_bench::config::Mode::Quality;
    cfg.load.concurrency = 1;
    let quality = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("quality"))
        .await
        .unwrap();
    assert_eq!(quality["overall"]["statuses"]["truncated"], 1);
    assert_eq!(quality["overall"]["quality"]["accuracy"], 0.0);
}
#[tokio::test]
async fn warmup_uses_distinct_prompts_and_is_excluded_from_counts() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 0,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.requests = 1;
    cfg.load.warmup_requests = 1;
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["overall"]["http_requests"], 1);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0]["messages"], requests[1]["messages"]);
}
#[tokio::test]
async fn request_timeout_is_an_explicit_failure() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 200,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.requests = 1;
    cfg.load.request_timeout_seconds = 0.02;
    let temp = tempfile::tempdir().unwrap();
    let summary = runner::run(cfg, vec![case()], "x".into(), &temp.path().join("run"))
        .await
        .unwrap();
    assert_eq!(summary["overall"]["statuses"]["timeout"], 1);
    assert_eq!(summary["overall"]["quality"]["accuracy"], 0.0);
}
#[tokio::test]
async fn comparison_refuses_unknown_environment_even_for_identical_runs() {
    let server = server(|_| Reply {
        status: 200,
        body: completion("你好"),
        delay_ms: 0,
        fragment: 1000,
    })
    .await;
    let mut cfg = config(&server.url);
    cfg.load.requests = 1;
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join("run");
    runner::run(cfg, vec![case()], "x".into(), &out)
        .await
        .unwrap();
    let comparison = runner::compare(&out, &out).unwrap();
    assert_eq!(comparison["comparable"], false);
    assert!(comparison["differences"]
        .as_array()
        .unwrap()
        .contains(&json!("missing_metadata:hardware")));
}
