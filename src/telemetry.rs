use crate::{adapter, config::Telemetry};
use anyhow::{anyhow, ensure, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub status: String,
    pub samples: Vec<Sample>,
}

pub async fn snapshot(client: &reqwest::Client, cfg: &Telemetry) -> Snapshot {
    match tokio::time::timeout(Duration::from_secs(5), fetch(client, cfg)).await {
        Ok(Ok(samples)) => Snapshot {
            status: "success".into(),
            samples,
        },
        _ => Snapshot {
            status: "unavailable".into(),
            samples: vec![],
        },
    }
}
async fn fetch(client: &reqwest::Client, cfg: &Telemetry) -> Result<Vec<Sample>> {
    let mut req = client.get(&cfg.url);
    if let Some(key) = adapter::auth(&cfg.api_key_env)? {
        req = req.bearer_auth(key);
    }
    let response = req.send().await?;
    ensure!(response.status().is_success(), "telemetry HTTP failure");
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend(chunk?);
        ensure!(
            bytes.len() <= 8 * 1024 * 1024,
            "telemetry response too large"
        );
    }
    parse(std::str::from_utf8(&bytes)?, cfg)
}
pub fn parse(text: &str, cfg: &Telemetry) -> Result<Vec<Sample>> {
    let mut samples = Vec::new();
    for line in text
        .lines()
        .filter(|s| !s.starts_with('#') && !s.trim().is_empty())
    {
        let name_end = line
            .find(|c: char| c == '{' || c.is_whitespace())
            .unwrap_or(line.len());
        let name = &line[..name_end];
        if !cfg
            .histograms
            .values()
            .any(|m| name == format!("{m}_sum") || name == format!("{m}_count"))
        {
            continue;
        }
        let mut rest = line[name_end..].trim_start();
        let mut labels = BTreeMap::new();
        if rest.starts_with('{') {
            rest = &rest[1..];
            while !rest.starts_with('}') {
                let eq = rest.find('=').ok_or_else(|| anyhow!("bad metric label"))?;
                let key = rest[..eq].trim().to_string();
                rest = rest[eq + 1..].trim_start();
                ensure!(rest.starts_with('"'), "bad label string");
                let mut end = None;
                let mut escaped = false;
                for (i, c) in rest.char_indices().skip(1) {
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if c == '\\' {
                        escaped = true;
                        continue;
                    }
                    if c == '"' {
                        end = Some(i + 1);
                        break;
                    }
                }
                let end = end.ok_or_else(|| anyhow!("bad label string"))?;
                labels.insert(key, serde_json::from_str::<String>(&rest[..end])?);
                rest = rest[end..].trim_start();
                if rest.starts_with(',') {
                    rest = rest[1..].trim_start();
                } else {
                    ensure!(rest.starts_with('}'), "bad label separator");
                }
            }
            rest = rest[1..].trim_start();
        }
        if !cfg.labels.iter().all(|(k, v)| labels.get(k) == Some(v)) {
            continue;
        }
        let value = rest
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow!("missing sample"))?
            .parse::<f64>()?;
        ensure!(
            value.is_finite() && value >= 0.0,
            "invalid histogram sample"
        );
        ensure!(
            !samples
                .iter()
                .any(|s: &Sample| s.metric == name && s.labels == labels),
            "duplicate telemetry series"
        );
        samples.push(Sample {
            metric: name.into(),
            labels,
            value,
        });
    }
    Ok(samples)
}
pub fn summarize(before: &Snapshot, after: &Snapshot, cfg: &Telemetry) -> Value {
    let mut values = BTreeMap::new();
    for (phase, name) in &cfg.histograms {
        let paired = [before, after].iter().all(|snapshot| {
            let sums: std::collections::BTreeSet<_> = snapshot
                .samples
                .iter()
                .filter(|s| s.metric == format!("{name}_sum"))
                .map(|s| &s.labels)
                .collect();
            let counts: std::collections::BTreeSet<_> = snapshot
                .samples
                .iter()
                .filter(|s| s.metric == format!("{name}_count"))
                .map(|s| &s.labels)
                .collect();
            sums == counts
        });
        let delta = |suffix: &str| -> Option<f64> {
            let metric = format!("{name}_{suffix}");
            let old: Vec<_> = before
                .samples
                .iter()
                .filter(|s| s.metric == metric)
                .collect();
            let new: Vec<_> = after
                .samples
                .iter()
                .filter(|s| s.metric == metric)
                .collect();
            if old.is_empty() || old.len() != new.len() {
                return None;
            }
            new.iter()
                .map(|n| {
                    old.iter().find(|o| o.labels == n.labels).and_then(|o| {
                        if n.value >= o.value {
                            Some(n.value - o.value)
                        } else {
                            None
                        }
                    })
                })
                .sum()
        };
        let result = if before.status == "success" && after.status == "success" && paired {
            delta("sum").zip(delta("count")).filter(|(_,count)|*count>0.0).map(|(sum,count)|json!({"status":"available","mean_seconds":sum/count,"observations":count,"sum_seconds":sum}))
        } else {
            None
        };
        values.insert(phase,result.unwrap_or_else(||json!({"status":"not_available","reason":"missing_series_reset_or_no_observations"})));
    }
    json!({"scope":"server_histogram_interval","exclusive_declared":cfg.exclusive,"phases":values,"limitations":["Interval includes scrape boundary error and possibly other traffic.","Histogram means are not request-linked timings or kernel time.","Counter resets or changing series invalidate the phase estimate."]})
}
