<div align="center">

# linux-intelligence

**Linux 系统观察、操作审计与恢复基础设施。**

*Linux System Observability & Operation Recovery Infrastructure.*

![phase](https://img.shields.io/badge/phase-0--6%20implemented-green)
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

Phase 0–6（含 D1 受限零-exec `li-helper`、D2 `/tmp` 沙盒 `BtrfsProvider` / 探针 `LvmProvider`、D5 ADR/协议转 `accepted`、Phase 6 POSIX `probe.sh` 与 systemd user unit）已在本机完成实现与验证；跨架构/musl/`hidepid` 等本机未覆盖项在各报告与 [`packaging/README.md`](packaging/README.md) 中诚实标注 `NOT TESTED`。

逐任务的状态、每个阶段的验收标准与「明确不做的事」在 [`docs/PLAN.md`](docs/PLAN.md)。

```
crates/
  li-proto/     # ✅ 事件信封、消息类型（含 DropReport）、版本协商。零 I/O
  li-journal/   # ✅ SQLite schema、迁移、追加式写入、脱敏、审计式 purge
  li-hook/      # ✅ shell 适配器用的无 Tokio 客户端，继承 FD 5ms 超时与 DropReport
  li-daemon/    # ✅ Tokio UDS 守护进程、procfs/inotify、PTY+responder、关联引擎、恢复提供者、li-helper
  li-cli/       # ✅ 用户命令 li run / query / checkpoint / purge / correlate / status / ping / shell-init
shell/          # ✅ bash / zsh / fish 适配器与能力表（39/39 PTY 矩阵通过）
packaging/      # ✅ 严格 POSIX 能力探测 probe.sh、systemd user unit 与打包说明
```

## 明确不做的事

- **没有 AI**。不调模型、不含提示词逻辑。接口默认只读。
- **用户守护进程默认零特权**。`li-daemon` 以普通用户身份运行；可选受限辅助进程 `li-helper` 零 `execve`、闭集 opcode、`openat2(BENEATH|NO_SYMLINKS|NO_MAGICLINKS)` 与 `PR_SET_NO_NEW_PRIVS`。
- **不在真实宿主机执行破坏性恢复**。`BtrfsProvider` 仅在沙盒镜像（`/tmp`）创建/恢复与验真，绝不触碰宿主机真卷；`LvmProvider` 按能力探测诚实降级。
- **不做文件到进程的归因**。见上面「为什么难」。
- **不碰 eBPF、不碰内核 audit**。

## 文档

| 文件 | 内容 |
|---|---|
| [`docs/PLAN.md`](docs/PLAN.md) | **总计划**：六层架构、Phase 0–6 状态、12 条硬约束、明确不做的事、判断点 |
| [`docs/reports/`](docs/reports/) | Phase 0 及 Phase 1–6 各阶段验收报告 |
| [`docs/adr/`](docs/adr/) | 六份架构决策记录（`accepted`） |
| [`docs/spec/phase1-implementation-spec.md`](docs/spec/phase1-implementation-spec.md) | Phase 1 施工图（accepted），含 12 条从实测来的硬约束 |
| [`docs/ipc-protocol-v0.md`](docs/ipc-protocol-v0.md) | 线格式（`accepted`） |
| [`docs/security-boundary-v0.md`](docs/security-boundary-v0.md) | 信任边界（`accepted`） |

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
