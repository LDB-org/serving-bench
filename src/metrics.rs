use crate::{adapter::RequestRecord, config::Slo, evaluation::Score, METRIC_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    pub id: usize,
    pub case_id: String,
    pub group: String,
    pub language: String,
    pub tags: Vec<String>,
    pub scheduled: f64,
    pub dispatch: Option<f64>,
    pub terminal: f64,
    pub status: String,
    pub requests: Vec<RequestRecord>,
    pub score: Score,
    pub tool_traces: Vec<Value>,
}
pub fn distribution(mut values: Vec<f64>) -> Value {
    values.retain(|v| v.is_finite() && *v >= 0.0);
    if values.is_empty() {
        return json!({"n":0,"mean":null,"p50":null,"p95":null,"p99":null});
    }
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[((values.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    json!({"n":values.len(),"mean":values.iter().sum::<f64>()/values.len() as f64,"p50":at(0.50),"p95":at(0.95),"p99":at(0.99),"p99_low_sample":values.len()<100,"method":"nearest_rank"})
}
pub fn ttft(r: &RequestRecord) -> Option<f64> {
    r.events.first().map(|e| (e.seconds - r.dispatch) * 1000.0)
}
pub fn slo_pass(e: &Episode, slo: &Slo) -> bool {
    if e.status != "success" {
        return false;
    }
    let e2e = (e.terminal - e.scheduled) * 1000.0;
    let first = e
        .requests
        .first()
        .and_then(|r| r.events.first())
        .map(|v| (v.seconds - e.scheduled) * 1000.0);
    slo.e2e_ms.is_none_or(|v| e2e <= v) && slo.ttft_ms.is_none_or(|v| first.is_some_and(|t| t <= v))
}
pub fn summarize(
    episodes: &[Episode],
    window: f64,
    slo: &Slo,
    expected: usize,
    complete: bool,
    lag_limit: f64,
) -> Value {
    let all = aggregate(episodes, window, slo);
    let mut groups: BTreeMap<String, Vec<Episode>> = BTreeMap::new();
    for e in episodes {
        groups
            .entry(format!("{}:{}", e.group, e.language))
            .or_default()
            .push(e.clone());
    }
    let groups: BTreeMap<_, _> = groups
        .into_iter()
        .map(|(k, v)| (k, aggregate(&v, window, slo)))
        .collect();
    let late = episodes
        .iter()
        .filter(|e| {
            e.dispatch
                .is_some_and(|d| (d - e.scheduled) * 1000.0 > lag_limit)
        })
        .count();
    json!({"metric_version":METRIC_VERSION,"complete":complete,"planned_episodes":expected,"recorded_episodes":episodes.len(),"not_scheduled":expected.saturating_sub(episodes.len()),"window_seconds":window,"client_limited":late>0 || episodes.iter().any(|e| e.status=="client_rejected"),"late_episodes":late,"overall":all,"groups":groups,"limitations":["Client timestamps include network and HTTP connection waiting.","SSE chunks are not tokens; exact ITL is unavailable.","Token counts are server-reported, not independently verified.","Group throughput uses the entire cohort wall-clock window.","Quality is limited to configured scorers and selected cases."]})
}
fn aggregate(episodes: &[Episode], window: f64, slo: &Slo) -> Value {
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    for e in episodes {
        *statuses.entry(e.status.clone()).or_default() += 1;
    }
    let requests: Vec<_> = episodes.iter().flat_map(|e| &e.requests).collect();
    let successful: Vec<_> = requests
        .iter()
        .copied()
        .filter(|r| r.status == "success")
        .collect();
    let count = |predicate: fn(&Episode) -> bool| episodes.iter().filter(|e| predicate(e)).count();
    let success = count(|e| e.status == "success");
    let supported = count(|e| e.status != "unsupported");
    let scored = count(|e| e.score.passed.is_some());
    let passed = count(|e| e.score.passed == Some(true));
    let good = episodes.iter().filter(|e| slo_pass(e, slo)).count();
    let quality_good = episodes
        .iter()
        .filter(|e| slo_pass(e, slo) && e.score.passed == Some(true))
        .count();
    let rate = |n: f64| if window > 0.0 { Some(n / window) } else { None };
    let token_sum = |input: bool| -> Option<u64> {
        if successful.is_empty() {
            return None;
        }
        successful
            .iter()
            .map(|r| {
                if input {
                    r.input_tokens
                } else {
                    r.output_tokens
                }
            })
            .try_fold(0u64, |sum, value| sum.checked_add(value?))
    };
    let input = token_sum(true);
    let output = token_sum(false);
    let mut gaps = Vec::new();
    let mut tpot = Vec::new();
    for r in &successful {
        let mut times: Vec<_> = r.events.iter().map(|v| v.seconds).collect();
        times.dedup_by(|a, b| *a == *b);
        gaps.extend(times.windows(2).map(|w| (w[1] - w[0]) * 1000.0));
        if let (Some(n), Some(first), Some(last)) = (r.output_tokens, times.first(), times.last()) {
            if n > 1
                && times.len() > 1
                && r.reasoning_tokens == Some(0)
                && r.events.iter().all(|e| e.channel == "answer")
            {
                tpot.push((last - first) * 1000.0 / (n - 1) as f64);
            }
        }
    }
    let quality_complete = scored == supported && supported > 0;
    let traces: Vec<_> = episodes.iter().flat_map(|e| &e.tool_traces).collect();
    let unique_cases = episodes
        .iter()
        .map(|e| &e.case_id)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    json!({"episodes":episodes.len(),"statuses":statuses,"successful_episodes":success,"http_requests":requests.len(),"successful_http_requests":successful.len(),"success_rate": if supported>0 {Some(success as f64/supported as f64)} else {None},
        "unique_cases":unique_cases,"repeated_episodes":episodes.len().saturating_sub(unique_cases),
        "tools":{"invocations":traces.len(),"execution_errors":traces.iter().filter(|t|t["status"]=="tool_error").count(),"execution_ms":distribution(traces.iter().filter_map(|t|Some((t["terminal"].as_f64()?-t["started"].as_f64()?)*1000.0)).collect()),"first_complete_call_upper_bound_ms":distribution(episodes.iter().filter_map(|e|e.tool_traces.first().and_then(|t|t["started"].as_f64()).map(|t|(t-e.scheduled)*1000.0)).collect())},
        "episode_throughput_per_second":rate(success as f64),"request_throughput_per_second":rate(successful.len()as f64),
        "input_tokens":input,"output_tokens":output,"input_tokens_per_second":input.and_then(|n|rate(n as f64)),"output_tokens_per_second":output.and_then(|n|rate(n as f64)),"token_count_coverage":if successful.is_empty(){0.0}else{successful.iter().filter(|r|r.output_tokens.is_some()).count() as f64/successful.len()as f64},
        "ttft_ms":distribution(successful.iter().filter_map(|r|ttft(r)).collect()),
        "first_answer_ms":distribution(successful.iter().filter_map(|r|r.events.iter().find(|e|e.channel=="answer").map(|e|(e.seconds-r.dispatch)*1000.0)).collect()),
        "http_e2e_ms":distribution(successful.iter().map(|r|(r.terminal-r.dispatch)*1000.0).collect()),
        "episode_e2e_ms":distribution(episodes.iter().filter(|e|e.status=="success").map(|e|(e.terminal-e.scheduled)*1000.0).collect()),
        "failure_terminal_ms":distribution(episodes.iter().filter(|e|e.status!="success"&&e.status!="unsupported").map(|e|(e.terminal-e.scheduled)*1000.0).collect()),
        "schedule_lag_ms":distribution(episodes.iter().filter_map(|e|e.dispatch.map(|t|(t-e.scheduled)*1000.0)).collect()),
        "chunk_gap_ms":distribution(gaps),"visible_tpot_estimate_ms":distribution(tpot),"exact_itl_ms":null,
        "slo_goodput_episodes_per_second":if slo.ttft_ms.is_some()||slo.e2e_ms.is_some(){rate(good as f64)}else{None},
        "quality":{"scored":scored,"supported":supported,"passed":passed,"complete":quality_complete,"accuracy":if quality_complete{Some(passed as f64/supported as f64)}else{None},"observed_accuracy":if scored>0{Some(passed as f64/scored as f64)}else{None},"goodput_episodes_per_second":if quality_complete&&(slo.ttft_ms.is_some()||slo.e2e_ms.is_some()){rate(quality_good as f64)}else{None}}})
}
