# wx-cli

WeChat macOS 数据库解密与查询工具。支持通过 `key scan`（Mach VM 内存扫描）或 `key extract`（LLDB hook）提取密钥，解密并查询 WeChat 4.1.7.x / 4.1.8.x 的 Apple SEE 加密 SQLite 数据库。

## 支持范围

- **平台**：macOS（arm64 / Apple Silicon）
- **WeChat 版本**：4.1.7.x / 4.1.8.x

## 前置条件

密钥提取**需要 SIP 关闭**（SIP enabled 时 `task_for_pid` 被内核拒绝，即使 root 也不行）。如果你已有密钥，可以跳过 SIP 要求，直接用 `key set` 手动录入。

| 条件 | key scan | key extract |
|------|----------|-------------|
| SIP disabled + sudo | **可用** | **可用，但通常不需要 sudo** |
| SIP disabled + 无 sudo | 不可用 | **可用（推荐）** |
| SIP enabled | 不可用 | 不可用 |

`key extract`（LLDB 方式）还需要：

1. `sudo DevToolsSecurity -enable`
2. `sudo dscl . append /Groups/_developer GroupMembership $USER`
3. `xcode-select --install`（提供 `lldb` 和 `python3`）

## 安装

### 从 Release 下载（推荐）

前往 [Releases](https://github.com/pandorafuture/wx-cli/releases/latest) 下载预编译二进制（macOS arm64），或使用命令行：

```bash
# 下载最新 release
curl -fSL "$(curl -fsSL https://api.github.com/repos/pandorafuture/wx-cli/releases/latest \
  | grep -o '"browser_download_url": "[^"]*macos-arm64[^"]*"' \
  | cut -d'"' -f4)" -o wx-cli.tar.gz
tar xzf wx-cli.tar.gz

# 安装到 PATH
mkdir -p ~/.local/bin
mv wx-cli ~/.local/bin/
chmod +x ~/.local/bin/wx-cli
wx-cli --version
```

### 从源码构建

```bash
# 需要 Rust 工具链（rustup 安装即可）
cargo build --release
```

编译产物位于 `target/release/wx-cli`。

### 部署二进制

```bash
mkdir -p ~/.local/bin
cp target/release/wx-cli ~/.local/bin/wx-cli
chmod +x ~/.local/bin/wx-cli

# 确保 PATH 包含该目录
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc

wx-cli --version
```

如果目标机器在远程（例如 VM），本机构建后传输即可，远程不需要 Rust 工具链：

```bash
scp target/release/wx-cli user@remote:~/.local/bin/wx-cli
```

## 使用

### 1. 检查环境

```bash
wx-cli doctor       # 检查 SIP、DevToolsSecurity、_developer 组、LLDB/python3
wx-cli status       # 查看 WeChat 运行状态和所有账号密钥/缓存状态
```

### 2. 提取密钥

```bash
# 方式 A（推荐）：内存扫描 — 不重启 WeChat，需要 sudo
sudo wx-cli key scan

# 方式 B：LLDB hook — 会重启 WeChat，通常不需要 sudo
wx-cli key extract --timeout 120

# 查看已保存的密钥
wx-cli key list
```

`key scan` 从 WeChat 进程内存中提取已缓存的数据库密钥；`key extract` 通过 LLDB hook 捕获 PBKDF2 调用获取原始密钥，覆盖范围更广。

手动设置密钥：

```bash
wx-cli key set <account> <64-hex-key>          # 数据库密钥
wx-cli key set-image <account> <image-key>     # 图片密钥
```

### 3. 解密数据库

```bash
wx-cli decrypt                # 自动解密到缓存目录
wx-cli decrypt --incremental  # 增量解密（只处理变化的文件）

# 手动指定路径和密钥
wx-cli decrypt -k <64位hex密钥> -d /path/to/xwechat_files/<account_dir> -o /tmp/decrypted
```

### 4. 查询聊天记录

```bash
wx-cli sessions --limit 10             # 最近会话
wx-cli contacts --search 张三           # 搜索联系人
wx-cli query 张三 --limit 20            # 查某人的消息
wx-cli search 周末 --limit 20           # 全局关键词搜索
wx-cli query 张三 --type text           # 按消息类型过滤
wx-cli query 周末爬山群                  # 群聊消息
wx-cli export 张三 -o /tmp/export --format json  # 导出会话
wx-cli watch --poll --poll-ms 3000      # 实时监听新消息
```

如果本机已启动 `server run` 服务，查询命令会自动复用 REST API（默认探测 `http://127.0.0.1:9100`）。可用 `--no-server` 强制本地查询，或 `--server-only` 强制远程。

### 5. 媒体解密

```bash
wx-cli decode-image input.dat -d <account_data_dir> -o output.png     # 解密图片
wx-cli decode-image /path/to/dat_dir/ -d <account_data_dir> -o /tmp/  # 批量解密
wx-cli media extract-voice --media-dir <dir> <svr_id> -o voice.mp3    # 提取语音（需 ffmpeg）
wx-cli media decrypt-video encrypted.bin --seed 2105122989 -o video.mp4  # 解密视频号视频
```

### 6. HTTP API 服务

```bash
wx-cli server run                              # 启动（默认 127.0.0.1:9100）
wx-cli server run --host 0.0.0.0 --token mysecret  # 远程访问（必须设 token）
wx-cli server status                           # 查看状态
wx-cli server stop                             # 停止
wx-cli server restart                          # 重启
```

REST 端点：`/api/v1/health`、`/api/v1/sessions`、`/api/v1/contacts`、`/api/v1/messages`、`/api/v1/search`、`/api/v1/media`、`/api/v1/events`（SSE）。

所有查询命令加 `--format json` 可获取 JSON 格式输出。

## 命令一览

| 命令 | 说明 |
|------|------|
| `wx-cli status` | 查看 WeChat 运行状态 |
| `wx-cli doctor` | 检查环境（SIP 等） |
| `sudo wx-cli key scan` | 内存扫描提取密钥（推荐） |
| `wx-cli key extract` | LLDB hook 提取密钥 |
| `wx-cli key list` | 查看已保存密钥 |
| `wx-cli key set <account> <key>` | 手动设置密钥 |
| `wx-cli decrypt` | 解密数据库 |
| `wx-cli sessions` | 最近会话列表 |
| `wx-cli contacts --search <名字>` | 搜索联系人 |
| `wx-cli query <联系人>` | 查询消息 |
| `wx-cli search <关键词>` | 全局搜索 |
| `wx-cli export <联系人>` | 导出会话 |
| `wx-cli watch` | 实时监听新消息 |
| `wx-cli decode-image <路径>` | 解密图片 |
| `wx-cli media extract-voice` | 提取语音 |
| `wx-cli media decrypt-video` | 解密视频号视频 |
| `wx-cli server run` | 启动 HTTP API 服务 |
| `wx-cli server status/stop/restart` | 管理服务 |
| `wx-cli paths` | 查看所有数据路径 |
| `wx-cli info <db>` | 查看数据库加密状态 |

## Contact Hiding

按账号隐藏指定联系人、群聊或带特定标签的联系人。启用后，查询、导出、监控和服务接口默认应用隐藏规则。

配置文件：`~/Library/Application Support/wx-cli/config/settings.toml`

```toml
[accounts."<account_id>"]
ignore_contacts = ["wxid_xxx", "12345@chatroom"]
ignore_tags = ["同事", "客户"]
```

本地命令支持 `--show-hidden` 忽略隐藏规则查看完整结果。`search` 当前不会自动应用隐藏配置。

## 文件路径

| 类别 | 路径（macOS） | 用途 | 可删除？ |
|------|---------------|------|----------|
| Config | `~/Library/Application Support/wx-cli/config/` | 密钥、设置 | 否（先备份） |
| Cache | `~/Library/Caches/wx-cli/` | 解密后数据库 | 可（重新 decrypt） |
| State | `~/Library/Application Support/wx-cli/state/` | 服务运行时元数据 | 可 |
| Logs | `~/Library/Logs/wx-cli/` | 服务日志 | 可 |
| Temp | `$TMPDIR/wx-cli/` | 密钥提取临时文件 | 可 |

使用 `wx-cli paths` 查看所有路径。清理缓存：`rm -rf ~/Library/Caches/wx-cli/`。

## 项目结构

```
wx-cli/
├── crates/
│   ├── wx-decrypt/     # 核心解密库（KDF、逐页解密、整库解密）
│   ├── wx-keychain/    # 密钥提取（LLDB / Mach VM）与本地存储
│   ├── wx-cli/         # CLI 入口
│   ├── wx-db/          # 数据库查询（联系人、消息、会话、群聊）
│   ├── wx-media/       # 媒体解密（图片、语音、视频）
│   ├── wx-monitor/     # 实时消息监听与增量监控
│   ├── wx-context/     # 账号解析、解密缓存、联系人解析
│   └── wx-paths/       # 平台路径管理
```

## 常见问题

### `key extract` 超时

- 确认 WeChat 已弹出登录界面并完成登录
- 增加超时：`--timeout 300`
- 检查日志：`$TMPDIR/wx-cli/lldb/wx_cli_lldb_output.txt`

### SIP / DevToolsSecurity 报错

密钥提取需要 SIP 关闭。重启进入恢复模式执行 `csrutil disable`，然后运行 `wx-cli doctor` 逐项检查。

### 解密后数据库无法打开

- `wx-cli key list` 确认密钥正确
- `wx-cli info <db>` 检查文件是否为加密状态
- 确认 WeChat 版本在 4.1.7.x / 4.1.8.x 范围内
