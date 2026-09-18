# serving-bench

A Rust CLI and library for measuring deployed language-model services: latency, throughput, answer quality, and tool-call correctness.

**Status: usable initial implementation, tested with controlled HTTP fixtures. Real vLLM, SGLang, and private-engine acceptance is pending.** The full [evaluation matrix](docs/evaluation-matrix.zh-CN.md) remains the roadmap; see [当前实现与边界](docs/implementation.zh-CN.md) for delivered capabilities.

## Build

```sh
cargo build --release --locked
./target/release/serving-bench --help
```

Windows: `target\release\serving-bench.exe`. The binary requires neither Python nor Torch/CUDA. Build for each destination platform; this is not a universal cross-platform binary. Cargo downloads Rust dependencies on the first build. The toolchain file selects stable Rust; this revision was compiled locally with Rust 1.95.0 on macOS ARM64.

## Use

Edit `target.base_url` and `target.model` in a copied example. Dataset paths resolve relative to the configuration file. For authentication set `target.api_key_env` to the name of an environment variable containing the key. The key itself is never written into run artifacts. TLS certificate verification is enabled; `target.ca_file` adds a private CA. HTTP redirects, environment proxies, and automatic retries are disabled.

```sh
# Local validation and budget inspection; no endpoint traffic.
./target/release/serving-bench validate examples/benchmark.json
./target/release/serving-bench plan examples/suite.json

# The following commands send requests to your configured service.
./target/release/serving-bench probe examples/benchmark.json
./target/release/serving-bench run examples/benchmark.json --output runs/performance
./target/release/serving-bench run examples/suite.json --output runs/quality
./target/release/serving-bench scan examples/suite.json --concurrency 1,4,16 --repetitions 3 --output runs/scan --plan-only
./target/release/serving-bench scan examples/suite.json --concurrency 1,4,16 --repetitions 3 --output runs/scan

# Offline artifact aggregation and comparison.
./target/release/serving-bench report runs/performance
./target/release/serving-bench compare runs/baseline runs/candidate
```

Output directories must be new. `requests` means **episodes**, and cases cycle in dataset order; a tool episode can contain several HTTP requests. `quality` requires closed-loop concurrency 1; `quality_under_load` allows load testing. `scan` stores each point and repetition separately, without averaging percentiles or claiming statistical significance. Budgets apply per run; inspect the number of jobs before executing a scan. `compare` refuses an affirmative comparability result when deployment metadata or cache policy is unknown, while still returning descriptive results. Supply `target.metadata` fields engine, engine_version, hardware, tokenizer, chat_template, model_revision, and quantization for controlled comparisons.

For open-loop traffic, set `load.kind` to `open_loop`, `load.rate` to the episode arrival rate, and `load.poisson` to true or false. At the concurrency cap, an arrival is recorded as `client_rejected`; it is not silently delayed into a closed-loop workload. `max_seconds` limits scheduling, `drain_seconds` limits completion wait, and each HTTP request and episode has its own timeout. Warmup has a separate bounded budget and uses distinct prompts.

## Coverage

| Area | Implemented |
| --- | --- |
| Protocol | Chat/completions, streaming SSE and non-streaming JSON, tool-call fragment assembly |
| Performance | Client TTFT, first-answer latency, E2E, chunk gaps, cautious visible TPOT estimate, throughput, failures, SLO goodput |
| Answer quality | Exact answers, numeric tolerance, required substrings, JSON Schema with optional exact value oracle |
| Tools | Deterministic `add`, `kv_get`, `kv_set`; call/argument matching, ordered or unordered sets, final answer and state checks |
| Workloads | Content/language groups, fixed concurrency, open-loop arrivals, concurrency scans, repeated runs |
| Prefill/decode | Long-input, long-output, mixed examples; explicit Prometheus histogram mappings for phase means |
| Evidence | Per-episode/request/events/scores/tool traces, SHA-256 checks, offline JSON/CSV/Markdown reports |

[Prefill](examples/prefill.json), [decode](examples/decode.json), and [mixed](examples/mixed.json) profiles vary actual content and output budgets. Their input token lengths are **not** certified: no tokenizer is bundled. [vLLM telemetry](examples/vllm-telemetry.json) is an optional example; verify actual metric names, labels, and definitions on the deployed version. Other engines can supply their own histogram mappings. Telemetry credentials use a separate environment-variable setting.

The included 12 quality cases are original smoke fixtures, **not a model capability benchmark**. Code reading is covered; generated-code execution, subjective judge grading, official lm-eval/BFCL integration, tokenizer-controlled length sweeps, and non-compatible private protocol plugins remain to be implemented. Unsupported scorers can be explicitly declared and are reported without fabricated scores.

## Measurement rules

- SSE chunks are not tokens. Exact ITL is unavailable. TTFT is not pure prefill latency.
- Missing token usage makes whole-cohort token throughput unavailable; coverage is shown.
- Visible TPOT is only estimated for multiple content events, more than one output token, no tool/reasoning events, and explicit zero reasoning tokens in usage. The formula uses the last content timestamp, not trailing usage-frame arrival.
- Server telemetry is a separate interval-level source. It may include other traffic; counter resets and changing series invalidate estimates. Histogram means cannot be assigned to individual benchmark requests.
- The cohort window includes drain time. Failures remain in the episode denominator. Quality scoring runs after the measurement window.
- `finish_reason=length` is a normal capped performance result; quality mode counts it as truncated failure.
- Raw answers and tool arguments/results are not saved by default. `save_content: true` opts into local content retention. Offline reporting reaggregates saved scores; independent regrading is not implemented.
- Reports separate quality coverage, transport success, and correctness. CLI exit 0 means the run completed with successful episodes, **not** that every answer was correct; inspect quality scores. Exit 1 denotes unsuccessful/incomplete episodes or an incompatible comparison, and exit 2 denotes a configuration/I/O error.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests create short-lived local HTTP fixtures and do not load a model. CI is configured for Linux, macOS, and Windows; local compilation only establishes the platform actually tested.

The Rust library exposes `config::Config::load`, `runner::plan`, `runner::run`, `runner::report`, and `runner::compare`, plus protocol, scoring, and metric modules. [Forge](https://github.com/LDB-org/Forge) can later consume versioned artifacts; [LDB](https://github.com/LDB-org/LDB) remains the separate runtime debugger. Neither is a dependency. Licensed under [MIT](LICENSE). Crates.io publication is not enabled.
