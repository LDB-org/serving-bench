# serving-bench

Engine-neutral performance, answer quality, and tool-use evaluation for deployed language models.

**Status: design proposal. No executable toolkit or measured results yet.**

Measure existing vLLM, SGLang, and private inference endpoints through a shared workload, event model, and metric definition. A Python library and CLI are proposed; neither requires installing an inference engine or loading model weights.

Start with the [中文设计讨论稿](docs/design.zh-CN.md), [evaluation matrix](docs/evaluation-matrix.zh-CN.md), and [illustrative suite configuration](examples/suite.json). Names, interfaces, and scope remain open for discussion.

## Proposed scope

- Streaming and non-streaming text generation over compatible HTTP APIs.
- Fixed concurrency and explicit arrival-rate workloads.
- Client-observed latency, throughput, failure rates, and SLO goodput.
- Prefill/decode workload profiles and separately sourced engine telemetry.
- Answer quality and tool-call correctness across languages, content types, and load levels.
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

No deployment automation, public leaderboard, or web dashboard is planned for the first version. Licensing and public release are undecided.

The initial release scope includes performance, quality, and tool correctness. Implementation milestones are incremental; the project remains design-only. A basic performance configuration is also available in [benchmark.json](examples/benchmark.json).
