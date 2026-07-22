# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [0.7.4] - 2026-07-22

### Features

- **Contact and conversation avatars** — Expose an optional `avatar_url` in contact and enriched session JSON, preferring the small avatar and falling back to the large avatar while remaining compatible with older database schemas
- **Image quality metadata** — Mark `/api/v1/media` image responses as `full` or `thumbnail`, while continuing to prefer locally available high-resolution/original image files

### Maintenance

- Keep formatting and Clippy clean on the current stable Rust toolchain, and serialize integration-test server startup to avoid ephemeral-port races

## [0.7.3] - 2026-07-10

### Features

- **Agent-ready WeChat data layer** — Ship an Agent Skill for Claude Code, Codex, Cursor, and other compatible agents to query and subscribe to local WeChat data
- **Cross-conversation timeline** — Add `GET /api/v1/timeline` for time-bounded message reads across all conversations in one request, with pagination, ordering, message-type filters, and privacy filtering
- **Compact timeline output** — Return the conversation identity, sender, direction, timestamp, type, and snippet needed by memory, archive, and reporting agents without duplicating raw message payloads

### Performance

- **Reuse SQLCipher derived keys** — Cache per-database derived keys and reuse them across open, count, refresh, and reopen paths instead of repeating the 256k-round KDF
- **Bound timeline memory** — Keep only the candidates required for the requested page rather than retaining the complete cross-conversation history during sorting
- **Faster long-running server queries** — Reuse warm database connections for batch timeline reads, avoiding one CLI process and HTTP round trip per conversation

### Documentation and maintenance

- Rewrite the README around user-facing capabilities: local database access, real-time subscriptions, Agent integration, automation, memory, CRM, and workflow use cases
- Document Release installation and the bundled Agent Skill
- Update project dependencies and GitHub Actions
- Restore clean `cargo fmt --check`, `cargo clippy -- -D warnings`, and full-workspace test baselines

## [0.7.2] - 2026-04-06

### Features

- **Decrypt WeChat databases** — Automatically decrypt WeChat macOS (4.1.7.x / 4.1.8.x) encrypted databases
- **Extract encryption keys** — Two methods available: `key extract` (recommended, uses LLDB) and `key scan` (memory scan, requires sudo)
- **Browse contacts** — Search and view your WeChat contacts with details like phone, signature, region, labels, and memo
- **Browse conversations** — List recent conversations with unread counts and last message preview
- **Query messages** — Filter messages by contact, group, date range, or message type, with pagination support
- **Full-text search** — Search across all conversations by keyword, with automatic index building
- **Export conversations** — Export chats to TXT or JSON, including images, voice messages, videos, and file attachments
- **Real-time monitoring** — Watch for new messages as they arrive with `watch` command
- **Media handling** — Decrypt images, convert WeChat voice messages to standard audio, decode WeChat-format images (WXGF), and decrypt video channel videos
- **HTTP server mode** — Run as a local HTTP service with REST API and real-time event stream (SSE) for integration with other apps
- **Privacy filtering** — Hide specific contacts or group members from query and server results
- **Environment check** — `doctor` command verifies all prerequisites are met
- **Parallel processing** — Large exports process images, voice, and video in parallel for faster completion
