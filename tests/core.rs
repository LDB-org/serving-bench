use serde_json::json;
use serving_bench::{
    adapter::{parse, ContentEvent, RequestRecord, Response},
    config::{Case, Config, ExpectedCall, Scorer, Slo, Telemetry},
    evaluation,
    metrics::{self, Episode},
    telemetry,
};
use std::collections::BTreeMap;

fn record() -> RequestRecord {
    RequestRecord {
        id: "0:0".into(),
        episode_id: 0,
        scheduled: 0.0,
        dispatch: 0.1,
        terminal: 2.0,
        status: "success".into(),
        http_status: Some(200),
        finish_reason: Some("stop".into()),
        stream: true,
        events: vec![
            ContentEvent {
                seconds: 0.5,
                channel: "answer".into(),
            },
            ContentEvent {
                seconds: 1.5,
                channel: "answer".into(),
            },
        ],
        input_tokens: Some(10),
        output_tokens: Some(3),
        reasoning_tokens: Some(0),
        token_source: "server_reported".into(),
        answer: None,
    }
}
fn response() -> Response {
    let mut r = record();
    r.events.clear();
    r.finish_reason = None;
    Response {
        record: r,
        answer: String::new(),
        reasoning: String::new(),
        calls: vec![],
    }
}
fn case(scorer: Scorer) -> Case {
    Case {
        id: "a".into(),
        group: "qa".into(),
        language: "zh".into(),
        prompt: "q".into(),
        messages: vec![],
        tags: vec![],
        scorer: Some(scorer),
        tools: vec![],
        state: BTreeMap::new(),
    }
}
fn episode(id: usize, status: &str) -> Episode {
    Episode {
        id,
        case_id: "a".into(),
        group: "qa".into(),
        language: "zh".into(),
        tags: vec![],
        scheduled: 0.0,
        dispatch: Some(0.1),
        terminal: 2.0,
        status: status.into(),
        requests: vec![record()],
        score: evaluation::Score::failure("incorrect"),
        tool_traces: vec![],
    }
}
#[test]
fn role_and_usage_frames_do_not_change_first_or_last_content() {
    let mut r = response();
    parse(
        &json!({"choices":[{"delta":{"role":"assistant","content":""}}]}),
        true,
        0.1,
        &mut r,
    )
    .unwrap();
    assert!(r.record.events.is_empty());
    parse(
        &json!({"choices":[{"delta":{"content":"你好世界"}}]}),
        true,
        0.5,
        &mut r,
    )
    .unwrap();
    parse(
        &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        true,
        1.0,
        &mut r,
    )
    .unwrap();
    parse(
        &json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":4}}),
        true,
        5.0,
        &mut r,
    )
    .unwrap();
    assert_eq!(r.record.events.len(), 1);
    assert_eq!(r.record.events[0].seconds, 0.5);
    assert_eq!(r.record.output_tokens, Some(4));
}
#[test]
fn interleaved_tool_fragments_are_assembled_by_index() {
    let mut r = response();
    parse(&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"add","arguments":"{\"a\":"}},{"index":0,"id":"a","function":{"name":"kv_get","arguments":"{\"key\":\"x\"}"}}]}}]}),true,0.2,&mut r).unwrap();
    parse(&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2,\"b\":3}"}}]},"finish_reason":"tool_calls"}]}),true,0.3,&mut r).unwrap();
    assert_eq!(r.calls[0].name, "kv_get");
    assert_eq!(
        r.calls[1].expected().unwrap().arguments,
        json!({"a":2,"b":3})
    );
    assert!(parse(
        &json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"changed"}]}}]}),
        true,
        0.4,
        &mut r
    )
    .is_err());
}
#[test]
fn non_streaming_does_not_invent_token_timing() {
    let mut r = response();
    parse(
        &json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}),
        false,
        2.0,
        &mut r,
    )
    .unwrap();
    assert!(r.record.events.is_empty());
    assert_eq!(r.answer, "ok");
}
#[test]
fn hand_calculated_metrics_keep_failures_and_full_window() {
    let mut good = episode(0, "success");
    good.score.passed = Some(true);
    let mut bad = episode(1, "client_rejected");
    bad.requests.clear();
    bad.dispatch = None;
    let s = metrics::summarize(
        &[good, bad],
        4.0,
        &Slo {
            ttft_ms: Some(1000.0),
            e2e_ms: Some(3000.0),
        },
        2,
        true,
        50.0,
    );
    let a = &s["overall"];
    assert_eq!(a["episode_throughput_per_second"], 0.25);
    assert_eq!(a["output_tokens_per_second"], 0.75);
    assert_eq!(a["ttft_ms"]["p50"], 400.0);
    assert_eq!(a["visible_tpot_estimate_ms"]["p50"], 500.0);
    assert_eq!(a["quality"]["accuracy"], 0.5);
    assert_eq!(a["quality"]["goodput_episodes_per_second"], 0.25);
    assert_eq!(s["client_limited"], true);
}
#[test]
fn missing_usage_invalidates_full_token_throughput() {
    let a = episode(0, "success");
    let mut b = episode(1, "success");
    b.requests[0].output_tokens = None;
    let s = metrics::summarize(&[a, b], 4.0, &Slo::default(), 2, true, 50.0);
    assert!(s["overall"]["output_tokens_per_second"].is_null());
    assert_eq!(s["overall"]["token_count_coverage"], 0.5);
}
#[test]
fn hidden_reasoning_disables_visible_tpot() {
    let mut a = episode(0, "success");
    a.requests[0].reasoning_tokens = None;
    let s = metrics::summarize(&[a], 2.0, &Slo::default(), 1, true, 50.0);
    assert_eq!(s["overall"]["visible_tpot_estimate_ms"]["n"], 0);
}
#[test]
fn nearest_rank_is_not_an_average_of_percentiles() {
    let d = metrics::distribution(vec![1.0, 2.0, 3.0, 100.0]);
    assert_eq!(d["p50"], 2.0);
    assert_eq!(d["p95"], 100.0);
    assert_eq!(d["mean"], 26.5);
}
#[test]
fn schema_success_does_not_hide_wrong_values() {
    let c = case(Scorer::JsonSchema {
        schema: json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}),
        expected: Some(json!({"n":3})),
    });
    let s = evaluation::score(&c, "{\"n\":4}", &[], &BTreeMap::new());
    assert_eq!(s.passed, Some(false));
    assert_eq!(s.details["schema_valid"], true);
}
#[test]
fn numeric_answers_and_exact_answers_have_explicit_semantics() {
    let c = case(Scorer::Numeric {
        expected: 3.0,
        tolerance: 0.01,
    });
    assert_eq!(
        evaluation::score(&c, "3.005", &[], &BTreeMap::new()).passed,
        Some(true)
    );
    assert_eq!(
        evaluation::score(&c, "nan", &[], &BTreeMap::new()).passed,
        Some(false)
    );
    assert_eq!(
        evaluation::score(&c, "answer 3", &[], &BTreeMap::new()).passed,
        Some(false)
    );
}
#[test]
fn tools_check_arguments_sequence_and_final_state() {
    let calls = vec![
        ExpectedCall {
            name: "kv_set".into(),
            arguments: json!({"key":"x","value":4}),
        },
        ExpectedCall {
            name: "kv_get".into(),
            arguments: json!({"key":"x"}),
        },
    ];
    let mut c = case(Scorer::Tools {
        calls: calls.clone(),
        unordered: false,
        final_answers: vec!["4".into()],
        final_state: BTreeMap::from([("x".into(), json!(4))]),
    });
    c.tools = vec!["kv_get".into(), "kv_set".into()];
    let mut state = BTreeMap::new();
    for call in &calls {
        assert!(evaluation::execute(call, &c.tools, &mut state)
            .get("error")
            .is_none());
    }
    assert_eq!(
        evaluation::score(&c, "4", &calls, &state).passed,
        Some(true)
    );
    let reversed = calls.iter().cloned().rev().collect::<Vec<_>>();
    assert_eq!(
        evaluation::score(&c, "4", &reversed, &state).passed,
        Some(false)
    );
    assert_eq!(
        evaluation::execute(
            &ExpectedCall {
                name: "kv_set".into(),
                arguments: json!({"key":"x","oops":1})
            },
            &c.tools,
            &mut state
        )["error"],
        "invalid_arguments"
    );
}
#[test]
fn tool_names_are_not_executable_commands() {
    let mut state = BTreeMap::new();
    let v = evaluation::execute(
        &ExpectedCall {
            name: "rm -rf /".into(),
            arguments: json!({}),
        },
        &["kv_get".into()],
        &mut state,
    );
    assert_eq!(v["error"], "unknown_tool");
}
#[test]
fn strict_config_rejects_unknown_fields_and_overrides() {
    let value = json!({"version":1,"target":{"base_url":"http://localhost:8000/v1","model":"x"},"dataset":"x"});
    let mut cfg: Config = serde_json::from_value(value.clone()).unwrap();
    cfg.validate().unwrap();
    cfg.generation.extra.insert("messages".into(), json!([]));
    assert!(cfg.validate().is_err());
    let mut wrong = value;
    wrong["concurency"] = json!(2);
    assert!(serde_json::from_value::<Config>(wrong).is_err());
}
#[test]
fn credentials_in_urls_are_rejected() {
    let cfg:Config=serde_json::from_value(json!({"version":1,"target":{"base_url":"http://user:password@localhost/v1","model":"x"},"dataset":"x"})).unwrap();
    assert!(cfg.validate().is_err());
}
#[test]
fn telemetry_is_label_scoped_and_detects_series_resets() {
    let cfg = Telemetry {
        url: "http://localhost/metrics".into(),
        api_key_env: None,
        histograms: BTreeMap::from([("prefill".into(), "vllm:prefill".into())]),
        labels: BTreeMap::from([("model".into(), "test model".into())]),
        exclusive: false,
    };
    let before="vllm:prefill_sum{model=\"test model\",worker=\"a\"} 4\nvllm:prefill_count{model=\"test model\",worker=\"a\"} 2\nvllm:prefill_sum{model=\"other\"} 900\n";
    let after = before.replace("} 4", "} 10").replace("} 2", "} 4");
    let b = telemetry::Snapshot {
        status: "success".into(),
        samples: telemetry::parse(before, &cfg).unwrap(),
    };
    let a = telemetry::Snapshot {
        status: "success".into(),
        samples: telemetry::parse(&after, &cfg).unwrap(),
    };
    assert_eq!(
        telemetry::summarize(&b, &a, &cfg)["phases"]["prefill"]["mean_seconds"],
        3.0
    );
    let reset = telemetry::Snapshot {
        status: "success".into(),
        samples: telemetry::parse(&before.replace("} 4", "} 1"), &cfg).unwrap(),
    };
    assert_eq!(
        telemetry::summarize(&b, &reset, &cfg)["phases"]["prefill"]["status"],
        "not_available"
    );
}
#[test]
fn unsupported_scorers_are_not_counted_as_passes() {
    let c = case(Scorer::Unsupported {
        reason: "no sandbox".into(),
    });
    assert!(!c.supported());
    assert_eq!(
        evaluation::score(&c, "x", &[], &BTreeMap::new()).passed,
        None
    );
}
#[test]
fn external_schema_references_are_not_fetched() {
    let c = case(Scorer::JsonSchema {
        schema: json!({"$ref":"https://example.invalid/schema"}),
        expected: None,
    });
    assert!(c.validate().is_err());
}

#[test]
fn persisted_measurement_window_preserves_float_bits() {
    let window = 0.041655041999999996_f64;
    let encoded = serde_json::to_vec(&json!({"window_seconds": window})).unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    let restored = decoded["window_seconds"].as_f64().unwrap();
    assert_eq!(window.to_bits(), restored.to_bits());
    assert_eq!((4.0 / window).to_bits(), (4.0 / restored).to_bits());
}
