# Packaging, Portability, and Capability Probing (Phase 6)

本目录汇总 `linux-intelligence` 在多发行版分发、能力探测与用户级 systemd 服务安装中的实测基线与边界说明。

## 1. 静态 vs 动态链接决策（P6-01 / 0-F H4）

| 构型 | 本机（Arch Linux x86_64 glibc 2.44）现状 | 跨发行版兼容结论 |
|---|---|---|
| 动态 glibc 构建（`cargo build --release --offline`） | **已编译并通过测试**：依赖宿主机 `glibc`（`rustc 1.99.0` 在 Arch `glibc 2.44` 上编出） | 根据 0-F H4 实测，`glibc 2.44` 上编出的动态二进制会引用 `GLIBC_2.43` 符号，在 `glibc 2.39/2.41`（Ubuntu/Debian/Fedora 较旧容器）上无法直接运行 |
| 静态 `x86_64-unknown-linux-musl` 构建 | **`NOT TESTED`**：本机仅通过发行版包安装 Rust，未安装 `rustup` 或 `x86_64-unknown-linux-musl` target | 0-F H4 已验证静态 musl 二进制在各容器镜像均无 glibc 版本依赖；正式跨发行版发布应使用 CI `musl` 静态目标 |
| ARM64 / Alpine / NixOS / `hidepid` 主机 | **`NOT TESTED`** | 严格按 `probe.sh` 返回值与能力探测降级（C12） |

## 2. POSIX 能力探测脚本（`packaging/probe.sh`，P6-02 .. P6-04）

- 严格遵守 POSIX `sh`：无 `local`、`[[ ]]`、`source`、数组、`echo -e`、`set -o pipefail`（`sh -n packaging/probe.sh` 校验通过）。
- 所有 procfs 文件（`/proc/1/comm`、`/proc/sys/kernel/pid_max`、`/proc/sys/fs/inotify/max_user_watches`）一律通过 `cat` 读取（避免 `dash` 下 `read < procfile` 截断为 1 字节的问题）。
- PID 1 判定只认 `/proc/1/comm == systemd`（不因 `systemctl` 二进制存在而误判）。

运行：

```sh
sh packaging/probe.sh
```

## 3. 安装用户级二进制定稿与 systemd user unit（P6-08）

```sh
cargo build --release --offline
install -Dm755 target/release/li ~/.local/bin/li
install -Dm755 target/release/li-daemon ~/.local/bin/li-daemon
install -Dm755 target/release/li-hook ~/.local/bin/li-hook
install -Dm755 target/release/li-helper ~/.local/bin/li-helper
install -Dm644 packaging/systemd/li-daemon.service ~/.config/systemd/user/li-daemon.service
systemctl --user daemon-reload
systemctl --user enable --now li-daemon.service
li ping
```
