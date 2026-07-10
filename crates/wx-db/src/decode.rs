use rusqlite::Connection;

use crate::error::DbError;
use crate::model::{split_local_type, ChatRoomMember, Message, MessageContent, PackedInfo};

// --- Protobuf structs (hand-written prost, no .proto files) ---

#[derive(prost::Message)]
pub(crate) struct PackedInfoProto {
    #[prost(uint32, tag = "1")]
    pub r#type: u32,
    #[prost(uint32, tag = "2")]
    pub version: u32,
    #[prost(message, optional, tag = "3")]
    pub image: Option<ImageHashProto>,
    #[prost(message, optional, tag = "4")]
    pub video: Option<VideoHashProto>,
}

#[derive(prost::Message)]
pub(crate) struct ImageHashProto {
    #[prost(string, tag = "4")]
    pub md5: String,
}

#[derive(prost::Message)]
pub(crate) struct VideoHashProto {
    #[prost(string, tag = "8")]
    pub md5: String,
}

#[derive(prost::Message)]
pub(crate) struct RoomDataProto {
    #[prost(message, repeated, tag = "1")]
    pub users: Vec<RoomDataUserProto>,
}

#[derive(prost::Message)]
pub(crate) struct RoomDataUserProto {
    #[prost(string, tag = "1")]
    pub user_name: String,
    #[prost(string, optional, tag = "2")]
    pub display_name: Option<String>,
}

// Zstd magic bytes: 0x28 0xB5 0x2F 0xFD
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

// --- Decode functions ---

/// Compute the message table name for a given talker (wxid / chatroom id).
/// The table name is `Msg_` followed by the full 32-char MD5 hex digest.
pub(crate) fn msg_table_name(talker: &str) -> String {
    let hash = md5::compute(talker.as_bytes());
    format!("Msg_{:x}", hash)
}

/// Check whether a table exists in the given SQLite connection.
pub(crate) fn table_exists(conn: &Connection, name: &str) -> Result<bool, DbError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Check whether a specific column exists in a table.
pub(crate) fn check_column_exists(
    conn: &Connection,
    table: &str,
    col: &str,
) -> Result<bool, DbError> {
    let sql = format!("PRAGMA table_info([{}])", table);
    let mut stmt = conn.prepare(&sql)?;
    let exists = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            Ok(name)
        })?
        .any(|r| r.is_ok_and(|name| name == col));
    Ok(exists)
}

/// Decode raw content bytes, optionally using wcdb compression type.
///
/// - If `wcdb_ct == Some(4)`, treat as zstd compressed data.
/// - Else if raw starts with zstd magic bytes, decompress as zstd.
/// - Otherwise, interpret as UTF-8 (lossy).
pub(crate) fn decode_content(raw: &[u8], wcdb_ct: Option<i32>) -> Result<String, DbError> {
    let is_zstd = wcdb_ct == Some(4) || (raw.len() >= 4 && raw[..4] == ZSTD_MAGIC);

    if is_zstd {
        let decompressed = zstd::decode_all(raw).map_err(|e| DbError::Zstd(e.to_string()))?;
        Ok(String::from_utf8_lossy(&decompressed).into_owned())
    } else {
        Ok(String::from_utf8_lossy(raw).into_owned())
    }
}

/// Decode a PackedInfo protobuf blob into our model type.
/// Returns `None` on any decode error (swallows errors).
pub(crate) fn decode_packed_info(blob: &[u8]) -> Option<PackedInfo> {
    use prost::Message;
    let proto = PackedInfoProto::decode(blob).ok()?;
    Some(PackedInfo {
        image_md5: proto.image.map(|img| img.md5).filter(|s| !s.is_empty()),
        video_md5: proto.video.map(|vid| vid.md5).filter(|s| !s.is_empty()),
    })
}

/// Decode room data protobuf blob into a list of ChatRoomMembers.
pub(crate) fn decode_room_data(blob: &[u8]) -> Vec<ChatRoomMember> {
    use prost::Message;
    let proto = match RoomDataProto::decode(blob) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    proto
        .users
        .into_iter()
        .map(|u| ChatRoomMember {
            user_name: u.user_name,
            display_name: u.display_name.filter(|s| !s.is_empty()),
        })
        .collect()
}

/// Encode a `PackedInfo` protobuf blob for use in test fixtures.
///
/// This is a test-only helper; it is **not** part of the public API.
#[doc(hidden)]
pub fn encode_packed_info_for_test(image_md5: Option<&str>, video_md5: Option<&str>) -> Vec<u8> {
    use prost::Message;
    let proto = PackedInfoProto {
        r#type: 106,
        version: 14,
        image: image_md5.map(|md5| ImageHashProto {
            md5: md5.to_string(),
        }),
        video: video_md5.map(|md5| VideoHashProto {
            md5: md5.to_string(),
        }),
    };
    proto.encode_to_vec()
}

/// Encode a room-data protobuf blob for use in test fixtures.
///
/// This is a test-only helper; it is **not** part of the public API.
#[doc(hidden)]
pub fn encode_room_data_for_test(members: &[(&str, Option<&str>)]) -> Vec<u8> {
    use prost::Message;
    let proto = RoomDataProto {
        users: members
            .iter()
            .map(|(name, display)| RoomDataUserProto {
                user_name: name.to_string(),
                display_name: display.map(|s| s.to_string()),
            })
            .collect(),
    };
    proto.encode_to_vec()
}

/// Decode raw DB columns into a Message for test use.
///
/// This mirrors the logic in `decode_message_row()` (same decode steps in the
/// same order) but takes explicit arguments instead of a `rusqlite::Row`.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn decode_message_for_test(
    sort_seq: i64,
    server_id: i64,
    local_type: i64,
    sender: &str,
    talker: &str,
    create_time: i64,
    raw_content: &[u8],
    packed_info_data: Option<&[u8]>,
    status: i32,
    wcdb_ct: Option<i32>,
    compress_content: Option<&[u8]>,
    is_group: bool,
) -> Result<Message, DbError> {
    // Decode content (zstd decompression if needed)
    let decoded_text = decode_content(raw_content, wcdb_ct)?;

    // Group sender parsing: extract sender from content prefix
    let (sender, content_text) = parse_group_sender(is_group, decoded_text, sender.to_string());

    // Decode packed info
    let packed_info = packed_info_data.and_then(|b| {
        if b.is_empty() {
            None
        } else {
            decode_packed_info(b)
        }
    });

    // Split local_type into msg_type and sub_type
    let (msg_type, sub_type) = split_local_type(local_type);

    // Parse content into typed enum
    let content = parse_content(
        msg_type,
        sub_type,
        &content_text,
        server_id,
        packed_info.as_ref(),
        compress_content,
    );

    Ok(Message {
        sort_seq,
        server_id,
        msg_type,
        sub_type,
        sender,
        talker: talker.to_string(),
        create_time,
        content,
        status,
    })
}

/// Parse group-sender info from decoded text.
///
/// In group chats, the decoded text often starts with a `"sender:\n"` prefix.
/// This function splits on `":\n"` to extract the sender and the remaining content.
///
/// Returns `(sender, content)` tuple. If `is_group` is false or no `":\n"` separator
/// is found, returns `(fallback_sender, decoded_text)` unchanged.
pub(crate) fn parse_group_sender(
    is_group: bool,
    decoded_text: String,
    fallback_sender: String,
) -> (String, String) {
    if is_group {
        if let Some((sender_prefix, rest)) = decoded_text.split_once(":\n") {
            (sender_prefix.to_string(), rest.to_string())
        } else {
            (fallback_sender, decoded_text)
        }
    } else {
        (fallback_sender, decoded_text)
    }
}

/// Parse message content into a typed MessageContent enum variant.
///
/// `compress_content` is an optional zstd-compressed blob from the DB's
/// `compress_content` column, used by app messages (especially sub_type=57
/// quotes) as an alternative content source.
pub(crate) fn parse_content(
    msg_type: u32,
    sub_type: u32,
    content: &str,
    _server_id: i64,
    packed: Option<&PackedInfo>,
    compress_content: Option<&[u8]>,
) -> MessageContent {
    use crate::model::*;
    match msg_type {
        MSG_TYPE_TEXT => MessageContent::Text(content.to_string()),
        MSG_TYPE_IMAGE => MessageContent::Image {
            md5: packed.and_then(|p| p.image_md5.clone()),
        },
        MSG_TYPE_VOICE => MessageContent::Voice,
        MSG_TYPE_VIDEO => MessageContent::Video {
            md5: packed.and_then(|p| p.video_md5.clone()),
        },
        MSG_TYPE_EMOJI => MessageContent::Emoji(content.to_string()),
        MSG_TYPE_LOCATION => MessageContent::Location(content.to_string()),
        MSG_TYPE_APP => {
            // Try compress_content first (zstd-compressed XML), fall back to content
            let xml = if let Some(blob) = compress_content {
                decode_content(blob, None).unwrap_or_else(|_| content.to_string())
            } else {
                content.to_string()
            };
            crate::xml_extract::dispatch_app_message(sub_type, &xml)
        }
        MSG_TYPE_SYSTEM => {
            // Try to extract readable text from sysmsg XML (e.g. revokemsg);
            // fall back to raw content for plain-text system messages.
            let text = crate::xml_extract::extract_system_message_text(content)
                .unwrap_or_else(|| content.to_string());
            MessageContent::System(text)
        }
        MSG_TYPE_REVOKE => MessageContent::Revoke(content.to_string()),
        _ => MessageContent::Unknown {
            msg_type,
            raw: content.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_group_returns_fallback_unchanged() {
        let (sender, content) =
            parse_group_sender(false, "hello world".to_string(), "fallback".to_string());
        assert_eq!(sender, "fallback");
        assert_eq!(content, "hello world");
    }

    #[test]
    fn group_with_sender_prefix() {
        let (sender, content) = parse_group_sender(
            true,
            "wxid_abc:\nHello group".to_string(),
            "fallback".to_string(),
        );
        assert_eq!(sender, "wxid_abc");
        assert_eq!(content, "Hello group");
    }

    #[test]
    fn group_without_colon_newline() {
        let (sender, content) = parse_group_sender(
            true,
            "no colon newline here".to_string(),
            "fallback".to_string(),
        );
        assert_eq!(sender, "fallback");
        assert_eq!(content, "no colon newline here");
    }

    #[test]
    fn group_empty_content_after_separator() {
        let (sender, content) =
            parse_group_sender(true, "wxid_abc:\n".to_string(), "fallback".to_string());
        assert_eq!(sender, "wxid_abc");
        assert_eq!(content, "");
    }

    #[test]
    fn group_only_colon_before_newline() {
        let (sender, content) =
            parse_group_sender(true, ":\nsome content".to_string(), "fallback".to_string());
        assert_eq!(sender, "");
        assert_eq!(content, "some content");
    }

    #[test]
    fn group_colon_but_no_newline() {
        // ":\n" is the separator; colon without newline should not split
        let (sender, content) = parse_group_sender(
            true,
            "wxid_abc: no newline".to_string(),
            "fallback".to_string(),
        );
        assert_eq!(sender, "fallback");
        assert_eq!(content, "wxid_abc: no newline");
    }

    #[test]
    fn non_group_ignores_sender_prefix() {
        // Even if text has ":\n", non-group should not split
        let (sender, content) = parse_group_sender(
            false,
            "wxid_abc:\nHello".to_string(),
            "fallback".to_string(),
        );
        assert_eq!(sender, "fallback");
        assert_eq!(content, "wxid_abc:\nHello");
    }
}
