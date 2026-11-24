# 证据日志的路径与身份替换说明（SCRUB NOTICE）

本仓库准备从 private 转为 public。Phase 0 的实验日志里保留了运行时**原样观测到的字节**，
其中包含采集机器的绝对路径、UNIX 用户名和主机名。这些会暴露本机目录结构与机器身份，
因此在公开前做了**两轮纯字符串替换**。第一轮只在工作树上做（未重写历史），
第二轮连同历史一起重写。

这份文件记录替换了什么、没替换什么、以及原始形态长什么样，
任何人都可以据此把语义还原回去。

---

## 1. 替换规则

按先长后短的顺序，对全部 tracked 文件做字面替换。第二轮（含历史重写）
在第一轮的基础上补了身份信息：

| 类别 | 原始形态 | 替换为 |
|---|---|---|
| 仓库根 | `/home/$USER/$REPO` | `<WORKSPACE>` |
| home 目录 | `/home/$USER` | `<HOME>` |
| passwd 行 | `$USER:x:$UID:$GID` | `USER:x:UID:GID` |
| 提示符 | `$USER@$HOST` | `USER@HOST` |
| 裸用户名 | `$USER`（`ls -l` 属主列、`ps` 的 USER 列等成词出现） | `USER` |
| 裸主机名 | `$HOST`（终端标题、OSC 0 载荷等成词出现） | `HOST` |
| 提交者姓名 | 真实姓名 | `dongfangyy` |
| 提交者邮箱 | 真实邮箱 | `132216470+dongfangyy@users.noreply.github.com` |

上表的 `$USER` / `$HOST` / `$REPO` 是占位记号，**不是**原值。
原值就是一台普通的 Arch 机器上一个 1000 号用户的 home 目录、
他对本仓库的 checkout 位置、以及他给机器起的名字。三类替换后的语义都能从
上下文推回：`<WORKSPACE>` 恒指本仓库根，`<HOME>` 恒指采集用户的 home，
`USER` / `HOST` 恒指发起实验的那个用户与那台机器。

替换是**机械的**。第一轮对全部 694 个文件做过往返校验：把两个占位符换回原值，
与改动前的版本**逐字节一致**（0 处不一致），每个文件的行数也完全相同。

## 2. 没有动什么

这一条比上面那条重要。

Phase 0 的立场是**不改写、不删除、不美化实测结果**。路径字符串与用户名不承载任何实测结论，
换掉它不影响被验证的量，所以可以替换；除此之外一律没有碰：

- **时间戳与计时**：`ts_wall`、`ts_mono_ns`、`duration_ns`、各类 `*_ns` / `*_us` / `*_ms`
- **PID / TID / starttime / 进程计数**：`pid`、`starttime`、`process_count`、`ppid`
- **退出码与信号**：`exit_status`、`signal`、被 kill 的 PID
- **计数与行数**：断言数、事件数、帧数、`stdout_bytes`、`stderr_bytes`、`screen_bytes`
- **字节长度与容量**：`wrote_bytes`、`requested_bytes`、`so_sndbuf_bytes`
- **事件 JSON 的结构**：字段名、字段顺序、缩进、是否带尾随换行，全部原样
- **终端 transcript 的转义序列**：`\e[m`、`\r\n` 等控制字节原样保留（CRLF 与
  `\r\n` 混排是 PTY 实验的实测现象，不是文件格式瑕疵）

换句话说：**凡是"测出来的数"一个都没改；只有"这台机器把它放在哪、谁来跑"被换掉了。**

## 3. 被替换的位置长什么样

替换落在四类地方，语义都不受影响：

1. **证据 JSON 的字段值**——`payload.argv[]`、`payload.cwd`、
   `child_cmdline_from_proc`、`cmdline_first`、`proc.exe` 等，值是当时从
   `/proc` 或 `execve` 真实读到的字节。这些字段证明的是"能看到什么、看不到什么"，
   不是"路径具体是哪一个"。
2. **日志与报表里的引用**——`cases.json` 里的产物路径、`00-build.log` 里的
   编译输出、`REPORT.md` 里引用的 workspace 根。
3. **harness 脚本里的默认值**——例如 `ZSH = "<WORKSPACE>/experiments/toolchain/root/usr/bin/zsh"`。
   这些本来就是**默认值**，运行时可用 `LI_ZSH` / `LI_ZSH_MODULE_PATH` 等环境变量覆盖；
   换掉默认值不改变任何已记录的测量。
4. **终端转录里的提示符与 OS 身份**——0-A 的 `*.term.log`、0-D 的 PTY 转录、
   0-G 的 `ps` 快照。这些是"当时屏幕上出现过什么"，替换只换了名字本身，
   提示符结构、转义序列、时间对齐全部保留。

## 4. 一处遗留：不是路径，但也不是秘密

`crates/li-proto/src/json.rs` 与 `crates/li-proto/tests/protocol.rs` 里的
`"cwd": "<HOME>/src"` 是**协议 golden 样本的构造值**，不是实测数据。
它被一并替换只是为了让 `git grep` 的验证结果干净；这两个文件里的断言
（字段顺序、golden 串逐字节相等）在替换前后都通过
（`cargo test -p li-proto`：50 passed / 0 failed）。

## 5. 历史也被重写了

只在工作树上替换是不够的：`git log -p` 里原本仍有数千行旧路径，仓库一旦公开，
任何人点开旧 commit 的 diff 就能看到。所以第二轮用 `git filter-repo` 重写了全部 commit，
并且**全部 commit hash 因此改变**。

同时改了提交者身份：所有 commit 的 author/committer 统一为 GitHub noreply 地址，
不再暴露真实邮箱与真实姓名。

这意味着**任何在重写之前 clone 过的副本都与本仓库不再兼容**。
本轮重写发生在仓库转为 public 之前，因此不存在需要迁移的下游。

## 6. 已知未覆盖

- **时间戳本身**：日志里的 `2026-11-23T10:58:45.120Z` 之类精确到毫秒，
  与实验者当天的操作时间一一对应。这是实测数据，不替换。
- **文件大小与构建产物的字节数**：属测量值，不替换。
- **被引用的历史 commit hash**：如果某份文档引用了重写前的 hash，
  重写后该 hash 已不存在。仓库转 public 前没有外部引用者，
  但如果你在别处留了旧 hash 的记录，需要自行核对。

## 7. 如何验证

```sh
# 除本文件外不应有裸原始路径残留（预期无输出）
git grep -n "/home/" -- . | grep -v SCRUB-NOTICE

# 提交元数据不应含真实邮箱（预期 0）
git log --format='%ae %ce' | grep -c "@163.com"

# 历史里不应有旧路径（预期 0）
git log --all -p | grep -c "/home/"
```

本文件是唯一有意描述原始形态的地方——它必须写清楚换掉了哪几类东西，
否则"可还原"就是空话。为不重新引入敏感值，这里用 `$USER` / `$HOST` / `$REPO`
这类记号代替真实值。
