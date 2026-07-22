# wx-cli

> 把微信变成 Agent 能读取、能搜索、能实时订阅的数据源。

wx-cli 直接读取 Mac 上的微信本地数据，让你和 Agent 都能访问自己的聊天记录、联系人、群聊和媒体消息。数据默认留在本机，不需要上传聊天数据库，也不依赖云端导出。

## 它能做什么

- **读取微信本地数据库**：按联系人、群聊、时间范围和消息类型查询历史消息。
- **搜索全部聊天记录**：从所有会话中查关键词，快速找回客户需求、承诺、文件和讨论结论。
- **一次读取跨会话时间线**：按时间范围取得所有会话的新消息，适合记忆补全、归档和日报任务。
- **实时订阅新消息**：用命令行持续监听，或通过 SSE 把新消息实时推给 Agent 和其他程序。
- **导出与处理内容**：把会话导出为 JSON 或文本，并读取图片、语音、视频等媒体内容。
- **让 Agent 直接使用**：项目自带 Agent Skill，Claude Code、Codex、Cursor 等工具安装后就知道怎样查询和订阅微信。
- **提供稳定的本地服务**：REST API 可供个人助理、自动化任务、工作流和多个 Agent 共同使用。
- **保护不想暴露的内容**：可隐藏指定联系人、群聊、标签或群成员，查询和订阅时自动过滤。

## 你可以基于它在微信上做什么

wx-cli 提供了最关键的两样东西：完整的历史上下文，以及持续发生的实时消息。把它接给 Agent 后，你可以基于这个项目在微信上做任何事，例如：

- 给 Agent 建立长期微信记忆，自动维护联系人画像和关系上下文；
- 从聊天里识别待办、承诺、商机、风险和需要跟进的人；
- 做个人或团队的微信搜索、知识库、CRM、客服和销售助手；
- 自动生成日报、周报、客户纪要、对账线索和项目进展；
- 监听关键词或关键联系人，在重要消息出现时触发提醒和工作流；
- 结合你已有的 Agent 操作或消息发送能力，实现自动回复、业务办理和端到端协作。

它不是只用来“导出聊天记录”的工具，而是微信之上的 Agent 能力层。

## 为什么对 Agent 友好

- 自带可直接安装的 Skill，不需要每次重新教 Agent 命令和数据格式；
- 命令行、JSON、REST API 和实时事件订阅覆盖查询与持续运行两类任务；
- 长驻服务可复用已打开的数据库，适合高频查询和定时记忆任务；
- 所有能力都以本地数据为中心，便于控制隐私边界。

## 支持范围

- **平台**：macOS（arm64 / Apple Silicon）
- **WeChat 版本**：4.1.7.x / 4.1.8.x

## 前置条件

密钥提取**需要 SIP 关闭**（SIP enabled 时 `task_for_pid` 被内核拒绝，即使 root 也不行），通常不需要 sudo。如果你已有密钥，可以跳过 SIP 要求，直接用 `key set` 手动录入。

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

### 让 Agent 直接使用

本项目提供 [Agent Skill](https://skills.sh)。安装后，Claude Code、Codex、Cursor 等 Agent 可以直接理解 wx-cli 的能力，并帮你读取历史消息、搜索聊天和订阅新消息：

```bash
npx skills add pandorafuture/wx-cli
```

## 使用

### 1. 检查环境

```bash
wx-cli doctor       # 检查 SIP、DevToolsSecurity、_developer 组、LLDB/python3
wx-cli status       # 查看 WeChat 运行状态和所有账号密钥/缓存状态
```

### 2. 提取密钥

```bash
# LLDB hook 提取密钥 — 会重启 WeChat，通常不需要 sudo
wx-cli key extract --timeout 120

# 查看已保存的密钥
wx-cli key list
```

`key extract` 通过 LLDB hook 捕获 PBKDF2 调用获取原始密钥，覆盖所有数据库。

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
wx-cli export 张三 -o /tmp/export --all --format json  # 导出会话
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

REST 端点：`/api/v1/health`、`/api/v1/sessions`、`/api/v1/contacts`、`/api/v1/messages`、`/api/v1/timeline`、`/api/v1/search`、`/api/v1/media`、`/api/v1/events`（SSE）。

`/api/v1/media` 的图片响应会通过 `X-Wechat-Media-Quality: full|thumbnail` 标明本机返回的是完整图还是缩略图；服务始终优先选择本机已经下载的高清/原图。

会话与联系人 JSON 在本地数据库有记录时会返回可选的 `avatar_url`，可用于展示个人或群聊头像；没有头像字段的旧数据库会省略该值。

其中 `/api/v1/timeline?since=<unix>&until=<unix>` 可在一次请求中读取时间范围内所有会话的消息，适合 Agent 记忆补全、归档和批处理，避免逐会话反复调用。

所有查询命令加 `--format json` 可获取 JSON 格式输出。

## 命令一览

| 命令 | 说明 |
|------|------|
| `wx-cli status` | 查看 WeChat 运行状态 |
| `wx-cli doctor` | 检查环境（SIP 等） |
| `wx-cli key extract` | LLDB hook 提取密钥 |
| `wx-cli key list` | 查看已保存密钥 |
| `wx-cli key set <account> <key>` | 手动设置密钥 |
| `wx-cli key set-image <account> <image-key>` | 手动设置图片密钥 |
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

按账号隐藏指定联系人、群聊或带特定标签的联系人。启用后，查询、导出、监控等命令默认应用隐藏规则（全文搜索除外）。

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
│   ├── wx-keychain/    # 密钥提取（LLDB hook）与本地存储
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
