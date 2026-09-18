use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use serving_bench::{
    adapter,
    config::{Config, Mode},
    runner,
};
use std::{fs, path::PathBuf, time::Instant};

#[derive(Parser)]
#[command(
    version,
    about = "Measure deployed model performance, answer quality, and tool correctness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Validate configuration and dataset without network access.
    Validate { config: PathBuf },
    /// Show budgets and dataset coverage without network access.
    Plan { config: PathBuf },
    /// Send one short generation request and report observed protocol capabilities.
    Probe { config: PathBuf },
    /// Run a bounded evaluation against the configured endpoint.
    Run {
        config: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Recompute reports from saved evidence without contacting the endpoint.
    Report { directory: PathBuf },
    /// Compare compatible runs; return differences without declaring a winner.
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
    },
    /// Run bounded concurrency points and repetitions sequentially.
    Scan {
        config: PathBuf,
        #[arg(long, value_delimiter = ',', default_value = "1,4,16")]
        concurrency: Vec<usize>,
        #[arg(long, default_value_t = 3)]
        repetitions: usize,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        plan_only: bool,
    },
}
#[tokio::main]
async fn main() {
    match execute(Cli::parse()).await {
        Ok((value, success)) => {
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            if !success {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(2);
        }
    }
}
fn passed(summary: &Value) -> bool {
    summary["complete"] == true
        && summary["overall"]["statuses"]
            .as_object()
            .is_some_and(|s| s.keys().all(|k| k == "success"))
}
async fn execute(cli: Cli) -> Result<(Value, bool)> {
    let result = match cli.command {
        Command::Validate { config } | Command::Plan { config } => {
            let (cfg, cases, hash) = Config::load(&config)?;
            let mut p = runner::plan(&cfg, &cases);
            p["dataset_sha256"] = json!(hash);
            (p, true)
        }
        Command::Probe { config } => {
            let (mut cfg, _, _) = Config::load(&config)?;
            cfg.generation.max_tokens = 16;
            let client = adapter::client(&cfg)?;
            adapter::auth(&cfg.target.api_key_env)?;
            let body = adapter::payload(&cfg, &[json!({"role":"user","content":"Reply OK."})], &[]);
            let response = adapter::request(&client, &cfg, body, 0, 0, 0.0, Instant::now()).await;
            let ok = response.record.status == "success";
            (
                json!({"status":response.record.status,"http_status":response.record.http_status,"stream_requested":cfg.generation.stream,"observed_content_events":response.record.events.len(),"usage_observed":response.record.output_tokens.is_some(),"finish_reason":response.record.finish_reason,"tools":"not_probed","engine_phases":"not_probed"}),
                ok,
            )
        }
        Command::Run { config, output } => {
            let (cfg, cases, hash) = Config::load(&config)?;
            let summary = runner::run(cfg, cases, hash, &output).await?;
            let ok = passed(&summary);
            (summary, ok)
        }
        Command::Report { directory } => {
            let summary = runner::report(&directory)?;
            let ok = summary["complete"] == true;
            (summary, ok)
        }
        Command::Compare {
            baseline,
            candidate,
        } => {
            let result = runner::compare(&baseline, &candidate)?;
            let ok = result["comparable"] == true;
            (result, ok)
        }
        Command::Scan {
            config,
            concurrency,
            repetitions,
            output,
            plan_only,
        } => {
            let (cfg, cases, hash) = Config::load(&config)?;
            ensure!(
                !concurrency.is_empty()
                    && concurrency.len() <= 32
                    && repetitions > 0
                    && repetitions <= 100,
                "invalid scan size"
            );
            ensure!(
                concurrency.iter().all(|c| *c > 0 && *c <= 4096),
                "invalid concurrency"
            );
            let jobs = concurrency.len() * repetitions;
            if plan_only {
                return Ok((
                    json!({"jobs":jobs,"max_episodes":jobs*cfg.load.requests,"per_run":runner::plan(&cfg,&cases),"network_requests_sent":false}),
                    true,
                ));
            }
            if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            fs::create_dir(&output)?;
            let mut results = Vec::new();
            let mut success = true;
            for rep in 0..repetitions {
                for (point, c) in concurrency.iter().enumerate() {
                    let mut next = cfg.clone();
                    next.load.concurrency = *c;
                    if next.mode == Mode::Quality {
                        next.mode = Mode::QualityUnderLoad;
                    }
                    let directory = output.join(format!("r{rep}-p{point}-c{c}"));
                    let summary =
                        runner::run(next, cases.clone(), hash.clone(), &directory).await?;
                    success &= passed(&summary);
                    results.push(json!({"repetition":rep,"concurrency":c,"directory":directory.file_name(),"summary":summary}));
                    fs::write(
                        output.join("scan.json"),
                        serde_json::to_vec_pretty(&results)?,
                    )?;
                    if results.last().unwrap()["summary"]["complete"] != true {
                        return Ok((json!({"runs":results,"complete":false}), false));
                    }
                }
            }
            (json!({"runs":results,"complete":true}), success)
        }
    };
    Ok(result)
}
