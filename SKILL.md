---
name: wx-cli
description: Use when the user asks about their WeChat messages, contacts, conversations, chat history, or needs to decrypt/search WeChat data. Provides structured CLI commands for querying encrypted WeChat macOS databases.
---

# wx-cli — WeChat 数据查询工具

## Overview

`wx-cli` 解密并查询 WeChat macOS 加密数据库。零参数即可运行——自动检测账号、密钥和数据目录。

**核心原则：** 用 `--format json` 获取结构化数据供分析；用默认 text 格式展示给用户。

## When to Use

- 用户问"最近跟谁聊了什么"、"某人发过什么消息"
- 用户要搜索聊天记录中的关键词
- 用户要查看联系人信息
- 用户要解密数据库或图片
- 用户提到 WeChat / 微信相关数据需求

## Quick Reference

| 需求 | 命令 |
|------|------|
| 当前状态 | `wx-cli status` |
| **密钥提取（推荐）** | `wx-cli key extract` |
| 手动设置数据库密钥 | `wx-cli key set <account_id> <hex_key>` |
| 手动设置图片密钥 | `wx-cli key set-image <account_id> <image_key>` |
| 已存密钥列表 | `wx-cli key list` |
| 解密数据库 | `wx-cli decrypt` |
| 增量解密 | `wx-cli decrypt --incremental` |
| 解密图片（自动推导密钥）| `wx-cli decode-image <路径> -d <data_dir>` |
| 解密图片（手动 V2 key）| `wx-cli media decrypt-dat <路径> --v2-key <key>` |
| 解密图片（KeyStore）| `wx-cli decode-image <路径> --account <account_id>` |
| 提取语音 | `wx-cli media extract-voice --media-dir <dir> <svr_id>` |
| 提取语音（原始 SILK）| `wx-cli media extract-voice --media-dir <dir> <svr_id> --raw` |
| 解密视频号视频 | `wx-cli media decrypt-video <文件> --seed <seed>` |
| 查询 hardlink 路径 | `wx-cli media resolve-path --db <hardlink.db> <key>` |
| 检查 DB 文件 | `wx-cli info <db_file>` |
| 版本信息 | `wx-cli --version` |
| 最近会话列表 | `wx-cli sessions` |
| 查看系统路径 | `wx-cli paths` |
| 查看系统路径（JSON） | `wx-cli paths --json` |
| 忽略隐藏配置查看完整会话列表 | `wx-cli sessions --show-hidden` |
| 搜索联系人 | `wx-cli contacts --search <关键词>` |
| 忽略隐藏配置搜索联系人 | `wx-cli contacts --search <关键词> --show-hidden` |
| 查某人的消息 | `wx-cli query <联系人名/账号ID>` |
| 忽略隐藏配置查看消息 | `wx-cli query <联系人> --show-hidden` |
| 定位某条消息上下文 | `wx-cli query <联系人> --around-sort-seq <seq> --context 10` |
| 按 server_id 定位上下文 | `wx-cli query <联系人> --around-server-id <id> --context 10` |
| 增量拉取新消息 | `wx-cli query <联系人> --after-sort-seq <seq> --limit 20` |
| 全局搜索关键词 | `wx-cli search <关键词>` |
| **导出会话（TXT，默认并行）** | `wx-cli export <联系人> -o <目录> --all` |
| **导出会话（JSON，默认并行）** | `wx-cli export <联系人> -o <目录> --all --format json` |
| 忽略隐藏配置导出会话 | `wx-cli export <联系人> -o <目录> --all --show-hidden` |
| 导出（无媒体） | `wx-cli export <联系人> -o <目录> --all --no-media` |
| **启动 HTTP API** | `wx-cli server run` |
| 查看 HTTP 服务状态 | `wx-cli server status` |
| 停止 HTTP 服务 | `wx-cli server stop` |
| 重启 HTTP 服务 | `wx-cli server restart` |
| HTTP API（远程） | `wx-cli server run --host 0.0.0.0 --token <secret>` |
| 实时监听会话变化 | `wx-cli watch` |
| 实时监听（忽略隐藏配置） | `wx-cli watch --show-hidden` |
| 实时监听（轮询） | `wx-cli watch --poll --poll-ms 3000` |
| 实时监听（文件事件模式） | `wx-cli watch --fsnotify` |
| 前置条件检查 | `wx-cli doctor` |
| 前置条件检查 + 修复建议 | `wx-cli doctor --fix` |

## 前置条件

密钥提取**需要 SIP 禁用**，通常不需要 sudo。SIP enabled 时 `task_for_pid` 被内核拒绝（kern_return=5），即使 root 也不行。检查 SIP 状态：`csrutil status`。禁用需在 Recovery Mode 执行 `csrutil disable`。

## 常用工作流

### 0. 首次使用：提取密钥并解密

WeChat 数据库是加密的，查询前必须先有密钥。**需要 SIP 禁用**（SIP enabled 时 `task_for_pid` 被内核拒绝）：

```bash
# LLDB hook 提取密钥 — 会重启 WeChat，需要 LLDB + python3，通常不需要 sudo
wx-cli key extract --timeout 120
```

`key extract` 拿到的是完整数据库密钥，**覆盖所有数据库**。后续多数查询可直接读取加密数据库，无需先执行 `decrypt`。

提取后验证：

```bash
# 查看已存密钥（`raw=yes` 表示已保存完整数据库密钥）
wx-cli key list

# 有完整数据库密钥时：query/sessions/search/contacts/server run/watch 可直接读取加密数据库，无需先 decrypt

# 如需明文导出
wx-cli decrypt
wx-cli decrypt --incremental
```

手动设置密钥：

```bash
wx-cli key set <account_id> 0123456789abcdef...       # 数据库密钥（32 字节 hex）
wx-cli key set-image <account_id> abcdefghijklmnop     # 图片 AES 密钥（V2 格式）
```

**密钥类型说明：**

| 密钥 | 用途 | 提取方式 | 覆盖范围 |
|------|------|---------|---------|
| 完整数据库密钥（`raw_key`） | 解密 SQLite 数据库 | `key extract`（LLDB hook） | 所有 DB |
| Image key（16 bytes） | 解密 V2 格式 .dat 图片 | `decode-image -d <data_dir>` 自动推导 | — |

### 1. 查看最近聊天

```bash
wx-cli sessions --limit 10
# 返回按时间倒序的会话列表，含联系人显示名和最后一条消息摘要
```

### 2. 查找某人并读消息

```bash
# 先搜联系人（支持昵称、备注、wxid、微信号、手机号等模糊匹配）
wx-cli contacts --search 张三

# 用搜到的名字或 wxid 查消息
wx-cli query 张三 --limit 20

# 用 JSON 获取结构化数据
wx-cli query 张三 --format json --limit 50
```

### 2a. 联系人隐藏规则

配置文件：`~/Library/Application Support/wx-cli/config/settings.toml`

```toml
[accounts."<account_id>"]
ignore_contacts = ["wxid_hidden_contact"]
ignore_tags = ["同事"]
```

- `query` / `sessions` / `contacts` / `export` / `watch` 支持 `--show-hidden` 忽略隐藏配置
- `search` 当前**不会自动应用隐藏配置**
- **隐私优先：** 当输出里出现 `[消息已隐藏]`，除非用户明确要求，agent 不应主动追加 `--show-hidden`

### 3. 全局搜索关键词

```bash
wx-cli search 周末 --limit 20
# 优先查询 WeChat 自带的全文索引库，搜索通常在亚秒级完成
```

**搜索语义：**
- 中文按字拆分；英文支持 Porter stemming
- 多词搜索为 AND 逻辑
- `stats.scanned=0` 表示走微信内置全文索引；`stats.scanned>0` 表示退回到全量扫描

### 4. 按条件过滤消息

```bash
wx-cli query 张三 --type text           # 按类型：text/image/voice/video/emoji/app/system/revoke
wx-cli query 张三 --since 1772600000 --until 1772700000   # 时间范围（Unix 秒）
```

### 4b. 锚点上下文查询

```bash
wx-cli query 张三 --around-sort-seq 1773421188000 --context 10    # 按 sort_seq 定位前后 10 条
wx-cli query 张三 --around-server-id 5455993825313690274 --context 10  # 按 server_id 定位
wx-cli query 张三 --after-sort-seq 1773421188000 --limit 20       # 增量拉取新消息
```

**行为规则：**
- 锚点查询始终按升序返回，`--order desc` 会被忽略
- `--context` 默认 50，仅对 `around-*` 有效
- `around-*` 参数与 `--since`/`--until`/`--all` 互斥
- `--around-sort-seq`、`--around-server-id`、`--after-sort-seq` 三者互斥

### 5. 群聊查询

```bash
wx-cli query 18819405230@chatroom --limit 10   # 群聊 ID
wx-cli query 周末爬山群                          # 或群名模糊匹配
```

### 6. 导出会话

```bash
wx-cli export 张三 -o /tmp/export/ --all                        # TXT 格式，全部消息
wx-cli export 张三 -o /tmp/export/ --all --format json           # JSON 格式
wx-cli export 张三 -o /tmp/export/ --all --no-media              # 跳过媒体文件
wx-cli export 张三 -o /tmp/export/ --all --show-emoji            # 显示表情细节
```

**排序默认 `asc`**（时间正序），与 `query` 默认 `desc` 相反。

### 7. 图片解密与转码

```bash
# 推荐：-d 自动推导 V2 密钥
wx-cli decode-image input.dat -d <account_data_dir> -o output.png

# 批量目录
wx-cli decode-image /path/to/dat_dir/ -d <account_data_dir> -o /tmp/output/

# 直接传入 V2 AES key
wx-cli media decrypt-dat input.dat --v2-key abcdefghijklmnop -o output.png
```

### 8. 语音提取

```bash
wx-cli media extract-voice --media-dir <dir> <svr_id> -o voice.mp3    # 默认 MP3（需 ffmpeg）
wx-cli media extract-voice --media-dir <dir> <svr_id> --raw -o voice.silk  # 原始 SILK
```

### 8b. Hardlink 路径查询

```bash
wx-cli media resolve-path --db /path/to/hardlink.db <md5_key>                    # 图片
wx-cli media resolve-path --db /path/to/hardlink.db --media-type video <key>     # 视频
wx-cli media resolve-path --db /path/to/hardlink.db --media-type file <key>      # 文件
```

### 9. 视频号视频解密

```bash
wx-cli media decrypt-video encrypted.bin --seed 2105122989 -o video.mp4      # 十进制 seed
wx-cli media decrypt-video encrypted.bin --seed 0x7d844e8d -o video.mp4      # 十六进制 seed
```

### 10. HTTP API 服务

```bash
wx-cli server run                                          # 本地启动（默认 127.0.0.1:9100）
wx-cli server run --host 0.0.0.0 --token mysecret          # 远程访问（必须设 token）
wx-cli server status / stop / restart                      # 管理服务
```

### 10a. CLI 自动复用已运行的 server

`sessions`、`contacts`、`query`、`search` 默认会先尝试连接本机 `http://127.0.0.1:9100`，如果 server 可用就走 HTTP，否则回退到本地直查。

| 参数 | 说明 |
|------|------|
| `--server-url <url>` | 覆盖默认地址 |
| `--server-token <token>` | Bearer token |
| `--server-only` | 只走远程，不回退 |
| `--no-server` | 强制本地查询 |

### 10b. REST 端点（只读）

| 端点 | 说明 | 关键参数 |
|------|------|---------|
| `GET /api/v1/health` | 健康探测 | 无 |
| `GET /api/v1/sessions` | 会话列表 | `limit`, `offset`, `order`, `show_hidden` |
| `GET /api/v1/contacts` | 联系人列表 | `limit`, `offset`, `search`, `show_hidden` |
| `GET /api/v1/messages` | 消息查询 | `contact`（必填）, `limit`, `offset`, `since`, `until`, `type`, `order`, `around_sort_seq`, `around_server_id`, `after_sort_seq`, `context`, `show_hidden` |
| `GET /api/v1/timeline` | 跨全部会话按时间批量读取消息 | `since`（必填）, `until`（必填）, `limit`, `offset`, `type`, `order`, `show_hidden` |
| `GET /api/v1/media` | 媒体内容直出 | `server_id`（必填）, `talker`（必填）, `format=ogg\|mp3`（仅语音） |
| `GET /api/v1/search` | 全文搜索 | `q`（必填）, `limit`, `offset` |
| `GET /api/v1/events` | SSE 事件流 | 无 |

图片响应会携带 `X-Wechat-Media-Quality: full|thumbnail`。服务会优先返回本机已有的高清/原图文件；若微信只下载过缩略图，则返回 `thumbnail`，调用方不应长期缓存，并可在微信下载原图后重试。

**认证：** 带 `--token` 启动时须携带 `Authorization: Bearer <token>`。`--host` 不是本机地址时 `--token` 必填。

当前 HTTP API 为**只读**，没有 send/reply/webhook 等写接口。

### 11. 实时监听

```bash
wx-cli watch                          # 默认启动
wx-cli watch --poll --poll-ms 5000    # 自定义轮询间隔
wx-cli watch --format json            # JSON 格式（每行一个 JSON 对象）
wx-cli watch --show-hidden            # 忽略隐藏配置
```

### 12. 前置条件检查

```bash
wx-cli doctor           # 列出各项检查结果（SIP、DevToolsSecurity、_developer、lldb、python3）
wx-cli doctor --fix      # 对 FAIL 项输出修复命令
```

## 错误自动恢复

**`error: no key for account <account_id>`**

```bash
wx-cli status                           # 1. 确认 WeChat 状态
wx-cli key extract --timeout 120        # 2. 提取密钥
wx-cli key list                         # 3. 验证密钥
wx-cli sessions                         # 4. 重试查询
```

**`task_for_pid failed (kern_return=5)`** — SIP 启用，需在 Recovery Mode 执行 `csrutil disable`。

**`warning: ffmpeg not found`** — 安装 ffmpeg：`brew install ffmpeg`，或 `FFMPEG_PATH=/path/to/ffmpeg wx-cli ...`

## JSON 输出结构

所有查询命令加 `--format json` 返回统一信封：

```json
{
  "items": [...],
  "paging": { "offset": 0, "limit": 20, "returned": 20, "total": 103, "has_more": true },
  "stats": { "scanned": 103, "skipped": 0, "elapsed_ms": 3 }
}
```

| 命令 | item 关键字段 |
|------|-------------|
| sessions | `username`, `display_name`, `avatar_url?`, `summary`, `sort_timestamp`, `direction?` |
| query | `sort_seq`, `server_id`, `msg_type`, `sender`, `content`, `direction` |
| timeline API | `sort_seq`, `server_id`, `msg_type`, `sender`, `talker`, `talker_display_name`, `create_time`, `direction`, `snippet`；统一按时间跨会话排序 |
| contacts | `user_name`, `alias`, `remark`, `nick_name`, `avatar_url?`, `phone`, `labels` |
| search | `server_id`, `talker`, `sender`, `snippet`, `hit_type` |

**易混淆字段：**
- contacts 用 `user_name`（下划线），sessions 用 `username`（无下划线）
- `avatar_url` 优先取微信小头像地址，缺失时回退大头像地址；本地没有记录时不输出
- message 的 `sender` 才是消息级 self/other 判断依据
- `/api/v1/health` 的 `current_account.wxid` 是判断"我发的"的主事实源

**JSON 编程使用：** JSON 走 stdout，诊断信息走 stderr。分离采集才能正确解析：

```bash
OUTPUT=$(wx-cli query 张三 --format json --limit 5)
echo "$OUTPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d['items']))"
```

## 分页与排序

```bash
wx-cli query 张三 --format json --limit 10 --offset 0    # 第一页
wx-cli query 张三 --format json --limit 10 --offset 10   # 第二页
wx-cli query 张三 --order asc                             # 时间正序
wx-cli query 张三 --all                                   # 获取全部消息（上限 20,000）
```

## 消息类型标签（type=49 结构化解析）

| 变体 | sub_type | 输出 |
|------|----------|------|
| Link | 4, 5, 7, 92 | `[链接] <标题>` |
| File | 6 | `[文件] <文件名>` |
| MiniProgram | 33, 36 | `[小程序] <名称>` |
| MergedMessages | 19 | `[聊天记录] <标题>` |
| Quote | 57 | `[引用 @发送者: 原文] 回复文本` |
| Transfer | 2000 | `[转账] ¥金额` |
| RedEnvelope | 2001, 2003 | `[红包] <标题>` |
| ChannelVideo | 51, 63 | `[视频号] <标题>` |
| Pat | 62 | `[拍一拍]` |

JSON `content` 字段为 tagged union：外层 key 是变体名，值是结构化字段。

## 文件路径

| 类别 | 路径（macOS） | 用途 |
|------|---------------|------|
| Config | `~/Library/Application Support/wx-cli/config/` | 密钥、设置 |
| Cache | `~/Library/Caches/wx-cli/` | 解密后数据库 |
| State | `~/Library/Application Support/wx-cli/state/` | 服务运行时 |
| Logs | `~/Library/Logs/wx-cli/` | 服务日志 |
| Temp | `$TMPDIR/wx-cli/` | 密钥提取临时文件 |

使用 `wx-cli paths` 查看所有路径。

## 注意事项

- 所有命令自动检测账号和密钥，通常无需 `--account` 或 `--key`
- `query` 支持 wxid、chatroom ID、filehelper，也支持中文名模糊匹配
- `search` 直接查询 WeChat 自带 `message_fts.db`，无需单独建索引
- `search` 当前不会自动应用隐藏配置，也没有 `--show-hidden`
- `--all` 覆盖 `--limit`（当前上限 20,000）；`export --all` 会按批次拉完全部结果
- `--limit 0` 会退回到该命令的默认值
- `server run` 启动后会自动增量刷新解密缓存

## 诊断策略

当查询返回空结果时，**不要反复换参数重试**：

```bash
# 1. 检查 stats.skipped
wx-cli query <contact> --all --format json 2>/dev/null | \
  python3 -c "import json,sys; d=json.load(sys.stdin); print(f'items={len(d[\"items\"])}, skipped={d[\"stats\"][\"skipped\"]}')"

# 2. skipped > 0 → 重新解密
wx-cli decrypt

# 3. items=0 且 skipped=0 → 检查联系人
wx-cli contacts --search <name> --format json
```
