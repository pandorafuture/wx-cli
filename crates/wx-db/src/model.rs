use serde::{Deserialize, Serialize};

use crate::error::ShardWarning;

/// Maximum number of rows that a single query can return.
pub const MAX_QUERY_LIMIT: usize = 20_000;

/// Default number of rows returned when no explicit limit is specified (i.e. `limit == 0`).
pub const DEFAULT_QUERY_LIMIT: usize = 1_000;

/// Check whether a username refers to a group chat.
pub fn is_group_chat(username: &str) -> bool {
    username.ends_with("@chatroom")
}

/// Sort direction for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortOrder {
    Asc,
    #[default]
    Desc,
}

impl SortOrder {
    /// Return the SQL keyword for this sort direction.
    pub fn sql_keyword(self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }
}

// Message type constants

/// Message type constant: plain text message.
pub const MSG_TYPE_TEXT: u32 = 1;
/// Message type constant: image message.
pub const MSG_TYPE_IMAGE: u32 = 3;
/// Message type constant: voice / audio message.
pub const MSG_TYPE_VOICE: u32 = 34;
/// Message type constant: video message.
pub const MSG_TYPE_VIDEO: u32 = 43;
/// Message type constant: custom emoji / sticker.
pub const MSG_TYPE_EMOJI: u32 = 47;
/// Message type constant: location sharing.
pub const MSG_TYPE_LOCATION: u32 = 48;
/// Message type constant: app / rich-media message (links, mini-programs, etc.).
pub const MSG_TYPE_APP: u32 = 49;
/// Message type constant: system notification.
pub const MSG_TYPE_SYSTEM: u32 = 10000;
/// Message type constant: message recall / revoke notification.
pub const MSG_TYPE_REVOKE: u32 = 10002;

/// Return a human-readable label for the given message type.
pub fn msg_type_label(msg_type: u32) -> &'static str {
    match msg_type {
        MSG_TYPE_TEXT => "text",
        MSG_TYPE_IMAGE => "image",
        MSG_TYPE_VOICE => "voice",
        MSG_TYPE_VIDEO => "video",
        MSG_TYPE_EMOJI => "emoji",
        MSG_TYPE_LOCATION => "location",
        MSG_TYPE_APP => "app",
        MSG_TYPE_SYSTEM => "system",
        MSG_TYPE_REVOKE => "revoke",
        _ => "unknown",
    }
}

/// Parse a message type string (name or numeric) into a `msg_type` value.
///
/// Accepts the same labels returned by [`msg_type_label`] (case-insensitive)
/// plus raw numeric values (e.g. `"49"`).
pub fn parse_msg_type(s: &str) -> Option<u32> {
    match s.to_lowercase().as_str() {
        "text" => Some(MSG_TYPE_TEXT),
        "image" => Some(MSG_TYPE_IMAGE),
        "voice" => Some(MSG_TYPE_VOICE),
        "video" => Some(MSG_TYPE_VIDEO),
        "emoji" => Some(MSG_TYPE_EMOJI),
        "location" => Some(MSG_TYPE_LOCATION),
        "app" => Some(MSG_TYPE_APP),
        "system" => Some(MSG_TYPE_SYSTEM),
        "revoke" => Some(MSG_TYPE_REVOKE),
        _ => s.parse().ok(),
    }
}

// App message sub_type constants (for first-class structured variants)

pub const APP_SUB_TYPE_LINK: u32 = 5;
pub const APP_SUB_TYPE_FILE: u32 = 6;
pub const APP_SUB_TYPE_MINI_PROGRAM: u32 = 33;
pub const APP_SUB_TYPE_MINI_PROGRAM_2: u32 = 36;
pub const APP_SUB_TYPE_MERGED: u32 = 19;
pub const APP_SUB_TYPE_CHANNEL: u32 = 51;
pub const APP_SUB_TYPE_QUOTE: u32 = 57;
pub const APP_SUB_TYPE_PAT: u32 = 62;
pub const APP_SUB_TYPE_CHANNEL_LIVE: u32 = 63;
pub const APP_SUB_TYPE_MUSIC: u32 = 92;
pub const APP_SUB_TYPE_TRANSFER: u32 = 2000;
pub const APP_SUB_TYPE_RED_ENVELOPE: u32 = 2001;

/// Return a human-readable label for the given message type and sub_type.
///
/// For `MSG_TYPE_APP` messages, dispatches on `sub_type` to return a specific
/// label (e.g. `"link"`, `"quote"`, `"transfer"`). For other message types,
/// falls back to [`msg_type_label`]. Unknown app sub_types return `"app"`.
///
/// This is the **authoritative source** for sub_type labels (English).
/// The `schema.rs` `app_sub_type_display_label` covers only the `AppGeneric`
/// fallback display path (Chinese) — sub_types that already have dedicated
/// `MessageContent` variants (link, file, mini-program, quote, transfer, etc.)
/// are handled before reaching `AppGeneric`.
pub fn msg_sub_type_label(msg_type: u32, sub_type: u32) -> &'static str {
    if msg_type != MSG_TYPE_APP {
        return msg_type_label(msg_type);
    }
    match sub_type {
        1 => "text_share",
        2 => "image_share",
        3 => "audio_share",
        4 => "video_share",
        5 => "link",
        6 => "file",
        7 => "webview",
        8 => "gif",
        10 => "location_sharing",
        13 => "brand",
        14 => "chat_log_backup",
        15 => "chat_log_migrate",
        16 => "card_ticket",
        17 => "realtime_location",
        19 => "merged_messages",
        21 => "mini_program_promo",
        24 => "note",
        33 => "mini_program",
        35 => "message_history",
        36 => "mini_program",
        40 => "channel_forward",
        44 => "channel_live_product",
        51 => "channel_video",
        53 => "group_chat_reference",
        57 => "quote",
        62 => "pat",
        63 => "channel_live",
        74 => "channel_files",
        87 => "group_announcement",
        88 => "group_note",
        92 => "music",
        100 => "sticker_set",
        101 => "ad",
        107 => "open_link",
        113 => "video_account_intro",
        116 => "channel_show_card",
        117 => "channel_product",
        124 => "wechat_gift",
        2000 => "transfer",
        2001 => "red_envelope",
        2003 => "red_envelope_cover",
        _ => "app",
    }
}

/// Extract `msg_type` and `sub_type` from a `local_type` value stored in the database.
///
/// WeChat 4.x encodes `local_type` as `(sub_type << 32) | msg_type`.
/// `msg_type` occupies the lower 32 bits, `sub_type` the upper 32 bits.
pub fn split_local_type(local_type: i64) -> (u32, u32) {
    let msg_type = (local_type & 0xFFFFFFFF) as u32;
    let sub_type = ((local_type >> 32) & 0xFFFFFFFF) as u32;
    (msg_type, sub_type)
}

/// Compute the effective query limit from a caller-supplied value.
///
/// - If `limit == 0`, returns [`DEFAULT_QUERY_LIMIT`] (1000).
/// - If `limit > MAX_QUERY_LIMIT`, clamps to [`MAX_QUERY_LIMIT`] (20 000).
/// - Otherwise returns `limit` unchanged.
pub fn effective_limit(limit: usize) -> usize {
    let l = if limit == 0 {
        DEFAULT_QUERY_LIMIT
    } else {
        limit
    };
    l.min(MAX_QUERY_LIMIT)
}

/// Statistics about a query execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryStats {
    /// Total number of database rows scanned (before filtering and pagination).
    pub total_rows: usize,
    /// Number of rows matching keyword + type filters, before pagination.
    /// `Some(n)` when application-level filtering is used (messages with `with_filtered_count`,
    /// contacts with keyword search); `None` otherwise.
    pub filtered_count: Option<usize>,
    /// Number of rows that were skipped due to decode errors (e.g. corrupted zstd data).
    pub skipped: usize,
}

/// A paginated query result containing items and execution statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult<T> {
    /// The result items after filtering, sorting, and pagination.
    pub items: Vec<T>,
    /// Statistics about the query execution.
    pub stats: QueryStats,
}

/// A message query result with shard-level fault tolerance.
///
/// Unlike [`QueryResult<T>`] which propagates errors, this type collects
/// warnings about individual shards that could not be read, allowing the
/// remaining shards to still return results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageQueryResult {
    /// The result items after filtering, sorting, and pagination.
    pub items: Vec<Message>,
    /// Statistics about the query execution.
    pub stats: QueryStats,
    /// Warnings about shards that were skipped due to errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shard_warnings: Vec<ShardWarning>,
}

/// A decoded WeChat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Sort sequence number used for ordering within a shard.
    pub sort_seq: i64,
    /// Server-assigned unique message identifier.
    pub server_id: i64,
    /// The primary message type (lower 32 bits of `local_type`).
    pub msg_type: u32,
    /// The message sub-type (upper 32 bits of `local_type`), used by app messages.
    pub sub_type: u32,
    /// The wxid of the message sender.
    pub sender: String,
    /// The wxid of the conversation partner or chatroom.
    pub talker: String,
    /// Unix timestamp (seconds) when the message was created.
    pub create_time: i64,
    /// Typed message content parsed from raw data.
    pub content: MessageContent,
    /// Message status code from the database.
    pub status: i32,
}

/// Typed message content, parsed from the raw database blob based on `msg_type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    /// Plain text message.
    Text(String),
    /// Image message with an optional MD5 hash from packed info.
    Image {
        /// MD5 hash of the image, extracted from protobuf `packed_info_data`.
        md5: Option<String>,
    },
    /// Voice / audio message (content not decoded).
    Voice,
    /// Video message with an optional MD5 hash from packed info.
    Video {
        /// MD5 hash of the video, extracted from protobuf `packed_info_data`.
        md5: Option<String>,
    },
    /// Custom emoji / sticker (raw XML content).
    Emoji(String),
    /// Location sharing (raw XML content).
    Location(String),
    /// Link share (sub_type=5, 4, 7, 92 etc.).
    Link {
        sub_type: u32,
        title: Option<String>,
        des: Option<String>,
        url: Option<String>,
        raw_xml: String,
    },
    /// File transfer (sub_type=6).
    File {
        title: Option<String>,
        file_ext: Option<String>,
        file_size: Option<u64>,
        md5: Option<String>,
        raw_xml: String,
    },
    /// Mini program (sub_type=33, 36).
    MiniProgram {
        sub_type: u32,
        title: Option<String>,
        url: Option<String>,
        raw_xml: String,
    },
    /// Merged forwarded messages (sub_type=19).
    MergedMessages {
        title: Option<String>,
        raw_xml: String,
    },
    /// Quote / reply (sub_type=57).
    Quote {
        reply_text: Option<String>,
        refer_sender: Option<String>,
        refer_content: Option<String>,
        refer_type: Option<u32>,
        raw_xml: String,
    },
    /// Transfer / payment (sub_type=2000).
    Transfer {
        amount_desc: Option<String>,
        pay_memo: Option<String>,
        pay_sub_type: Option<u32>,
        raw_xml: String,
    },
    /// Red envelope (sub_type=2001, 2003).
    RedEnvelope {
        title: Option<String>,
        raw_xml: String,
    },
    /// Channel video / live (sub_type=51, 63).
    ChannelVideo {
        sub_type: u32,
        title: Option<String>,
        raw_xml: String,
    },
    /// Pat message (sub_type=62).
    Pat { raw_xml: String },
    /// Generic app message (unknown sub_type fallback).
    AppGeneric {
        sub_type: u32,
        title: Option<String>,
        des: Option<String>,
        url: Option<String>,
        raw_xml: String,
    },
    /// System notification message.
    System(String),
    /// Message recall / revoke notification.
    Revoke(String),
    /// Unknown or unsupported message type, preserved as raw text.
    Unknown {
        /// The unrecognized `msg_type` value.
        msg_type: u32,
        /// The raw content text.
        raw: String,
    },
}

/// Decoded protobuf packed info attached to image/video messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackedInfo {
    /// MD5 hash of the image, if present in the protobuf.
    pub image_md5: Option<String>,
    /// MD5 hash of the video, if present in the protobuf.
    pub video_md5: Option<String>,
}

/// A WeChat contact entry from `contact.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    /// The unique WeChat user name (wxid).
    pub user_name: String,
    /// The user-set alias (WeChat ID).
    pub alias: String,
    /// The remark name set by the account owner.
    pub remark: String,
    /// The user's display nickname.
    pub nick_name: String,
    /// Memo / description set by the account owner.
    pub memo: Option<String>,
    /// Gender from extra_buffer (1=male, 2=female).
    pub gender: Option<u32>,
    /// Personal signature from extra_buffer.
    pub signature: Option<String>,
    /// Region from extra_buffer (country · province · city).
    pub region: Option<String>,
    /// Source scene code from extra_buffer.
    pub source_scene: Option<u32>,
    /// Phone number from extra_buffer.
    pub phone: Option<String>,
    /// Resolved label names from extra_buffer + contact_label table.
    pub labels: Vec<String>,
    /// Preferred avatar URL from `small_head_url`, falling back to `big_head_url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// A WeChat chatroom (group chat) entry with its member list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRoom {
    /// The chatroom identifier (e.g. `"12345@chatroom"`).
    pub username: String,
    /// The wxid of the chatroom owner.
    pub owner: String,
    /// List of chatroom members decoded from the protobuf `ext_buffer`.
    pub members: Vec<ChatRoomMember>,
}

/// A single member within a chatroom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRoomMember {
    /// The member's wxid.
    pub user_name: String,
    /// The member's in-group display name, if set.
    pub display_name: Option<String>,
}

/// A recent conversation session from `session.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// The wxid of the conversation partner or chatroom.
    pub username: String,
    /// Summary text of the last message in this session.
    pub summary: String,
    /// Unix timestamp (seconds) of the last activity, used for sort order.
    pub sort_timestamp: i64,
    /// The message type of the last message (e.g. 1=text, 3=image).
    pub last_msg_type: Option<u32>,
    /// The wxid of the last message sender.
    pub last_msg_sender: Option<String>,
    /// The display name of the last message sender.
    pub last_sender_display_name: Option<String>,
}

// --- Query structs with builder methods ---

/// Anchor mode for context-based message queries.
#[derive(Debug, Clone)]
pub enum AnchorMode {
    /// Query messages around a sort_seq (before + pivot bucket + after).
    /// When multiple messages share this sort_seq, all are included as the pivot bucket.
    AroundSortSeq(i64),
    /// Query messages around a server_id (two-phase: locate → context).
    /// Locates the exact message by server_id, retrieves its (sort_seq, create_time, server_id)
    /// as compound pivot key, then queries context around that precise position.
    AroundServerId(i64),
    /// Query messages strictly after a sort_seq (incremental pull).
    AfterSortSeq(i64),
}

/// Default context window size for around queries (messages before/after pivot).
pub const DEFAULT_CONTEXT: usize = 50;

/// Parameters for querying messages from a specific conversation.
///
/// Use [`MessageQuery::for_talker`] to create a query, then chain builder
/// methods to refine it:
///
/// ```ignore
/// let q = MessageQuery::for_talker("wxid_alice")
///     .time_range(1700000000, 1710000000)
///     .keyword("hello")
///     .limit(50)
///     .offset(0);
/// ```
#[derive(Debug, Clone)]
pub struct MessageQuery {
    /// The wxid or chatroom ID to query messages for.
    pub talker: String,
    /// Start of the time range filter (inclusive, Unix seconds). Default: `0`.
    pub start_time: i64,
    /// End of the time range filter (inclusive, Unix seconds). Default: `i64::MAX`.
    pub end_time: i64,
    /// Optional keyword for post-SQL content filtering (case-insensitive).
    pub keyword: Option<String>,
    /// Maximum number of results to return. `0` means use [`DEFAULT_QUERY_LIMIT`].
    pub limit: usize,
    /// Number of results to skip for pagination.
    pub offset: usize,
    /// Sort direction for results. Default: [`SortOrder::Desc`].
    pub order: SortOrder,
    /// Whether to compute `filtered_count` in [`QueryStats`].
    pub with_filtered_count: bool,
    /// Optional message type filter (applied after keyword filter, before pagination).
    pub msg_type_filter: Option<u32>,
    /// Anchor mode for context-based queries (mutually exclusive with time range / offset).
    pub anchor: Option<AnchorMode>,
    /// Context window size for around queries (messages before/after pivot). Default: 50.
    pub context: usize,
}

impl MessageQuery {
    /// Create a new query targeting the given talker (wxid or chatroom ID).
    pub fn for_talker(talker: impl Into<String>) -> Self {
        Self {
            talker: talker.into(),
            start_time: 0,
            end_time: i64::MAX,
            keyword: None,
            limit: 0,
            offset: 0,
            order: SortOrder::default(),
            with_filtered_count: false,
            msg_type_filter: None,
            anchor: None,
            context: DEFAULT_CONTEXT,
        }
    }

    /// Set the time range filter `[start, end]` (inclusive, Unix seconds).
    pub fn time_range(mut self, start: i64, end: i64) -> Self {
        self.start_time = start;
        self.end_time = end;
        self
    }

    /// Set only the start of the time range (inclusive, Unix seconds).
    pub fn since(mut self, start: i64) -> Self {
        self.start_time = start;
        self
    }

    /// Set only the end of the time range (inclusive, Unix seconds).
    pub fn until(mut self, end: i64) -> Self {
        self.end_time = end;
        self
    }

    /// Set a keyword filter. Only messages whose text content contains
    /// this keyword (case-insensitive) will be returned.
    pub fn keyword(mut self, kw: impl Into<String>) -> Self {
        self.keyword = Some(kw.into());
        self
    }

    /// Set the maximum number of results. Values of `0` use
    /// [`DEFAULT_QUERY_LIMIT`]; values above [`MAX_QUERY_LIMIT`] are clamped.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the pagination offset (number of results to skip).
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Set the sort direction for results.
    pub fn order(mut self, order: SortOrder) -> Self {
        self.order = order;
        self
    }

    /// Enable or disable `filtered_count` computation in [`QueryStats`].
    pub fn with_filtered_count(mut self, yes: bool) -> Self {
        self.with_filtered_count = yes;
        self
    }

    /// Set a message type filter. Only messages with this `msg_type` will be returned.
    pub fn msg_type(mut self, mt: u32) -> Self {
        self.msg_type_filter = Some(mt);
        self
    }

    /// Set anchor mode to query messages around a specific sort_seq.
    pub fn around_sort_seq(mut self, seq: i64) -> Self {
        self.anchor = Some(AnchorMode::AroundSortSeq(seq));
        self
    }

    /// Set anchor mode to query messages around the message with a specific server_id.
    pub fn around_server_id(mut self, id: i64) -> Self {
        self.anchor = Some(AnchorMode::AroundServerId(id));
        self
    }

    /// Set anchor mode to query messages strictly after a specific sort_seq.
    pub fn after_sort_seq(mut self, seq: i64) -> Self {
        self.anchor = Some(AnchorMode::AfterSortSeq(seq));
        self
    }

    /// Set the context window size for around queries.
    pub fn context(mut self, n: usize) -> Self {
        self.context = n;
        self
    }

    /// Whether this query is eligible for SQL LIMIT pushdown.
    ///
    /// Returns `true` when there is no keyword search, no `filtered_count`
    /// request, and no anchor mode — i.e. a straightforward paginated browse.
    pub fn limit_pushdown_eligible(&self) -> bool {
        self.keyword.is_none() && !self.with_filtered_count && self.anchor.is_none()
    }
}

/// Parameters for querying contacts.
///
/// ```ignore
/// let q = ContactQuery::new().keyword("alice").limit(10);
/// ```
#[derive(Debug, Clone)]
pub struct ContactQuery {
    /// Optional keyword to search across userName, alias, remark, nickName, description,
    /// phone, labels, signature, and region.
    pub keyword: Option<String>,
    /// Maximum number of results. `0` means use [`DEFAULT_QUERY_LIMIT`].
    pub limit: usize,
    /// Number of results to skip for pagination.
    pub offset: usize,
}

impl ContactQuery {
    /// Create a new contact query with default parameters.
    pub fn new() -> Self {
        Self {
            keyword: None,
            limit: 0,
            offset: 0,
        }
    }

    /// Set a keyword filter. Matches against userName, alias, remark, nickName,
    /// description, phone, labels, signature, and region.
    pub fn keyword(mut self, kw: impl Into<String>) -> Self {
        self.keyword = Some(kw.into());
        self
    }

    /// Set the maximum number of results.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the pagination offset.
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }
}

impl Default for ContactQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parameters for querying chatrooms.
///
/// ```ignore
/// let q = ChatRoomQuery::new().username("12345@chatroom");
/// ```
#[derive(Debug, Clone)]
pub struct ChatRoomQuery {
    /// Optional chatroom username to filter by. If `None`, returns all chatrooms.
    pub username: Option<String>,
    /// Maximum number of results. `0` means use [`DEFAULT_QUERY_LIMIT`].
    pub limit: usize,
    /// Number of results to skip for pagination.
    pub offset: usize,
}

impl ChatRoomQuery {
    /// Create a new chatroom query with default parameters.
    pub fn new() -> Self {
        Self {
            username: None,
            limit: 0,
            offset: 0,
        }
    }

    /// Filter to a specific chatroom by its username.
    pub fn username(mut self, name: impl Into<String>) -> Self {
        self.username = Some(name.into());
        self
    }

    /// Set the maximum number of results.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the pagination offset.
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }
}

impl Default for ChatRoomQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parameters for querying recent sessions (conversations).
///
/// ```ignore
/// let q = SessionQuery::new().limit(20);
/// ```
#[derive(Debug, Clone)]
pub struct SessionQuery {
    /// Maximum number of results. `0` means use [`DEFAULT_QUERY_LIMIT`].
    pub limit: usize,
    /// Number of results to skip for pagination.
    pub offset: usize,
    /// Sort direction for results. Default: [`SortOrder::Desc`].
    pub order: SortOrder,
}

impl SessionQuery {
    /// Create a new session query with default parameters.
    pub fn new() -> Self {
        Self {
            limit: 0,
            offset: 0,
            order: SortOrder::default(),
        }
    }

    /// Set the maximum number of results.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set the pagination offset.
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Set the sort direction for results.
    pub fn order(mut self, order: SortOrder) -> Self {
        self.order = order;
        self
    }
}

impl Default for SessionQuery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_msg_type_names() {
        assert_eq!(parse_msg_type("text"), Some(MSG_TYPE_TEXT));
        assert_eq!(parse_msg_type("IMAGE"), Some(MSG_TYPE_IMAGE));
        assert_eq!(parse_msg_type("Voice"), Some(MSG_TYPE_VOICE));
        assert_eq!(parse_msg_type("video"), Some(MSG_TYPE_VIDEO));
        assert_eq!(parse_msg_type("emoji"), Some(MSG_TYPE_EMOJI));
        assert_eq!(parse_msg_type("location"), Some(MSG_TYPE_LOCATION));
        assert_eq!(parse_msg_type("app"), Some(MSG_TYPE_APP));
        assert_eq!(parse_msg_type("system"), Some(MSG_TYPE_SYSTEM));
        assert_eq!(parse_msg_type("revoke"), Some(MSG_TYPE_REVOKE));
    }

    #[test]
    fn parse_msg_type_numeric_fallback() {
        assert_eq!(parse_msg_type("49"), Some(49));
        assert_eq!(parse_msg_type("10002"), Some(10002));
    }

    #[test]
    fn parse_msg_type_invalid() {
        assert_eq!(parse_msg_type("unknown"), None);
        assert_eq!(parse_msg_type(""), None);
    }

    #[test]
    fn parse_msg_type_roundtrip() {
        for &mt in &[
            MSG_TYPE_TEXT,
            MSG_TYPE_IMAGE,
            MSG_TYPE_VOICE,
            MSG_TYPE_VIDEO,
            MSG_TYPE_EMOJI,
            MSG_TYPE_LOCATION,
            MSG_TYPE_APP,
            MSG_TYPE_SYSTEM,
            MSG_TYPE_REVOKE,
        ] {
            let label = msg_type_label(mt);
            assert_eq!(
                parse_msg_type(label),
                Some(mt),
                "roundtrip failed for {label}"
            );
        }
    }

    #[test]
    fn message_query_since_until() {
        let q = MessageQuery::for_talker("test").since(100).until(200);
        assert_eq!(q.start_time, 100);
        assert_eq!(q.end_time, 200);
    }

    #[test]
    fn message_query_since_only() {
        let q = MessageQuery::for_talker("test").since(100);
        assert_eq!(q.start_time, 100);
        assert_eq!(q.end_time, i64::MAX);
    }

    #[test]
    fn message_query_until_only() {
        let q = MessageQuery::for_talker("test").until(200);
        assert_eq!(q.start_time, 0);
        assert_eq!(q.end_time, 200);
    }

    #[test]
    fn limit_pushdown_eligible_basic() {
        let q = MessageQuery::for_talker("test");
        assert!(q.limit_pushdown_eligible());
    }

    #[test]
    fn limit_pushdown_blocked_by_keyword() {
        let q = MessageQuery::for_talker("test").keyword("hello");
        assert!(!q.limit_pushdown_eligible());
    }

    #[test]
    fn limit_pushdown_blocked_by_filtered_count() {
        let q = MessageQuery::for_talker("test").with_filtered_count(true);
        assert!(!q.limit_pushdown_eligible());
    }

    #[test]
    fn limit_pushdown_blocked_by_anchor() {
        let q = MessageQuery::for_talker("test").around_sort_seq(100);
        assert!(!q.limit_pushdown_eligible());
    }

    #[test]
    fn limit_pushdown_eligible_with_msg_type_and_limit() {
        let q = MessageQuery::for_talker("test")
            .msg_type(1)
            .limit(50)
            .offset(10);
        assert!(q.limit_pushdown_eligible());
    }

    #[test]
    fn message_query_anchor_builders() {
        let q = MessageQuery::for_talker("test")
            .around_sort_seq(12345)
            .context(20);
        assert!(matches!(q.anchor, Some(AnchorMode::AroundSortSeq(12345))));
        assert_eq!(q.context, 20);

        let q = MessageQuery::for_talker("test").around_server_id(999);
        assert!(matches!(q.anchor, Some(AnchorMode::AroundServerId(999))));
        assert_eq!(q.context, DEFAULT_CONTEXT);

        let q = MessageQuery::for_talker("test").after_sort_seq(5000);
        assert!(matches!(q.anchor, Some(AnchorMode::AfterSortSeq(5000))));
    }

    #[test]
    fn test_is_group_chat() {
        assert!(is_group_chat("12345678@chatroom"));
        assert!(is_group_chat("@chatroom"));
        assert!(!is_group_chat("wxid_abc123"));
        assert!(!is_group_chat("filehelper"));
        assert!(!is_group_chat(""));
    }
}
