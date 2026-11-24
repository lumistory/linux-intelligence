<div align="center">

<img src="assets/logo.png" alt="LumiStory" width="160">

# linux-intelligence

**Linux 系统观察、操作审计与恢复基础设施。**

*Linux System Observability & Operation Recovery Infrastructure.*

![phase](https://img.shields.io/badge/phase-1%20in%20progress-orange)
![rust](https://img.shields.io/badge/rust-2024%20edition-orange)
![license](https://img.shields.io/badge/license-not%20yet%20chosen-lightgrey)

</div>

---

## 这是什么

一台 Linux 机器上，你敲了什么、它启动了哪些进程、磁盘上哪些文件变了、这些变化跟你的操作有多确定的关系、出问题时哪些恢复点可信 —— 把这些记下来，并且**只说自己测得准的部分**。

三个刻意的不绑定：

**不绑定 AI。** 核心不含任何模型、API Key、Prompt。AI 只是外部使用者之一。

**不绑定发行版。** 通过 `/etc/os-release` 认发行版，但选哪套能力靠**实际探测**，不靠名字推断。Arch / Ubuntu / Debian / Fedora 都有实测记录。

**不绑定 Shell。** Bash / Zsh / Fish 各有独立适配器，各自声明能力，写不出来就标 `unknown`，不假装能观察。

## 为什么难

这类工具最容易犯的错是**把时间上的先后说成因果**。文件变了、你三秒前跑过 `make`，那几乎肯定是它 —— 但 inotify 不携带 pid，非特权的 fanotify 会被拒，所以「谁改的」这件事在很多情况下**测不出来**。

本项目的做法是分级而不是猜：`Confirmed / Strong / Probable / Weak / Unknown`，文件改动被**封顶在 Probable** —— 因为 `Confirmed` 要求把路径绑到命令文本上，而那对编译器、包管理器、git、make、测试运行器全都做不到，也就是恰好对我们最在乎的那些写操作做不到。

说「不知道」比编一个答案有用。

## Phase 0 已经跑完了

八项技术验证全部在本机执行，原始日志保留，被实测推翻的假设**保留在记录里**不改写成通过：

| 实验 | 结论 | 关键实测 |
|---|---|---|
| 0-A | 三种 Shell 的 hook 可用但有代价 | 39/39 用例；hook 本身 bash 3 µs / zsh 2.3 ms / fish 1.1 ms |
| 0-B | 脚本生命周期可观察，内部语句不可 | 4000 次命令调用只产出 2 个事件 |
| 0-C | UDS 并发与可靠性 | per-event ack 丢 12%，投递证明只能靠 watermark |
| 0-D | PTY 跨 Shell | 49/49 断言，4 会话零串扰 |
| 0-E | procfs 与 inotify | 7 个假设里 2 个被证伪，照实记 |
| 0-F | 四个发行版的兼容性 | 21 字段 × 5 环境，内核层面 12 同 9 异 |
| 0-G | 权限模型 | 29/29 检查，发现并修掉 5 个真实缺陷 |
| 0-H | 去重、关联、上下文 | 24 个可信度场景，10 万行基准 |

结论写在 [`docs/reports/PHASE0-REPORT.md`](docs/reports/PHASE0-REPORT.md)，包括 §5 的已知问题和 §6 的安全风险 —— 那些都没有删。

## 现在到哪一步

Phase 1（核心基础设施）进行中。已完成 `li-proto`（事件信封、消息帧、版本门、JSON 编解码，63 条测试）。

```
crates/
  li-proto/     # ✅ 事件信封、消息类型、版本协商。零 I/O
  li-journal/   # ⬜ SQLite schema、迁移、追加式写入
  li-hook/      # ⬜ shell 适配器用的极小程序，不依赖 tokio
  li-daemon/    # ⬜ tokio 守护进程：socket、摄取、观察者
  li-cli/       # ⬜ 用户命令 li run / li query / li checkpoint
```

## 明确不做的事

- **没有 AI**。不调模型、不含提示词逻辑。接口默认只读。
- **没有特权 helper**。Phase 1 是零特权的用户守护进程。特权辅助推迟到 Phase 3 最早，且只有 Phase 2 实测出「非它不可」的能力才做。
- **没有破坏性恢复**。Phase 1 只创建检查点，不恢复。真实系统上的恢复实验只在临时环境里做。
- **不做文件到进程的归因**。见上面「为什么难」。
- **不碰 eBPF、不碰内核 audit**。

## 文档

| 文件 | 内容 |
|---|---|
| [`docs/reports/PHASE0-REPORT.md`](docs/reports/PHASE0-REPORT.md) | Phase 0 完成报告 |
| [`docs/adr/`](docs/adr/) | 六份架构决策记录 |
| [`docs/spec/phase1-implementation-spec.md`](docs/spec/phase1-implementation-spec.md) | Phase 1 施工图，含 12 条从实测来的硬约束 |
| [`docs/ipc-protocol-v0.md`](docs/ipc-protocol-v0.md) | 线格式 |
| [`docs/security-boundary-v0.md`](docs/security-boundary-v0.md) | 信任边界 |

## 开发

```bash
cargo build --workspace
cargo test -p li-proto
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

`experiments/` 下是 Phase 0 的八个独立 Cargo 包，各自能单独构建（workspace 里用 `exclude` 挡住了）。

实验日志里保留了运行时原样观测到的字节，包括采集机器的绝对路径。公开前这些路径被替换成
`<WORKSPACE>` / `<HOME>`，所有被测量的量（时间戳、PID、时序、退出码、计数）一字未动。
规则与原始形态见 [`experiments/SCRUB-NOTICE.md`](experiments/SCRUB-NOTICE.md)。

---

<sub>独立项目。Lumi 是它的第一个预期使用者，但不是它的前提。</sub>
