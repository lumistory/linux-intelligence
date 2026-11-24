# linux-intelligence 总计划

Status: **authoritative plan, 2026-11-24**. 分支 `master`，基线提交 `619f14e`。

这份文档把《完整工程实施总任务书》落成可执行的长期计划，并且与仓库的**当前实际状态**
对齐：每一项「已完成」都能追到一次真实执行，每一项「未开始」都不会因为文档写得像
而变成已完成。

## 0. 怎么读这份文档

### 0.1 状态词表（只有这四个）

| 词 | 含义 | 判定依据 |
|---|---|---|
| **已实现** | 代码存在，并且有通过了的测试或实测数据 | 必须能指出测试名或实验日志 |
| **骨架** | crate/文件存在，但没有可执行逻辑 | `fn main()` 为空或只有 TODO |
| **未开始** | 仓库里不存在 | `ls` 不到 |
| **未测量** | 有代码但关键数字没测过 | 写明缺哪个数 |

不写「基本完成」「大致可用」。一个东西要么有证据，要么进「未开始」。

### 0.2 证据约定

- `0-A`…`0-H` 指 `experiments/00-*/REPORT.md`，数字一律引自那些文件里的表格。
- `PHASE0 §5.3` 指 `docs/reports/PHASE0-REPORT.md` 第 5 节的一个条目。
- `B1 §4.2` / `B2 §5.1` 指 `docs/reports/PHASE1-BATCH1-REPORT.md` /
  `PHASE1-BATCH2-REPORT.md` 的对应小节。
- 引用不到出处的数字，本文件写「未测量」。

### 0.3 两处必须说清的重建

**其一，任务书原文不在仓库里。** 本文件第 2 节的六层（L1–L6）与 14 个组件，是按
`docs/spec/phase1-implementation-spec.md §1` 的 crate 清单、八个实验各自的观察面
（0-A shell hooks / 0-B script lifecycle / 0-D PTY / 0-E procfs+inotify /
0-F 发行版兼容 / 0-G privilege+helper / 0-H correlation，以及 0-C 的传输层）
和 `docs/security-boundary-v0.md §1.1` 的组件表**重建**的。若与任务书原文的切分不同，
以任务书为准，只改那一张表，不改任何状态判断 —— 状态判断的依据是仓库，不是命名。

**其二，Phase 2–6 的阶段目标同样是重建。** 仓库里对这几个阶段只有零散约束
（0-G §8.2：helper 最早 Phase 3；spec §1：`li-recovery` 主要属于 Phase 5；
任务书：Phase 2 之前不做 eBPF 与内核 audit），没有文档系统性界定。凡属重建处都标
「**待确认**」。

---

## 1. 项目定位与六条不可违反的原则

一台 Linux 机器上，你敲了什么、它启动了哪些进程、磁盘上哪些文件变了、这些变化跟你
的操作有多确定的关系、出问题时哪些恢复点可信 —— 把这些记下来，并且**只说自己测得准
的部分**。

六条原则，每条标注它已经被哪份 ADR 或哪个实验落实：

| # | 原则 | 具体含义 | 落实在哪 | 证据 |
|---|---|---|---|---|
| P1 | **独立性** | 核心不含模型、Prompt、API Key；不联网；数据在本地 SQLite；协议只在本机 UDS 上 | ADR 0001（Rust + Tokio，单二进制）、ADR 0002（`AF_UNIX` + JSONL） | `crates/*/Cargo.toml` 依赖只有 `serde` / `serde_json` / `rusqlite`；`grep -rni api.key crates/` 只命中 `li-journal/src/redact.rs` 的脱敏规则与测试字符串；0-H §6.10：全程 `--offline`，一次网络 fetch 都没做 |
| P2 | **Linux 通用性** | 不按发行版名字选能力，按探测到的事实选；`/etc/os-release` 只用于显示 | ADR 0004（核心不认识 shell 特性）、C12 | 0-F：4 发行版 × 21 字段，**12 同 9 异**；差异全在 userland（`/bin/sh`、`echo -e`、`[[ ]]`、`set -o pipefail`、glibc 2.39–2.44），内核字段全同；0-F §8 Q1：`probe.sh` 在 5 个环境跑出可比 JSON |
| P3 | **Shell 通用性** | bash/zsh/fish 各自独立适配，各自声明能力，没测过的写 `unknown`，不假装能观察 | ADR 0004 | 0-A：13 用例 × 3 shell = 39/39；三种 shell 需要三套机制，hook body 成本 bash 3 µs / zsh 2348 µs / fish 1085 µs；`experiments/00-a-shell-hooks/capabilities/{bash,zsh,fish}.json` 里到处是 `partial:`，且 0-A §8 明确把 `fish.dead_sink_resilience`、`fish.monotonic_clock`、`zsh.zsocket_dialling`、`*.background_completion`、`*.monotonic_clock`、`*.internal_script_commands` 降级为 `unknown`（例：`bash.json`/`fish.json` 的 `internal_script_commands` 是 `false` 这个实测值，而 `background` 是 `partial:`） |
| P4 | **终端通用性** | 不接管、不改造用户现有终端；PTY 只能开新会话，不能附着已有终端 | ADR 0004（拒绝「强制所有 shell 进受管 PTY」） | 0-D：30/30 步、49/49 断言；§5.5 接管边界：`TIOCSTI` → `EIO`，`TIOCSCTTY(force=1)` → `EPERM`，12 个外来终端 `owns_this_terminal=false` |
| P5 | **权限** | 默认零特权用户守护进程；特权 helper 独立、可选、更晚，且只能靠能力集获得权限，不用 setuid | ADR 0005 | 0-G：29/29 检查；Phase 1 需要的每一项能力在 uid 1000 下 ALLOWED，每一项 DENIED 的能力在 Phase 1 都不需要；T2：namespace root ≠ host root |
| P6 | **证据** | 每条结论带 `evidence_status`；关系带可信度等级；文件归因封顶 `Probable`；`caused_by` 在解析器与 schema 两层都无法表达 | ADR 0003（append-only）、ADR 0006（provider 强制带 caveats） | 0-H：24/24 可信度场景，`Confirmed 4 / Strong 4 / Probable 3 / Weak 6 / Unknown 7`，单调性 0 违例；`event_relations.kind` 有 `CHECK`，`caused_by` 写不进去 |

**P6 的具体封顶**（最容易被后来人当成 bug 的一条）：文件改动最高只能到 `Probable`。
`Confirmed` 要求路径绑到命令文本上，这对编译器、包管理器、`git`、`make`、测试运行器
全都做不到 —— 恰好是最在乎的那些写操作（0-H §8.2）。想给文件改动升到 `Confirmed`
的改动，是违反 P6，不是修 bug。

---

## 2. 六层架构与组件清单

层与组件的切分见 §0.3 的重建说明。状态依据见每行最后一列（都是可执行的检查）。

| 层 | 组件 | 状态 | 依据（`ls` / `grep` / 测试） |
|---|---|---|---|
| **L1 采集** | 1. shell 适配器族（bash/zsh/fish） | 产品内未开始；实验原型存在 | `ls shell/` → 不存在。原型在 `experiments/00-a-shell-hooks/adapters/{bash.sh,zsh.zsh,fish.fish}`，412 行，仅实验用 |
| L1 | 2. `li-hook`（热路径客户端） | **骨架** | `crates/li-hook/src/main.rs` 9 行，`fn main()` 空，只有 `TODO(phase1-batch3)` |
| L1 | 3. 进程观察者（procfs polling） | 未开始 | 无对应 crate；参数与损失曲线在 0-E（200 ms、3.93 % 单核、RSS 2744 kB） |
| L1 | 4. 文件观察者（inotify） | 未开始 | 同上；溢出与归因边界在 0-E B5/B8/B10 |
| L1 | 5. PTY 监督者 | 未开始 | 0-D 只有实验 crate `li-pty-proto`；仓库无 `crates/li-pty` |
| L1 | 6. 脚本监督者（`li run`） | 未开始 | 0-B 已验证机制（`setsid` + `wait4` + leader-record），无产品代码 |
| **L2 传输** | 7. `li-proto`（信封、帧、版本门、编码） | **已实现** | `cargo test -p li-proto` → 13 + 50 = **63 passed**；2806 行 |
| L2 | 8. UDS socket 层（accept / 认证 / 帧上限执行） | **骨架** | crate 存在但层本身零代码：`crates/li-daemon/src/main.rs` 9 行空 `fn main()`；`check_inbound_frame_len` 已写好但**没有任何调用者**（`grep` 只有 `li-proto` 自己的测试） |
| **L3 存储** | 9. `li-journal`（schema、迁移、单写线程、脱敏） | **已实现**（保留策略除外） | `cargo test -p li-journal` → 44 passed；把 0001+0002 灌进 `sqlite3` 得到 **12 表 / 23 显式索引 / 2 触发器** |
| **L4 关联** | 10. 去重与关联引擎 | 产品内未开始 | 指纹字段已在 schema 与 wire 里，`li-journal` 只按 `event_id` 精确去重；分级器（R0–R7）只在 0-H 原型里 |
| L4 | 11. 操作上下文与可信度分级（L0–L4 策略） | 未开始 | `event_operations` / `event_relations` / `evidence` 表存在但无写入代码（B2 §1「schema-only」）；`crates/li-policy` 不存在 |
| **L5 恢复** | 12. `li-recovery` provider 抽象与 git/file provider | 未开始 | `ls crates/li-recovery` → 不存在；spec §1 列了它，B1 §4.1 记为偏离 |
| **L6 接口** | 13. `li-cli`（`li run` / `query` / `checkpoint`） | **骨架** | `crates/li-cli/src/main.rs` 8 行，`fn main()` 空 |
| L6 | 14. 审批与策略（approvals、破坏性操作门） | 未开始 | `approvals` / `recovery_plans` / `checkpoints` 表存在，无代码写它们（B2 §13） |

合计 14 个组件：**已实现 2、骨架 3、未开始 9**。

---

## 3. 阶段总览

| 阶段 | 目标 | 当前状态 | 关键交付物 | 门禁（什么算通过） |
|---|---|---|---|---|
| **Phase 0** | 用实测确定「哪些能观察、代价多少、哪里一定会丢」 | **完成**：8/8 实验通过各自的门禁。按各实验 REPORT §7 原文：0-D 是 **PASS**，其余七个（0-A/0-B/0-C/0-E/0-F/0-G/0-H）都是 **PASS-WITH-LIMITS**。限制逐条写在 §5 | `experiments/00-a..00-h`（8 套 README + run.sh + src + results + REPORT）、6 份 ADR、`ipc-protocol-v0.md`、`security-boundary-v0.md`、Phase 1 施工图 | 每个实验 `run.sh` 退出 0 且 REPORT 八节齐全；被证伪的假设**保留在记录里**（0-E H2/H6、0-A H-A6 都不改写） |
| **Phase 1** | 核心基础设施：事件从 shell 进得来、存得住、查得出来，且丢的时候知道自己丢了 | **进行中**：`li-proto` + `li-journal` 已实现（63 + 44 = 107 条测试通过）；`li-hook` / `li-daemon` / `li-cli` 只有空 `main()`，实现未开始 | 5 个 crate 可用 + `li run`/`query`/`checkpoint` | spec §6 七条，全部是测量而非声称（详见 §4.1.4） |
| **Phase 2** | 系统观察者：进程与文件自动被观察，能力边界写进能力记录 | **未开始** | procfs poller、inotify watcher、能力记录发布、eBPF 必要性判定 | 200 ms 轮询下 CPU ≤ 5 % 单核、RSS ≤ 10 MB（本计划设的门槛；0-E A4 实测 3.93 % / 2 744 kB）；`IN_Q_OVERFLOW` 触发重扫；10 000 事件 10 分钟零无法解释缺口 |
| **Phase 3** | 深度采集：PTY 会话、脚本、条件性特权 helper | **未开始** | PTY 监督者 + terminal-query responder、`li-hook` 产品化、helper（**仅在门禁满足时**） | fish 会话启动 ≤ 200 ms（本计划设的门槛：0-D 实测有 responder 时 49 ms、无 responder 时 10 064 ms）；4 并发会话零串扰；helper 启动后 `execve` 次数 = 0 |
| **Phase 4** | 关联与上下文：把事实变成带等级的关系 | **未开始** | 两级去重、late-arrival reorderer、R0–R7 分级器、多对多操作归属 | 24/24 场景在产品代码复现、单调性 0 违例、文件归因 ≤ `Probable`、输出 "caused" 出现 0 次 |
| **Phase 5** | 恢复：检查点、检查、验证；破坏性恢复单独授权 | **未开始** | provider trait + git/file provider；btrfs/snapper/LVM 只在临时环境实测 | 每个 provider 的 `caveats` 强制存在；restore 前置 scope preview + overwrite check + 审批 + 事后验证 |
| **Phase 6** | 分发与加固：跨发行版、跨架构、特权面收口 | **未开始** | 打包、静态/动态决策、`hidepid`/musl/ARM 实测、ADR 收尾 | 每个声明支持的发行版都有一次实测安装+运行记录；未知环境按能力降级 |

---

## 4. 各阶段的详细计划

### 4.1 Phase 1 —— 核心基础设施（进行中，本阶段要格外细）

#### 4.1.1 目标

Phase 1 结束时，一个新的 bash/zsh/fish 交互 shell 可以把「每一条命令的开始/结束/
退出码/工作目录」送进一个零特权的用户守护进程，落进一个 `kill -9` 之后还能
`integrity_check=ok` 的 SQLite 文件，并且能用 `li query` 读出来，每条结论带着
`evidence_status`、每条关系带着可信度等级；守护进程挂掉时 shell 不受影响，丢掉的事件
在重启后被**数出来并报成缺口**。

之前做不到的是：**没有任何一条真实事件流**。今天 `li-proto` 的 63 条测试和
`li-journal` 的 44 条测试都在测试进程内构造数据；没有任何一个 crate 曾经从外部被调用过
（B2 §14.1）。

#### 4.1.2 工作分解

批次顺序来自 spec §7 与 B2 §14（推荐先做 `li-hook` —— C1/C10 的门禁在它身上，
而且它定义了 daemon 要解析的帧形状）。

**批次 A — 底座（已完成）**

| # | 任务 | 状态 | 验收 / 验证方式 |
|---|---|---|---|
| P1-01 | workspace + 5 个 crate 骨架；`unsafe_code = "forbid"`；`exclude = ["experiments"]` | `[已完成]` | `cargo metadata` 列出 5 个包；注入 `unsafe` 块编译失败（B1 §3 实测） |
| P1-02 | `li-proto`：事件信封、19 种事件类型、消息帧、版本门、JSON 编解码 | `[已完成]` | `cargo test -p li-proto` → 13 单元 + 50 契约 = 63 passed（本次实测） |
| P1-03 | `li-journal` schema：append-only 触发器、JSON path 表达式索引**在 0001**、外键子列全索引 | `[已完成]` | 依次灌 0001+0002 到 `sqlite3` → 12 表 / 23 显式索引 / 2 触发器（本次实测）；`tests/schema.rs` 13 passed，含「删掉 JSON 索引要挂 2 条测试」的变异验证（B2 §3a） |
| P1-04 | `li-journal` 单写线程：有界队列（1024）、批量提交（128）、watermark | `[已完成]` | `tests/concurrency.rs` 7 passed |
| P1-05 | 崩溃一致性（真实 `SIGKILL`） | `[已完成]` | `tests/crash.rs` 1 passed；B2 §8 手工复现 15 585 committed → 重开 15 617 行、连续前缀、`integrity_check` ok |
| P1-06 | 脱敏前置：`redact` 跑在 `EventRow` 存在之前 | `[已完成]` | `tests/redaction.rs` 5 passed，扫 `journal.db` **和** `-wal` 原始字节 |

**批次 B — 把已有东西接起来（全部待做）**

| # | 任务 | 状态 | 验收 / 验证方式 |
|---|---|---|---|
| P1-07 | `li-hook`：非阻塞 connect **或** `SO_SNDTIMEO ≤ 5 ms`；有界队列；drop 计数；drop 时 `sequence` 前进 | `[待做]` | C1/C2。丢 1 条，`sequence` 仍 +1，接收端算出的缺口 = 1 |
| P1-08 | `li-hook`：sink 是**启动器继承的 FD**，不是 shell 打开的 socket | `[待做]` | C10 + 0-A：三种 shell 都无法用重定向打开 AF_UNIX（ENXIO） |
| P1-09 | `li-hook`：SIGPIPE 不可达（子 shell 写、或一次性探测失败后永久静默） | `[待做]` | 0-A：裸写死管道**杀死**交互 shell（`sig 13`） |
| P1-10 | `li-hook` 的 `panic = "abort"` 机制 | `[阻塞]` | workspace profile 会作用于全部 crate；「只给 hook 用 abort」需要一种表达方式，用户定 |
| P1-11 | `li-daemon`：UDS accept、目录 0700 / socket 0600、`SO_PEERCRED` uid 白名单 | `[待做]` | 0-G T3b 双闸门：跨 uid `connect` 先 `EACCES`，放宽文件权限后仍被应用层拒 |
| P1-12 | `li-daemon`：有界 ingest → **单** SQLite writer 线程 | `[待做]` | 0-C：瓶颈是 journal flush 不是 socket；`submit` 不得触盘 |
| P1-13 | `li-daemon`：崩溃哨兵 → `DaemonRestarted`；对每个已知 session 做 `sequence` 缺口对账并记成事件 | `[待做]` | `read.rs::sequence_gaps` 已就绪（B2 §1），但**无调用者** |
| P1-14 | `li-daemon`：epoch / boot-id 对账（`ts_mono_ns` 跨启动不可比） | `[待做]` | B1 §9.3 要求 journal 把 boot id 与它配对 |
| P1-15 | 把 `check_inbound_frame_len` 接进真实 reader | `[待做]` | B1 §6：「一个没有调用者的上限不是上限」 |

**批次 C — 适配器与脚本（待做）**

| # | 任务 | 状态 | 验收 / 验证方式 |
|---|---|---|---|
| P1-16 | `shell/bash/`：`PROMPT_COMMAND` + `builtin history 1`，PS0 只做 start tick | `[待做]` | 0-A 13 用例矩阵逐条过；hook body 不得引入 `$( )`（C9） |
| P1-17 | `shell/zsh/`：`preexec`/`precmd`，含 `module_path` 修复与 10 位小数截断到 6 位 | `[待做]` | 0-A：不截断会 64 位溢出（"number truncated"） |
| P1-18 | `shell/fish/`：`fish_preexec`/`fish_postexec` | `[待做]` | 0-A §8 门禁：sink 写必须带守卫；fish 的 dead-sink 行为在 `capabilities/fish.json` 里就是 `"hook_can_kill_shell_if_sink_dies": "unknown:fish_is_not_killed_by_SIGPIPE_in_this_setup_untested"`，产品能力记录必须原样保留这条 `unknown`，不许因为「bash 那边已经解决了」就假设 fish 也解决 |
| P1-19 | 能力记录随 `SessionStarted` 发布，未测项一律 `unknown` | `[待做]` | 0-A `capabilities/*.json` 逐字段搬，`partial:` 原样保留 |
| P1-20 | `li run -- cmd`：`setsid` + leader-record-at-spawn + `wait4`；能力记录写 `internal_script_commands: false` | `[待做]` | 0-B：4 000 次命令调用产出 **2** 个事件；不做 leader-record 则每个 <3 ms 脚本报 0 进程 |
| P1-21 | 脚本观察的两层**不得合并成一个事件类型** | `[待做]` | C8 / 0-B §4.6–4.7 |

**批次 D — 查询与检查点（待做）**

| # | 任务 | 状态 | 验收 / 验证方式 |
|---|---|---|---|
| P1-22 | `li query`：每个响应带 `evidence_status`，每条关系带可信度等级 | `[待做]` | spec §6.7 |
| P1-23 | 输出层禁止 "caused" | `[待做]` | `grep -ri caused` 只允许命中文档与 0-H 的**拒绝**用例 |
| P1-24 | `li checkpoint`：只创建，不恢复 | `[待做]` | ADR 0006 |
| P1-25 | 检查点写入路径（`checkpoints` / `approvals` 目前 schema-only） | `[待做]` | B2 §13；写入必须有代码，不能靠迁移来「证明」 |

**批次 E — 收口（待做 / 阻塞）**

| # | 任务 | 状态 | 验收 / 验证方式 |
|---|---|---|---|
| P1-26 | retention / purge（ADR 0003 要求的审计式 purge） | `[待做]` | 现在 append-only 是**绝对的**，触发器连合法 purge 也挡；`grep -rn purge crates/li-journal/src/` → 0 命中 |
| P1-27 | 建不建 `li-recovery` / `li-policy` | `[阻塞]` | spec §1 列 7 个 crate，实际 5 个（B1 §4.1）；需要用户决定 |
| P1-28 | `$.new_path` 的 JSON 索引（rename 查询） | `[待做]` | 现在只有 `$.path` 一个 JSON 索引；B2 §6.2 说明为何没加，等 `li query --path` 覆盖 rename 时随 v3 迁移做 |
| P1-29 | spec §6 七条验收整体跑一遍并留下日志 | `[待做]` | 见 §4.1.4 |

#### 4.1.3 依赖关系

```
P1-02 proto ─┬─> P1-07..10 hook ──> P1-16..19 adapters ──┐
             └─> P1-11..15 daemon ──────────────────────┴─> P1-20 li run
P1-03..06 journal ──> P1-12（单写线程是 daemon 的消费端）    │
                        │                                    v
                        └────────────> P1-22..25 cli / checkpoint
P1-29 验收要求全部前置完成
```

- P1-07 必须早于 P1-11：hook 定义帧形状，daemon 按它解析（B2 §14.1）。
- P1-12 必须晚于 P1-04：`Journal::submit` 已是唯一写入口，daemon 只允许持有一个 `Journal`。
- P1-13 必须晚于 P1-07：没有真实事件流，缺口对账无法测。
- P1-26 不阻塞任何人，但**不做的后果**要记住：现在一条错误的 `DELETE` 删不掉任何行，
  连合法的保留策略也删不掉（B2 §1）。

#### 4.1.4 验收标准（Phase 1 完成 = 全部测过）

来自 spec §6，另加三条本阶段可量化的补充：

| # | 标准 | 测量方式 |
|---|---|---|
| A1 | bash/zsh/fish 对 0-A 的 13 用例矩阵各产出正确的 `CommandStarted`/`CommandFinished` | 13 × 3 = 39 条断言全绿，缺一条即不通过 |
| A2 | `li run -- cmd` 产出生命周期事件，能力记录写 `internal_script_commands: false` | 读 `SessionStarted` 的 capability 记录 |
| A3 | 会话中途杀 daemon：shell 保持可用，损失被计数并在重启后报成缺口 | 手工 `kill` + 重启，`sequence_gaps` 非空且总和 = 实际丢失数 |
| A4 | `kill -9` daemon 后 `PRAGMA integrity_check` 返回 ok | 已由 `tests/crash.rs` 覆盖 `synchronous=NORMAL`；`FULL` 与断电是**未测量** |
| A5 | 10 000 条 shell 事件跑 10 分钟，零无法解释的缺口 | 缺口 = `max(sequence)+1 - count(rows)`；每个非零缺口都要能在日志里找到原因（client drop / transport loss / daemon 停机） |
| A6 | 脱敏测试语料在 journal 里零密钥 | 扫 `.db` 与 `-wal` 原始字节，不查可查询 payload |
| A7 | 每个查询 API 返回 `evidence_status`，每条关系带可信度等级；任何响应不含 "caused" | 加一条契约测试 + `grep` |
| A8 | `cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt --check` 干净 | 现有 63 + 44 条测试保持通过 |
| A9 | 全套测试连跑 8 次无 flake | B2 §3 已对 `li-journal` 做过（8 × 44 全绿）；Phase 1 收口时对全 workspace 重做 |

#### 4.1.5 风险

| 风险 | 为什么可能翻车 | 依据 |
|---|---|---|
| **hook 的进程启动成本吃掉整个预算** | hook body 本身 bash 只要 3 µs，但 fork 一个 helper 要 600–900 µs，而 adapter 级实测是 2 760–3 539 µs/命令 | 0-A §4.1 fork cost、§4.4 三 shell 的 adapter 延迟 |
| **死 sink 杀死 shell** | 裸写死管道被实测杀死交互 shell；`trap '' PIPE` 又把 `SigIgn` 泄给子进程 | 0-A `probe-bash-sigpipe.log`、`probe-bash-trap-pipe-inherit.log` |
| **单写线程被查询或 checkpointer 拖住** | 瓶颈是 flush 不是 socket；`submit` 一旦触盘就倒灌进用户提示符 | 0-C §5、B2 §9 |
| **缺口无法解释** | client drop 与 transport loss 今天不可区分 | 0-C §8 第 7 条「Required before shipping: DropReport」；`ipc-protocol-v0.md §13.4` |
| **脱敏给出假安全感** | 位置参数形式的密钥无法识别；未列出的 payload key 不被检查 | B2 §12；0-H §7.6 |
| **没有真实用户流量** | 所有事件种群都是合成的，陷阱类别也由实验作者自己设计 | PHASE0 §5.1 |

### 4.2 Phase 2 —— 系统观察者（未开始）

**目标**：不再需要用户装 hook，守护进程自己按 200 ms 轮询 procfs 并同时挂 inotify，
把进程与文件事件记下来，同时把「哪些根本看不见」写进能力记录并在 UI 上说清楚。
之前做不到的是：没有任何自动采集，只有 shell hook 能提供事实。

阶段归属依据：0-E §8.4 已把 System Observer 的实现条件写死；eBPF 是否必要需要一次
量测 —— 「Phase 2 之前不做 eBPF」是任务书边界，**待确认**是 Phase 2 内做判定还是之后。

| # | 任务 | 状态 | 验收 |
|---|---|---|---|
| P2-01 | procfs poller，默认 200 ms 间隔；`Stat` 深度为默认、`Full` 按需（间隔取自 0-E §8.1 的推荐值，非本计划发明） | `[待做]` | 门槛：CPU ≤ 5 % 单核、RSS ≤ 10 MB、p99 扫描 ≤ 10 ms（这三个上限是本计划设的验收线；0-E A4 实测为 3.93 %、2 744 kB、7.82 ms，最差单次 11.1 ms） |
| P2-02 | 进程创建/退出事件；能力记录声明「< ~60 ms 生命周期不可见」 | `[待做]` | 0-E A1b：60 ms 丢 71.67 %、15 ms 丢 94.17 %、5 ms 丢 96.67 %、250 ms 丢 0 % |
| P2-03 | 进程身份：`(pid, starttime)` + 冲突标志；同 tick 冲突降级 `Unknown` | `[待做]` | C5；0-E A6b 20/20 同 tick 冲突；`processes.ambiguous` 已建但无写入代码 |
| P2-04 | inotify watcher：读缓冲 ≥ 64 KiB、永不阻塞在业务逻辑上；`IN_Q_OVERFLOW` = 重扫该子树 | `[待做]` | 0-E B5：50 000 文件只交付 16 385 条 + 1 个 `IN_Q_OVERFLOW`，其余不可枚举 |
| P2-05 | 新子树自动加 watch | `[待做]` | 0-E B4：未加 watch 的新目录里 3 个文件产出 **0** 事件 |
| P2-06 | 文件事件一律 `observed` 且**不归因** | `[待做]` | C4；0-E B8/B10：`struct inotify_event` 无 pid/uid 字段；非特权 fanotify 被拒 |
| P2-07 | 能力记录发布 0-E §8.2 的整张表，逐条 `partial:` | `[待做]` | 0-E §8.4 门禁条件：「publishes the table in §8.2 verbatim」 |
| P2-08 | 跨源时钟偏差实测（hook / poller / inotify 三者） | `[待做]` | 0-H §8.4 条件 2：生成器用的 0–30 ms 是猜的，指纹窗口依赖它 |
| P2-09 | 去重种群改用**真实双重仪器**重跑（hook 注册两次），不用生成器 | `[待做]` | 0-H §8.4 条件 1 |
| P2-10 | eBPF / 内核 audit 的**必要性判定**（只判定，不实现） | `[待做]` | 若判定「需要」，进入 Phase 3 的 helper 门禁讨论；判定之前不写代码 |

**依赖**：P2-01..06 依赖 Phase 1 的 daemon 与 journal；P2-08 必须早于 Phase 4 的指纹窗口定值；P2-10 必须早于任何 Phase 3 helper 设计。

**验收**：200 ms 轮询连续 60 s，CPU ≤ 5 % 单核、RSS ≤ 10 MB、`read_failures = 0`；
50 000 文件突发后事件数 + 1 个 overflow 可解释，且溢出触发重扫；
能力记录每一行与 0-E §8.2 表格逐字一致。

**风险**：轮询丢短命进程（0-E A1b —— **设计上的不可修复项**，只能声明）；
inotify 溢出不可恢复（0-E B5）；`hidepid` 主机完全未测（0-E §8.3.5、PHASE0 §5）。

### 4.3 Phase 3 —— 深度采集与条件性特权（未开始）

**目标**：能自己起受管会话并读回结果（PTY），能在受管环境里跑脚本并给出可信的生命周期，
并在**门禁满足时**才引入特权 helper。之前做不到的是：没有受管会话，也无法获得任何
跨 uid / 快照类能力。

| # | 任务 | 状态 | 验收 |
|---|---|---|---|
| P3-01 | PTY 监督者：`openpty` → `setsid` → `TIOCSCTTY` → `dup2` → `execve` | `[待做]` | 0-D 30/30 步；3 shell × 10 步 |
| P3-02 | terminal-query responder 作为一等组件，覆盖 DSR / DA1 / kitty-keyboard / `OSC 10/11;?` / `ESC[18t` / XTGETTCAP，并记录答了什么 | `[待做]` | 0-D §4.7：无 responder 时 fish 启动 **10 064 ms**，有 responder 时 **49 ms** |
| P3-03 | 每 shell 驱动策略：命令间清编辑缓冲、不匹配字面 prompt、只断言裸输出行 | `[待做]` | 0-D §5.2 |
| P3-04 | 明确区分「拥有 PTY」与「拥有终端访问」两个能力 | `[待做]` | 0-D §5.5：外来终端可当文件读、作为终端是惰性的 |
| P3-05 | zsh 模块路径解析失败要**大声失败**，不能静默（死 ZLE 看起来像「shell 不响应」） | `[待做]` | 0-D 门禁条件 5 |
| P3-06 | 特权 helper：**只有** 0-G §8.2 四条前置全部满足才启动 | `[待做]` | 四条：能力清单有一项证明非特权拿不到；半帧停顿已修且有连接上限；审计 sink 问题有答案；`security-boundary-v0.md §5` 约束原样接受 |
| P3-07 | helper 接口：闭集数字 opcode、`openat2` 带 `RESOLVE_BENEATH` / `RESOLVE_NO_SYMLINKS` / `RESOLVE_NO_MAGICLINKS` 三个标志、`PR_SET_NO_NEW_PRIVS`、每请求一行审计 | `[待做]` | 0-G：拒绝矩阵全拒（8 遍历 + 12 元字符 + 8 opcode 滥用 + 4 超长帧 + 1 超长参数 + 3 对照组）；启动后 `execve` **0** 次；审计 2101/2101 行稠密有序 |
| P3-08 | 半帧停顿缺陷修复 + 连接/worker 上限 | `[待做]` | 0-G T7：无换行的帧把该连接卡在 `read()` 1500 ms；`security-boundary-v0.md §6.2` 记为 defect open |
| P3-09 | 审计 sink 防篡改性判定（或明确放弃声明） | `[待做]` | PHASE0 §5 末条：`0600` 只防别人不防 owner；不声称 tamper-evidence |

**依赖**：P3-02 早于 P3-03；P3-06 的前置是 P3-08 + P3-09 + P2-10 的判定；P3-01 可与 Phase 2 并行。

**验收**：4 个并发受管会话零串扰；`^C` → 退出码 130；resize 后 `stty size` 反映新几何；
helper 编译产物里不存在任何能到达 `exec` 的调用（`strace` 证明 + 源码审计）。

**风险**：PTY 只能开新会话，永远不能观察已有终端（0-D §5.5，**能力边界不是缺陷**）；
特权面一旦引入，`0600` 与审计不代表防篡改；musl/ARM 完全未测。

### 4.4 Phase 4 —— 关联与操作上下文（未开始）

**目标**：把互不相关的事件变成带等级的关系和操作上下文，并且在证据不够时输出
`Unknown` 而不是猜。之前做不到的是：只有事实，没有关系，也没有「同一个操作」这个概念。

| # | 任务 | 状态 | 验收 |
|---|---|---|---|
| P4-01 | 两级去重：`event_id` 决定存储，指纹只附加佐证，**永不删行** | `[待做]` | 0-H §8.1.1；0-H §4.1：精确 id 精确率 1.0000、召回 **0.5000** |
| P4-02 | 指纹键含 `session_id` 与 `pid:starttime` | `[待做]` | 0-H §8.1.2：丢任一项损失该类 50/50 可判定负例（D2、D5） |
| P4-03 | Reorderer：按**时钟**冲刷，grace = 4 × 实测 p99 重排延迟（此处 1 ms）；`LateArrival` 是一等结果 | `[待做]` | 0-H §4.2：grace 1000 µs → 0 强制缺口、0 late、agreement 1.0000；低于重排延迟则转成 `LateArrival`，仍 0 丢失 |
| P4-04 | 跨源排序存储 ±1 tick（10 ms）不确定性 | `[待做]` | 0-H §8.1.4；`li-proto` 已有 `CROSS_SOURCE_UNCERTAINTY_NS = 10_000_000` |
| P4-05 | `relation.kind` 只允许 `observed_within` / `spawned_by` / `modified_during`；解析器与 schema 双层拒绝 `caused_by` | schema 侧 `[已完成]`；解析器侧 `[待做]` | 0-H §4.5：`caused_by`/`causes`/`led_to`/`result_of` 全拒，`INSERT` 失败有测试 |
| P4-06 | 可信度分级 R0–R7，按顺序首个匹配生效；R0（身份冲突）优先于一切佐证 | `[待做]` | 0-H §4.4：24/24 场景、单调性 0 违例 |
| P4-07 | 「独立性」落到代码：`SourceClass` 不同 + 未标记 derived + 组合在白名单内 | `[待做]` | 0-H §4.4：两个 bash hook 触发两次是**一个**来源（S12/S13） |
| P4-08 | 操作归属用多对多 `event_operations`，每边带 `level` / `attached_by` / `detached_at` | 表 `[已完成]`，写入 `[待做]` | 0-H §8.3.1：一个事件不可能在两个等级的两个操作里 —— 单列表达不了 |
| P4-09 | 文件归因封顶 `Probable`；本可 `Confirmed` 的必须以 `Unknown` + 原因 `path_not_tied` 输出 | `[待做]` | 0-H §8.2 |
| P4-10 | 渲染按 kind 封顶而非按 level：只有 `spawned_by` 能渲染 "was spawned by" | `[待做]` | 0-H §4.5 |

**依赖**：P4-02/03 依赖 P2-08 的偏差实测；P4-06 依赖 P4-07。

**验收**：24 个场景在产品代码里逐条复现（等级**和**规则编号都要断言）；单调性 0 违例；
查询延迟 q1/q2/q3 各 ≤ 10 ms（0-H §7.4 实测 76.3 / 144.2 / 32.7 µs，100k 行）；
任何输出路径 "caused" 出现 0 次；不存在指向上层等级的 `Confirmed` 文件归因。

**风险**：D1 类（同一命令、同一会话、窗口内）在**所有**被测配置下都不可判定，50/50 会合并 ——
已知残留，不是待修 bug（0-H §6.3）；指纹窗口宽度由跨源偏差决定，而偏差至今是猜的。

### 4.5 Phase 5 —— 恢复（未开始）

**目标**：能创建检查点、检查它是什么、验证它还能用；破坏性恢复本身单独授权、只在临时环境
执行。之前做不到的是：只有一个空的 `checkpoints` 表。

| # | 任务 | 状态 | 验收 |
|---|---|---|---|
| P5-01 | `li-recovery`：provider trait `capabilities` / `create_checkpoint` / `inspect` / `restore` / `verify`，**强制** `caveats` | `[待做]` | ADR 0006；没有 `caveats` 的 provider 不得注册 |
| P5-02 | file provider（文件级备份） | `[待做]` | `caveats` 必须写「不是文件系统快照：没有原子时间点，没有打开文件一致性」 |
| P5-03 | git provider | `[待做]` | `caveats` 必须写「只恢复被跟踪文件，且只到某个 commit」 |
| P5-04 | btrfs / snapper provider | `[待做]` | 必须在**真实** btrfs 子卷上实测；0-F L5：本项目的快照/回滚从未被执行过 |
| P5-05 | LVM provider | `[待做]` | `caveats` 必须写「占空间、拖慢原卷写性能」 |
| P5-06 | restore 流程：scope preview → overwrite check → 明确审批 → 事后验证 → 失败路径记录 | `[待做]` | ADR 0006；`approvals` 表已存在，无代码 |
| P5-07 | 所有实验只在 scratch 子卷/目录，绝不在真实系统执行破坏性恢复 | `[待做]` | 见 §8 判断点 D2 |

**依赖**：P5-01 早于全部 provider；P5-06 早于任何 `restore` 可用；P5-04/05 需要一次真实环境准备（未测量）。

**验收**：每个 provider 的 `capabilities` 含非空 `caveats`（缺一条即失败）；
在 scratch 环境里「创建 → 修改 → 恢复 → 验证」全流程通过，且恢复后 `verify` 报出的
文件清单与恢复前 preview 完全一致；一次故意失败的 restore 留下可读的失败记录。

**风险**：这是全项目**验证最少**的区域 —— 0-F 只发现了状态，从未恢复任何东西
（PHASE0 §5.3）；真实 btrfs/snapper/LVM 在测试容器里全都 ABSENT（0-F L6）。

### 4.6 Phase 6 —— 分发与加固（未开始）

**目标**：能让别人装上、在多种发行版/架构上跑起来，并且把「哪些主机会降级」说清楚。

| # | 任务 | 状态 | 验收 |
|---|---|---|---|
| P6-01 | 静态 vs 动态链接的最终决策 | `[待做]` | 0-F H4：Arch glibc 2.44 编出的动态二进制在 2.39/2.41 上**跑不起来**（`hello_newer` 需 GLIBC_2.43，三台全 FAILS），静态版全绿 |
| P6-02 | 能力探测作为 provider 选择的唯一机制 | `[待做]` | 0-F §8 Q1 六条条件；严格 POSIX：不用 `local` / `[[ ]]` / `source` / 数组 / `echo -e` / `set -o pipefail` |
| P6-03 | 不用 `read < procfile`，一律 `cat` | `[待做]` | 0-F：dash 下 procfs 读被截断成 1 字节（`4194304` → `4`） |
| P6-04 | init 判定只认 `/proc/1/comm == systemd` | `[待做]` | 0-F：`systemctl` 存在 ≠ 是 init（Arch 容器 `is-system-running` 返回 `offline`） |
| P6-05 | ARM64 / musl(Alpine) / NixOS 实测 | `[待做]` | 0-F L11：全部 `NOT TESTED`；Alpine 根本没有 glibc |
| P6-06 | `hidepid` 主机实测 | `[待做]` | 0-E §8.3.5、PHASE0 §5：`NOT TESTED` |
| P6-07 | `nofile` 上限下的并发实测 | `[待做]` | 0-C §13 第 6 条：50 客户端够用，1 000 并发未测；测试机 `nofile` 1024 |
| P6-08 | 打包（发行版包 / 单二进制）与 systemd user unit | `[待做]` | 未测量：没有任何打包工作开始 |
| P6-09 | 六份 ADR 状态收尾（`proposed` → `accepted`） | `[待做]` | `docs/adr/README.md` 规定需 Phase 0 报告支持；报告已存在，转换需用户拍板（D5） |

**验收**：每个声明支持的发行版都有一次实测「安装 → 起服务 → 采到事件 → 查询」记录；
未知环境按能力探测降级而不是崩溃；能力记录每项都在**该主机上**探测得出，不是默认值。

**风险**：容器里测出的内核数字（`pid_max` 4194304、inotify 16384 等）是**本机内核**属性，
不是发行版默认值（0-F L3）；minimal 镜像里恢复/诊断工具全缺（0-F L6），真机上是否存在未知。

---

## 5. Phase 0 留下来的硬约束清单

这是全项目最值钱的部分：十二条花钱买来的教训。后面每个阶段都受它们约束，违反它们不是
「权衡」，是重犯已知错误。

### 5.1 C1–C12

来源：`docs/spec/phase1-implementation-spec.md §0`。**每条都回原始实验日志核对过**：
能对上原文的照抄数字，对不上的已按测量值改写并把差异记在 §10（例如 C9 的 190 µs）。

| # | 约束（可执行的规定） | 实测依据 | 落地位置 | 状态 |
|---|---|---|---|---|
| **C1** | 每个 IPC 客户端连接非阻塞，**或** `SO_SNDTIMEO ≤ 5 ms`；队列有界；丢弃要计数；丢弃时 `sequence` 前进 | 0-C T4：对 SIGSTOP 的 daemon 做普通阻塞写**永不返回**（>2 s） | `li-hook`；协议 §6/§10 | 未落地 |
| **C2** | 逐事件 `Ack` 不是投递证明；投递只由 watermark / 计数证明 | 0-C T1：50 客户端突发下 **2 684 / 22 390** 个 ack 被丢（12.0 %） | `li-journal::Watermark`（读回 `max(ingest_seq)`） | **已落地** |
| **C3** | `event_id` 是唯一去重键；指纹只能附加佐证，永不移除数据 | 0-C T7：21/21 重发在 `event_id` 上去重；0-H §4.1：精确 id 精确率 1.0000 / 召回 0.5000 | `events.event_id UNIQUE` + `duplicate_of`；`li-proto` 指纹为 `Option` | **存储层已落地** |
| **C4** | 文件与进程事件都只是 `observed`，**不归因** | 0-E B8/B10：inotify ABI 不带 pid/uid；非特权 fanotify 返回 EPERM | schema 无「谁改的」列；Phase 2 观察者 | schema 已符合 |
| **C5** | 进程身份 = `(pid, starttime)` **加**冲突标志；同 tick 冲突把关系降级为 `Unknown` | 0-E A6b：`CLK_TCK=100` 下 20/20 同 tick 冲突 | `processes.incarnation_seq` + `ambiguous` | schema 已符合，无写入代码 |
| **C6** | ~60 ms 以下的进程创建靠轮询不可见，**能力记录必须这么写** | 0-E A1b：60 ms 丢 71.67 %，5 ms 丢 96.67 % | Phase 2 能力记录 | 未落地 |
| **C7** | inotify 溢出是数据丢失，不是小故障；读端无条件排空，永不阻塞于业务逻辑 | 0-E B5：50 000+ 文件只交付 16 385 条，一个 `IN_Q_OVERFLOW`，无法枚举丢失了什么 | Phase 2 观察者 | 未落地 |
| **C8** | 脚本观察分两层且**永不合并成一个事件类型**：生命周期（可信）vs 内部命令（不可用） | 0-B §4.6–4.7：4 000 次命令调用只产出 **2** 个事件 | `li run` 能力记录 `internal_script_commands: false` | 未落地 |
| **C9** | shell hook 不得每条命令 fork | 0-A `probe-fork-cost.log`：参数展开 1 µs、`builtin history 1 >/dev/null` 3–7 µs、任何 `$( )` bash 272 µs / zsh 287–353 µs；外部时钟交叉验证 bash 237 µs / zsh 257 µs | 三个适配器 | 未落地 |
| **C10** | hook 永不写向读者已消失的管道 | 0-A：裸写死管道**杀死**交互 shell（`sig 13`）；`trap '' PIPE` 会把 `SigIgn` 泄给子进程（`0x0 → 0x1000`） | `li-hook` + 适配器 | 未落地 |
| **C11** | Phase 1 是零特权用户守护进程：不 setuid、不 root、不 CAP_* | 0-G §8.1：29/29 检查；Phase 1 需要的每项能力在 uid 1000 下 ALLOWED；T2 证明 namespace root ≠ host root（`CapEff 000001ffffffffff` 仍 DENIED `/etc/shadow`） | 全项目 | **已遵守** |
| **C12** | provider 由**能力探测**选择，绝不由发行版名推断 | 0-F：`/bin/sh` 在 Debian/Ubuntu 是 dash、Fedora/Arch 是 bash；glibc 2.39–2.44 | Phase 2 + Phase 6 | 未落地 |

### 5.2 PHASE0 §5 已知问题（原报告十项；编号「7」出现两次是原报告的编号缺陷，此处按位置列）

| # | 问题 | 在哪个阶段解决 |
|---|---|---|
| 1 | 从未对真实用户 shell 流量测过；0-H 所有事件种群都是合成的，陷阱类别由实验作者设计 | Phase 2（P2-09 真实双重仪器重跑）。**部分永不解决**：需要真实用户历史，而读用户历史文件越界 |
| 2 | zsh 从未作为发行版包测过，所有结果依赖 `ZDOTDIR` shim | Phase 3（P3-05）/ Phase 6（打包实测） |
| 3 | 恢复是全项目验证最少的部分；0-F 只发现状态，从未恢复任何东西 | Phase 5（P5-04） |
| 4 | 规模上限是 100 k 行，1 M 未测 | Phase 2（真实负载下重测） |
| 5 | `results/hexp-full-20261123T141333Z.log` 是陈旧产物，与报告不一致；故意未删，已标 stale | **不解决**（删证据违反项目规则）；0-H 报告没有任何数字来自它 |
| 6 | 密钥可链接性用 64 位 FNV-1a，可链接、非抗碰撞，只够回答「同一个值又出现了」 | **不解决**（设计要求如此）；Phase 4 之前不得把它当哈希用 |
| 7a | `hidepid` 主机、musl/Alpine、ARM、nushell 全部 `NOT TESTED` | Phase 6（P6-05/P6-06）；nushell 不在计划内 |
| 7b | `(pid, starttime)` 别名本身是**合成**的；真机只证明了前提（40 并发 fork 只产生 2 个不同 starttime），强制真别名需写 `kernel.pid_max` | Phase 6（若获得特权环境），否则**永久** `NOT TESTED` |
| 8 | helper 帧的半帧停顿（0-G T7）已复现并记录但**未修复**：无换行帧把该连接卡在 `read()` 1500 ms，新连接不受影响 | Phase 3（P3-08）—— 且是 P3-06 helper 门禁的前置 |
| 9 | 审计日志的 `0600` 只防其他用户，不防 owner；任何防篡改声明都不成立 | Phase 3（P3-09 给出答案或明确放弃） |

### 5.3 PHASE0 §6 安全风险 → 处置阶段

| 风险 | Phase 0 状态 | 现在是否仍然如此 | 处置阶段 |
|---|---|---|---|
| 卡死的 daemon 挂住每个 shell | 设计上已缓解：写超时 + 继承 FD，死 sink 与活 sink 同价（bash 1148 µs vs 1140 µs） | **仍未实现**（`li-hook` 是空 `main`） | Phase 1（P1-07..09） |
| 死 sink 通过 SIGPIPE 杀死 shell | 已缓解（子 shell 写）；`trap '' PIPE` 被拒，因为会把忽略标志泄给子进程 | 缓解方案在 0-A 原型里，产品里未落地 | Phase 1（P1-09） |
| `SO_PEERCRED` 的 pid 在客户端退出后可被复用 | 已证实（冻结快照、僵尸仍报 pid、冒充者接管同一 pid） | 约定已写进 `security-boundary-v0.md §2.2`，无代码 | Phase 1（P1-11） |
| `(pid, starttime)` 在同一 tick 内碰撞 | 已证实 20/20 | 缓解是「降级到 `Unknown` 而不是猜」；schema 支持，无代码 | Phase 2（P2-03） |
| 通过 helper 提权 | 原型里已封死：闭集 opcode、`openat2(RESOLVE_BENEATH)`、crate 内无 exec 可达调用（`grep` 验证）、10/10 注入被拒 | helper 尚不存在 | Phase 3（P3-06/07） |
| 密钥进入 journal | 三种被测形状已缓解（0 漏、53 条里 1 个误报 → V2 后 0 误报 / 0 漏）；位置参数形式的密钥不可检测 | **已落地**（B2 §12：0/21 clean、0/7 secrets，含 WAL 字节扫描） | Phase 1 已完成；边界永久 |
| inotify 溢出静默丢事件 | 本层无法缓解，声明为 `partial:IN_Q_OVERFLOW is data loss` | 未实现 | Phase 2（P2-04） |
| 把文件改动错误归因到进程 | 此层不可能做对：inotify 无 pid，非特权 fanotify 被拒；文件归因封顶 `Probable` | schema 已禁 `caused_by`；分级器无代码 | Phase 4（P4-09） |

---

## 6. 明确不做的事

这一节存在的目的是**防止后来人把边界内的东西当 bug 修**。每一条都不是待办。

| # | 不做 | 为什么（依据） | 什么时候会有人以为它是 bug |
|---|---|---|---|
| N1 | **不把时间关联说成因果** | 0-H §4.5 四个反例：后台任务活过命令、延迟回写、编辑器 rename-save、两写者一文件。`caused_by` / `causes` / `led_to` / `result_of` 在解析器和 schema 两层被拒 | 看到 `Probable` 想「升到 Confirmed」 |
| N2 | **不声称 inotify 能提供完整审计** | 0-E B5：溢出后只给一个 `IN_Q_OVERFLOW`，丢掉的不可枚举；B4：未加 watch 的子树是 0 事件 | 以为「有 inotify 就等于有审计」 |
| N3 | **不把文件级备份说成文件系统快照** | ADR 0006：没有原子时间点，没有打开文件一致性 | provider 的 `caveats` 被当成免责声明而删除 |
| N4 | **不假设 Git 能恢复所有工作区文件** | ADR 0006：只恢复被跟踪文件，且只到某个 commit | untracked 文件丢失后被当成恢复失败 |
| N5 | **不声称 PTY 能观察所有现有终端会话** | 0-D §5.5：`TIOCSTI` → `EIO`，`TIOCSCTTY(force=1)` → `EPERM`；外来终端可当文件读、作为终端是惰性的 | 想「接管用户已开的终端」 |
| N6 | **不假设 Nushell 的 REPL hook 能观察脚本内部每条命令** | 0-B §4.6–4.7：4 000 次调用 = 2 个事件；`nu` 在本机未安装（`NOT TESTED`） | 期待脚本内逐命令可见 |
| N7 | **Phase 2 之前不做 eBPF、不做内核 audit** | 0-E §8.3：归因与有序无损的进程事件需要 `CAP_BPF`/`CAP_PERFMON` 或 root 装审计规则；任务书边界 | 想「加个 eBPF 就解决了」——先过 Phase 3 的 helper 门禁 |
| N8 | **核心层不出现任何模型 / Prompt / API Key** | P1；`crates/` 依赖只有 serde / serde_json / rusqlite；`api_key` 只出现在脱敏规则与测试字符串 | 想在 daemon 里加「智能归因」 |
| N9 | **Phase 1 不做破坏性恢复** | ADR 0006；0-F 从未创建或回滚一次快照 | 想让 `li checkpoint` 顺手带 restore |
| N10 | **不承诺屏幕内容** | 0-D 门禁条件 6：Phase 1 能承诺的是会话控制、退出码、输出字节、resize、以及「某个全屏程序跑过」 | 想看「终端里显示了什么」 |
| N11 | **不承诺来自 shell hook 的单调时钟** | 0-A 门禁条件 3：让 daemon 收到时盖 `ts_mono_ns`，shell 侧时间标 `declared` | 想用 hook 的时间戳做精确排序 |
| N12 | **跨源不承诺优于一个 tick 的顺序** | 0-H §8.1.4：跨源排序必须带 ±1 tick（10 ms）不确定性 | 想「用纳秒时间戳排个准序」 |

---

## 7. 每阶段的报告格式（任务书 §十九，八节，原样保留）

阶段与批次报告**必须**是这八节，顺序不变，节标题不改。`docs/reports/PHASE0-REPORT.md`
与两份批次报告都按这个格式写。

| 节 | 标题 | 必须回答的问题 |
|---|---|---|
| 1 | **What was actually completed** | 实际做成了什么，每项附交付物路径。没做的**单列**，不混进「已完成」 |
| 2 | **Files changed** | 表格：路径 / 用途。另加**行数预算**：本批次代码行数与上限，超了就写明超了多少、超在哪 |
| 3 | **Test results** | 实际命令 + 实际输出。要贴 `cargo test` 的 passed 数、连跑次数（防 flake），以及「测试能不能失败」的验证（变异测试 / 回读断言值） |
| 4 | **Architecture changes / deviations** | 相对 spec 的偏离：逐条说明是有意决策还是短板；有意决策给理由，短板直说是短板 |
| 5 | **Known problems** | 未解决的问题，按重要性排序。**不许隐藏**，不许用 TODO 代替描述 |
| 6 | **Security risks** | 表格：风险 / 状态（mitigated / demonstrated / open / out of scope）+ 一句依据 |
| 7 | **Next phase plan** | 下一步做什么，按依赖排序，并说明为什么是这个顺序 |
| 8 | **Git status** | 分支、HEAD、未提交变更。**未提交就必须写「未提交」** |

附加写作纪律（源自 `docs/00-phase0-protocol.md §0`，对阶段报告同样有效）：

1. 没跑过的命令不能写成结果；没测过的数字写 `NOT MEASURED` / `NOT TESTED`。
2. 失败的假设是有效结果，**原样保留**，不改写成通过。
3. 每个数字必须能追到 `results/` 下的一个文件或一次可复现的命令。
4. 两种格式不要混用：**实验**报告用 `00-phase0-protocol.md §1` 的八节
   （Objective / Hypothesis / Method / Raw results / Analysis / Known limitations /
   Conclusion / Gate）；**阶段与批次**报告用上表的八节。

---

## 8. 里程碑与判断点

### 8.1 必须停下来问用户（不得自主推进）

| # | 判断点 | 为什么必须问 | 触发时机 |
|---|---|---|---|
| D1 | **特权 helper 上线** | 它把威胁模型从「同 uid 互不信任」变成「有一个带权限的进程」；`security-boundary-v0.md §6.3` 列了四条前置，即使全满足也要用户批准 | P3-06 之前 |
| D2 | **任何真实系统上的破坏性恢复** | 不可逆。恢复只能在 scratch 子卷/目录做；真实系统上的一次 `restore` 必须用户逐次明确授权 | P5-06 之后任何一次真实操作 |
| D3 | **跳过或重排阶段** | 门禁是按依赖设计的（helper 在 Phase 3、恢复在 Phase 5、eBPF 判定在 Phase 2）；跳过等于拿没测过的东西当地基 | 任何阶段启动前 |
| D4 | **单批次行数预算** | 两次批次都超了 1 200 行上限（B1 §2a：1 883 code lines；B2 §2a：3 162 code lines），两次都是事后才发现 | 每次派活前定，超了停下问 |
| D5 | **六份 ADR 转 `accepted`** | `docs/adr/README.md` 规定需 Phase 0 报告支持；报告已存在，但状态词是架构承诺，不应由执行者单方面改 | Phase 1 收口时 |
| D6 | **是否新建 `li-recovery` / `li-policy`** | spec §1 列 7 个 crate，实际 5 个；建了要维护，不建要改 spec | P1-27 |
| D7 | **脱敏 key 列表扩展** | 现在只检查 6 个 payload key；加 key 会改变已存数据的可读性 | 每次新增事件类型时 |
| D8 | **`synchronous=FULL` vs `NORMAL`** | 现在所有崩溃结论都是 `NORMAL` 的；`FULL` 会同时改吞吐与崩溃边界 | Phase 1 收口时（0-H §8.4 条件 4） |

### 8.2 可以自主推进（不需要每次确认）

| 可以做 | 条件 |
|---|---|
| 补测试、修实现缺陷、改文档与错字 | 不改变任何对外承诺 |
| 在已批准阶段内按依赖顺序做任务 | 不新增 crate、不新增依赖、不改协议 |
| 把实测数字写进代码注释和 schema | 数字必须来自 `results/` 里的文件 |
| 重跑已有实验与已有测试 | 不改实验结果，只加新的一次运行记录 |
| 在 scratch 环境（`/tmp/li-*`、临时子卷、rootless podman）里做任何实验 | 不触碰宿主 rc 文件、不装系统包、不用 sudo |

### 8.3 不许进入下一阶段的条件

| 阶段 | 不满足就不许往下 |
|---|---|
| Phase 1 → 2 | spec §6 七条全测过（§4.1.4 A1–A9） |
| Phase 2 → 3 | 能力记录逐字符合 0-E §8.2；`IN_Q_OVERFLOW` 有重扫路径 |
| Phase 3 → 4 | helper 若上线，启动后 `execve` = 0 且拒绝矩阵 100 %；半帧停顿已修 |
| Phase 4 → 5 | 24/24 场景在产品代码复现；文件归因 ≤ `Probable` |
| Phase 5 → 6 | 每个 provider 都有非空 `caveats`；scratch 全流程验证通过 |

---

## 9. 本文件的验证记录（2026-11-24）

写这份文档时对仓库实际执行的检查，用来支撑上面每一个「已完成」：

| 检查 | 命令 | 结果 |
|---|---|---|
| 提交历史与工作区 | `git log --oneline` / `git status --short` | 23 个提交，HEAD `619f14e`；**开工时**工作区干净，本次只新增/修改文档（见 §10 第 1、2 项） |
| 五个 crate 是否存在 | `find crates -name '*.rs' -o -name '*.sql'` 后 `wc -l` | `li-proto` 2806 行、`li-journal` 4128 行；`li-hook` 9 行、`li-daemon` 9 行、`li-cli` 8 行 |
| 三个骨架是否真为空 | `cat crates/{li-hook,li-daemon,li-cli}/src/main.rs` | 三个都是空 `fn main()`，只有 `TODO(phase1-batchN)` 注释 |
| `li-proto` 测试 | `cargo test -p li-proto` | 13 unit + 50 contract + 0 doc = **63 passed** |
| `li-journal` 测试 | `cargo test -p li-journal` | 11 + 7 + 1 + 7 + 5 + 13 = **44 passed** |
| schema 真实规模 | `sqlite3 v.db < 0001_init.sql` 后灌 `0002`，查 `sqlite_master` | **12 表 / 23 显式索引 / 2 触发器** |
| 未建的两个 crate | `ls crates/li-recovery crates/li-policy` | 都不存在 |
| shell 适配器目录 | `ls shell/` | 不存在 |
| 保留策略 | `grep -rn purge crates/li-journal/src/` | 0 命中 |
| 帧上限是否接线 | `grep -rn check_inbound_frame_len crates/` | 只有 `li-proto` 定义与自身测试，**无调用者** |
| 独立性 | `grep -rni -e api.key -e openai -e anthropic -e prompt -e llm crates/` | 只命中 `li-journal/src/redact.rs` 的脱敏规则与测试字符串 |
| 依赖面 | `grep -A3 '[dependencies]' crates/*/Cargo.toml` | `serde`、`serde_json`、`rusqlite`(bundled)，无其他 |

---

## 10. 核对中发现的不符（除第 1、2 项外未改，只记录）

| # | 不符 | 实际 |
|---|---|---|
| 1 | `README.md` 的 crate 树把 `li-journal` 标为 ⬜，「现在到哪一步」只说完成 `li-proto` | `li-journal` 已实现并通过 44 条测试。**本次一并修正**（纯文档） |
| 2 | `docs/spec/phase1-implementation-spec.md` 标 `draft` 且含 4 处 `[pending 0-X]` | Phase 0 已全部完成。**本次填掉并转 `accepted`** |
| 3 | `PHASE1-BATCH2-REPORT.md` §1/§2 写「11 real tables, 14 indexes」 | 实测 **12 表 / 23 显式索引** |
| 4 | `PHASE0-REPORT.md §7` 写「169× 索引收益」 | 0-H §7.4 实测 2664 µs → 16.6 µs = **160×**；0-H 自己两处都写 160×（B2 §6.1 也已指出） |
| 5 | `docs/reports/00-phase0-index.md` 写 0-G 是「28/28 checks」 | 0-G REPORT 与 PHASE0-REPORT 都写 **29/29** |
| 6 | spec C9 写「`$( )` 约 190 µs」 | 0-A 测量行是 272 µs（bash）/ 287–353 µs（zsh），外部时钟交叉验证 237/257 µs；190 只出现在日志结尾的结论文字里 |
| 7 | `PHASE0-REPORT.md §8` 写「11 commits，`afa8d77` 最新」 | 现在 23 commits、HEAD `619f14e`（历史事实，不必改，但别当现状引用） |
| 8 | `PHASE0-REPORT.md §5` 的编号「7」出现两次（十项问题只有九个编号） | 内容齐全，编号重复 |
| 9 | 六份 ADR 全部仍是 `proposed` | 支持它们的 Phase 0 报告已存在，按 `adr/README.md` 规则已可转 `accepted`，但需用户拍板（D5） |
| 10 | `docs/ipc-protocol-v0.md` 也标 `draft` | 不在本次授权范围，未改；建议与 ADR 一起收口 |
| 11 | spec §1 列 7 个 crate，实际 5 个 | B1 §4.1 已记为偏离；是否现在建见 D6 |
| 12 | `li-journal` 的持久性数字引用 `bundled` SQLite，而 0-H 用系统 3.53.4 | B2 §5.9 已记：索引资格结论可复现，但 0-H 的**耗时**不保证迁移 |
| 13 | `PHASE0-REPORT.md §1` 的结论表把 0-C、0-G、0-H 都记为 `PASS` | 三份实验自己的 REPORT §7 都写 **PASS-WITH-LIMITS**：0-C（突发下 p99 是 34–37 ms 不是 <5 ms；重连 1.631 s）、0-G（从未以真实权限运行过 helper；`0600` 不防 owner；半帧停顿是可用性缺陷）、0-H（重排只在会话内可定义；`(pid,starttime)` 别名那半是合成的）。本文件 §3 按实验原文记 |
