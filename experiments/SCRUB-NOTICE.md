# 证据日志的路径替换说明（SCRUB NOTICE）

本仓库准备从 private 转为 public。Phase 0 的实验日志里保留了运行时**原样观测到的字节**，
其中包含采集机器的绝对路径。这些路径会暴露本机目录结构（用户名、工作目录布局、
`~/.ssh`、`~/.config` 一类的存在性），因此在本仓库公开前做了一次**纯字符串替换**。

这份文件记录替换了什么、没替换什么、以及原始形态长什么样，任何人都可以据此把语义还原回去。

---

## 1. 替换规则

按先长后短的顺序，对全部 tracked 文件做两条字面替换：

| 原文 | 替换为 | 替换次数 |
|---|---|---|
| `<WORKSPACE>` | `<WORKSPACE>` | 2717 |
| `<HOME>` | `<HOME>` | 114 |

合计 2831 处，涉及 694 个文件。第二条只作用于第一条没吃掉的部分，所以
`/etc/passwd` 转储里的 home 字段、PTY 实验里的 `"cwd": "<HOME>/src"` 这类短形式也一并换掉了。
没有留下任何一处裸的 `<HOME>`。

两个占位符在替换**之后**的仓库里出现得比上表更多（`<WORKSPACE>` 2726、`<HOME>` 123），
差额来自本说明文件自身和各 `REPORT.md` 末尾的指引段。

原始形态就是本机上的两个真实绝对路径：一个是指向本仓库 checkout 的目录，
另一个是采集用户的 home 目录。举例来说，日志里原本写作
`<WORKSPACE>/experiments/00-b-script-lifecycle/scripts/execrepl.sh` 的字段，
现在写作 `<WORKSPACE>/experiments/00-b-script-lifecycle/scripts/execrepl.sh`；
原本写作 `<HOME>` 的 `HOME` 值，现在写作 `<HOME>`。

替换是**机械的、可逆的**：把 `<WORKSPACE>` 换回 `<WORKSPACE>`、
把 `<HOME>` 换回 `<HOME>`，每个被改动的文件都与改动前的版本**逐字节一致**。
本次提交前对全部 694 个文件做过这个往返校验，0 处不一致；每个文件的**行数也与改动前完全相同**。

## 2. 没有动什么

这一条比上面那条重要。

Phase 0 的立场是**不改写、不删除、不美化实测结果**。路径字符串不承载任何实测结论，
换掉它不影响被验证的量，所以可以替换；除此之外一律没有碰：

- **时间戳与计时**：`ts_wall`、`ts_mono_ns`、`duration_ns`、各类 `*_ns` / `*_us` / `*_ms`
- **PID / TID / starttime/ / 进程计数**：`pid`、`starttime`、`process_count`、`ppid`
- **退出码与信号**：`exit_status`、`signal`、被 kill 的 PID
- **计数与行数**：断言数、事件数、帧数、`stdout_bytes`、`stderr_bytes`、`screen_bytes`
- **字节长度与容量**：`wrote_bytes`、`requested_bytes`、`so_sndbuf_bytes`
- **事件 JSON 的结构**：字段名、字段顺序、缩进、是否带尾随换行，全部原样
- **终端 transcript 的转义序列**：`\e[m`、`\r\n` 等控制字节原样保留（CRLF 与
  `\r\n` 混排是 PTY 实验的实测现象，不是文件格式瑕疵）

换句话说：**凡是"测出来的数"一个都没改；只有"这台机器把它放在哪"被换掉了。**

## 3. 被替换的位置长什么样

替换落在三类地方，语义都不受影响：

1. **证据 JSON 的字段值**——`payload.argv[]`、`payload.cwd`、
   `child_cmdline_from_proc`、`cmdline_first`、`proc.exe` 等，值是当时从
   `/proc` 或 `execve` 真实读到的字节。这些字段证明的是"能看到什么、看不到什么"，
   不是"路径具体是哪一个"。
2. **日志与报表里的引用**——`cases.json` 里的产物路径、`00-build.log` 里的
   编译输出、`REPORT.md` 里引用的 workspace 根。
3. **harness 脚本里的默认值**——例如 `ZSH = "<WORKSPACE>/experiments/toolchain/root/usr/bin/zsh"`。
   这些本来就是**默认值**，运行时可用 `LI_ZSH` / `LI_ZSH_MODULE_PATH` 等环境变量覆盖；
   换掉默认值不改变任何已记录的测量。

## 4. 一处遗留：不是路径，但也不是秘密

`crates/li-proto/src/json.rs` 与 `crates/li-proto/tests/protocol.rs` 里的
`"cwd": "<HOME>/src"` 是**协议 golden 样本的构造值**，不是实测数据。
它被一并替换只是为了让 `git grep "<HOME>"` 的验证结果干净；
这两个文件里的断言（字段顺序、golden 串逐字节相等）在替换前后都通过
（`cargo test -p li-proto`：50 passed / 0 failed）。

## 5. 已知未覆盖：用户名与主机名仍然可读

这次只换了路径，**没有**替换非路径形式的身份信息。证据日志里仍然能看到：

- `ps`/`top` 输出里的 `USER` 列（`USER`），以及 `/etc/passwd` 转储里的 `USER:x:1000:1000::…`
- shell 提示符与终端标题里的 `USER@HOST`

这些都是**当时终端上真实出现过的字节**，是 0-A / 0-D 实验所观察对象的一部分；
替换它们会改动被观测的字节，所以按 Phase 0 的立场没有动。仓库转为 public 后，
这些信息对任何读者都是可见的。这一条是明确的取舍，不是疏漏。

## 6. 如何验证这次清洗

```sh
# 除本文件里的示例外，应当零命中（预期只输出本文件一行）
git grep -c "<HOME>" -- . | head -20

# 占位符就是这两条规则的全部痕迹
git grep -o "<WORKSPACE>" -- . | wc -l    # 2726
git grep -o "<HOME>" -- . | wc -l         # 123
```

本文件是唯一有意保留 `<HOME>` 字样的地方——它必须写出原始形态，否则"可还原"就是空话。
