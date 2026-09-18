# 当前实现与使用边界

2026-09-18：Rust 初始实现，提供 CLI 和 library。已完成受控协议与指标测试；不宣称完整 M1–M4 验收完成，不宣称已验证真实引擎性能。

## 已实现

- `validate / plan / probe / run / scan / report / compare`；严格 JSON 配置，错误字段不会静默忽略。
- Chat Completions 与 Completions 的流式/非流式调用，UTF-8/SSE 拆包、空角色帧、usage 尾帧、工具参数分片处理。
- 并发与 open-loop episode 调度、显式客户端拒绝、请求/episode/调度/排空预算、Ctrl-C 后有限证据落盘。无自动重试。
- 延迟分布、输入/输出吞吐、SLO goodput、失败计数、内容/语言分组、质量随负载变化；没有捏造精确 ITL。
- 确定性答案评分；工具选择和参数、调用顺序/无序集合、最终状态和最终回答检查。工具只操作每个 episode 独立的测试状态。
- 单独采集 Prometheus histogram 的 sum/count，计算所选标签范围内的区间阶段均值。保留采集快照，不用客户端 TTFT 反推 Prefill。
- JSON/CSV/Markdown 报告及离线重算。episodes.jsonl 和 scores.jsonl 是规范记录；manifest 保存 SHA-256。requests/events/tool-traces 为派生视图。

## 可执行配置

`examples/benchmark.json` 用于性能，`examples/suite.json` 用于可评分题目，另有 Prefill-heavy、Decode-heavy、Mixed 和 vLLM 遥测配置。先改模型和地址，再 `plan`。示例里没有真实凭据、正式题库或性能结果。

题目为 JSONL，一行一个 case。必须有 id、group、language，prompt 与 messages 二选一。可选 tags、scorer、tools、初始 state。标准答案保留在 scorer 中，不发送给待测模型。

```json
{"id":"sum","group":"math","language":"zh","prompt":"2+3等于多少？只输出数字。","scorer":{"kind":"numeric","expected":5,"tolerance":0}}
```

评分器：`exact`（answers、可选 case_sensitive）、`numeric`（expected、tolerance）、`contains`（required）、`json_schema`（schema、可选 expected）、`tools`（calls、unordered、final_answers、final_state）、`unsupported`（reason）。`contains` 仅验证指定片段；schema 合法不等于答案正确。JSON Schema 远程引用被禁用。

```json
{"id":"code-exec","group":"code","language":"en","prompt":"Write a sorting function.","scorer":{"kind":"unsupported","reason":"generated-code sandbox is not implemented"}}
```

这个例子会记录 unsupported，不执行生成代码，也不计为通过。

CLI 返回码区分运行状态，不直接充当质量门槛。模型全部 HTTP 调用成功但答错时，运行仍可返回 0；自动化验收必须检查 summary.json 的质量准确率与覆盖率。失败请求/拒绝/截断会降低质量，不会从分母消失；不支持的题目单列。重复样本属于重复观测，当前未实现置信区间或独立样本显著性判断。

SLO 对 episode 使用从计划到达到首内容/终态的时间，包含调度等待；HTTP 延迟分布从调用 transport 前开始。工具耗时与完整调用可执行时间单列，上限时间点包含尾帧完成等待。工具 fixture 顺序执行，即使模型一次输出多个调用；它验证调用集合，但不模拟真实并行工具服务的性能。

运行记录保存在内存并按完成 episode 追加日志；质量原始回答在测量结束后内存评分，默认不落盘。因此大规模运行仍应通过较小请求预算和分批 scan 控制客户端内存。没有分布式压测或可恢复执行；新 run 不覆盖旧目录。骤然 kill 导致的不完整 JSONL 尾行会使重算拒绝，不能冒充完整报告。

## 尚未交付

- 真实 vLLM、SGLang、私有端点的版本兼容与生成验收。这里只构建和测试工具，没有启动模型服务。
- 非兼容私有协议的插件注册接口；当前支持兼容 HTTP API 和自定义 telemetry histogram 映射。
- 生成代码沙箱、主观回答 judge、上游 lm-eval/BFCL 正式集、动态用户多轮脚本、tokenizer 精确长度控制。
- Prefill/Decode 请求级 trace 关联、KV cache/显存/功耗采样、PD 传输分解、GPU kernel profiler。
- 配对统计区间、独立重评分、自动调优、Web UI 和多模态。

保留完整设计方向，但这些能力不会因为存在配置占位或 smoke case 而被标为完成。

## 验证

协议测试使用临时本地 HTTP fixture：覆盖 SSE 任意 UTF-8 字节边界、断流、429 不重试、开环过载、预算终止、多轮工具和非流式 Completions。纯函数测试用手算数据验证统计分母、缺失 token 使用量、分位数、隐藏 reasoning、工具状态和遥测 reset。CI 配置检查 Linux/macOS/Windows；各平台是否通过以实际 CI 为准。

本次本地验证：Rust 1.95.0 / macOS ARM64；`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、29 项测试、`cargo build --release --locked` 均通过。release 二进制约 7.4 MiB；六份配置通过实际 CLI validate，scan 的 plan-only 未创建运行目录。未连接模型端点，未产生真实模型 benchmark 结果。
