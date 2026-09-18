# serving-bench

Engine-neutral performance measurement for deployed language models.

**Status: design proposal. No executable toolkit or measured results yet.**

Measure existing vLLM, SGLang, and private inference endpoints through a shared workload, event model, and metric definition. A Python library and CLI are proposed; neither requires installing an inference engine or loading model weights.

Start with the [中文设计讨论稿](docs/design.zh-CN.md) and [illustrative configuration](examples/benchmark.json). Names, interfaces, and scope remain open for discussion.

## Proposed scope

- Streaming and non-streaming text generation over compatible HTTP APIs.
- Fixed concurrency and explicit arrival-rate workloads.
- Client-observed latency, throughput, failure rates, and SLO goodput.
- Reproducible request records, offline aggregation, and comparable reports.
- Small adapters for private protocols; optional engine telemetry later.

This project measures running services. [Forge](https://github.com/LDB-org/Forge) searches deployment configurations; [LDB](https://github.com/LDB-org/LDB) handles runtime debugging. Neither is a runtime dependency.

## Proposed workflow (not implemented)

```text
serving-bench validate benchmark.json
serving-bench probe benchmark.json
serving-bench run benchmark.json --output runs/example
serving-bench report runs/example
serving-bench compare runs/baseline runs/candidate
```

`validate` would operate locally. `probe` would send a small explicit capability check. `run` would create load against the specified endpoint. Report and comparison would use saved artifacts only.

No deployment automation, model quality leaderboard, or web dashboard is planned for the first version. Licensing and public release are undecided.
