use crate::{
    adapter::fixture_definitions,
    config::{Case, ExpectedCall, Scorer},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Score {
    pub status: String,
    pub passed: Option<bool>,
    pub details: Value,
}
impl Score {
    pub fn unavailable(status: &str) -> Self {
        Self {
            status: status.into(),
            passed: None,
            details: Value::Null,
        }
    }
    pub fn failure(reason: &str) -> Self {
        Self {
            status: "failed".into(),
            passed: Some(false),
            details: json!({"reason":reason}),
        }
    }
}
fn normalize(s: &str, sensitive: bool) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if sensitive {
        s
    } else {
        s.to_lowercase()
    }
}
fn answer_matches(answer: &str, expected: &[String]) -> bool {
    expected
        .iter()
        .any(|s| normalize(s, false) == normalize(answer, false))
}
pub fn score(
    case: &Case,
    answer: &str,
    observed: &[ExpectedCall],
    state: &BTreeMap<String, Value>,
) -> Score {
    let Some(scorer) = &case.scorer else {
        return Score::unavailable("not_scored");
    };
    let (passed, details) = match scorer {
        Scorer::Exact {
            answers,
            case_sensitive,
        } => (
            answers
                .iter()
                .any(|s| normalize(s, *case_sensitive) == normalize(answer, *case_sensitive)),
            Value::Null,
        ),
        Scorer::Numeric {
            expected,
            tolerance,
        } => (
            answer
                .trim()
                .parse::<f64>()
                .is_ok_and(|v| v.is_finite() && (v - expected).abs() <= *tolerance),
            Value::Null,
        ),
        Scorer::Contains { required } => {
            let hits = required
                .iter()
                .filter(|s| answer.contains(s.as_str()))
                .count();
            (
                hits == required.len(),
                json!({"matched":hits,"required":required.len(),"scope":"required_substrings_only"}),
            )
        }
        Scorer::JsonSchema { schema, expected } => {
            let parsed: Option<Value> = serde_json::from_str(answer.trim()).ok();
            let validator = match jsonschema::validator_for(schema) {
                Ok(v) => v,
                Err(_) => return Score::unavailable("scorer_error"),
            };
            let valid = parsed.as_ref().is_some_and(|v| validator.is_valid(v));
            let correct = expected.as_ref().is_none_or(|v| parsed.as_ref() == Some(v));
            (
                valid && correct,
                json!({"schema_valid":valid,"value_correct":correct,"semantic_oracle":expected.is_some()}),
            )
        }
        Scorer::Tools {
            calls,
            unordered,
            final_answers,
            final_state,
        } => {
            let mut remaining = observed.to_vec();
            let matches = if *unordered {
                calls.iter().all(|c| {
                    remaining
                        .iter()
                        .position(|o| o == c)
                        .map(|i| {
                            remaining.remove(i);
                            true
                        })
                        .unwrap_or(false)
                }) && remaining.is_empty()
            } else {
                observed == calls
            };
            let answer_ok = final_answers.is_empty() || answer_matches(answer, final_answers);
            let state_ok = final_state.iter().all(|(k, v)| state.get(k) == Some(v));
            (
                matches && answer_ok && state_ok,
                json!({"calls_correct":matches,"answer_correct":answer_ok,"state_correct":state_ok,"observed_calls":observed.len(),"expected_calls":calls.len()}),
            )
        }
        Scorer::Unsupported { .. } => return Score::unavailable("unsupported"),
    };
    Score {
        status: if passed { "passed" } else { "failed" }.into(),
        passed: Some(passed),
        details,
    }
}

pub fn execute(
    call: &ExpectedCall,
    allowed: &[String],
    state: &mut BTreeMap<String, Value>,
) -> Value {
    if !allowed.contains(&call.name) {
        return json!({"error":"unknown_tool"});
    }
    let defs = fixture_definitions(std::slice::from_ref(&call.name));
    if !jsonschema::is_valid(&defs[0]["function"]["parameters"], &call.arguments) {
        return json!({"error":"invalid_arguments"});
    }
    let args = &call.arguments;
    match call.name.as_str() {
        "add" => {
            let Some(sum) = args["a"]
                .as_f64()
                .zip(args["b"].as_f64())
                .map(|(a, b)| a + b)
                .filter(|x| x.is_finite())
            else {
                return json!({"error":"numeric_overflow"});
            };
            json!({"result":sum})
        }
        "kv_get" => match state.get(args["key"].as_str().unwrap()) {
            Some(v) => json!({"value":v}),
            None => json!({"error":"not_found"}),
        },
        "kv_set" => {
            state.insert(args["key"].as_str().unwrap().into(), args["value"].clone());
            json!({"ok":true})
        }
        _ => json!({"error":"unknown_tool"}),
    }
}
