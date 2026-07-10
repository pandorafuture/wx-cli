use serde::{Deserialize, Serialize};
use wx_context::{ContactResolver, Direction, VisibilityIndex};
use wx_db::{extract_quote_fromusr, is_group_chat, FtsHit, Message, NativeFtsHit, Session};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrichedMessage {
    #[serde(flatten)]
    pub message: Message,
    pub sender_display_name: String,
    pub direction: Direction,
    pub snippet: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrichedSession {
    #[serde(flatten)]
    pub session: Session,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub server_id: i64,
    pub talker: String,
    pub talker_display_name: String,
    pub sender: String,
    pub sender_display_name: String,
    pub direction: Direction,
    pub create_time: i64,
    pub sort_seq: i64,
    pub msg_type: u32,
    pub sub_type: u32,
    pub snippet: String,
    pub hit_type: String,
}

pub fn enrich_message(
    msg: Message,
    self_wxid: &str,
    resolver: &ContactResolver,
) -> EnrichedMessage {
    let direction = Direction::detect(&msg.sender, self_wxid);
    let sender_display_name = resolver.display_with_id(&msg.sender);
    let snippet = format_content(&msg);
    EnrichedMessage {
        message: msg,
        sender_display_name,
        direction,
        snippet,
    }
}

pub fn enrich_session(
    session: Session,
    self_wxid: &str,
    resolver: &ContactResolver,
    detected_at: Option<i64>,
) -> EnrichedSession {
    let display_name = resolver.display_with_id(&session.username);
    let direction = derive_session_direction(session.last_msg_sender.as_deref(), self_wxid);
    EnrichedSession {
        session,
        display_name,
        direction,
        detected_at,
    }
}

pub fn enrich_session_event(
    ev: wx_monitor::SessionEvent,
    self_wxid: &str,
    resolver: &ContactResolver,
) -> EnrichedSession {
    let display_name = resolver.display_with_id(&ev.username);
    let direction = derive_session_direction(ev.last_msg_sender.as_deref(), self_wxid);
    let session = Session {
        username: ev.username,
        summary: ev.summary,
        sort_timestamp: ev.sort_timestamp,
        last_msg_type: ev.last_msg_type,
        last_msg_sender: ev.last_msg_sender,
        last_sender_display_name: ev.last_sender_display_name,
    };
    EnrichedSession {
        session,
        display_name,
        direction,
        detected_at: Some(ev.detected_at),
    }
}

pub fn derive_session_direction(
    last_msg_sender: Option<&str>,
    self_wxid: &str,
) -> Option<Direction> {
    let sender = last_msg_sender?.trim();
    if sender.is_empty() {
        None
    } else {
        Some(Direction::detect(sender, self_wxid))
    }
}

/// Kept for Task 6: remove self-built FTS index code.
#[allow(dead_code)]
pub fn enrich_fts_hit(hit: FtsHit, self_wxid: &str, resolver: &ContactResolver) -> SearchHit {
    let direction = Direction::detect(&hit.sender, self_wxid);
    SearchHit {
        server_id: hit.server_id,
        talker_display_name: resolver.display_with_id(&hit.talker),
        talker: hit.talker,
        sender_display_name: resolver.display_with_id(&hit.sender),
        sender: hit.sender,
        direction,
        create_time: hit.create_time,
        sort_seq: hit.sort_seq,
        msg_type: hit.msg_type,
        sub_type: hit.sub_type,
        snippet: hit.snippet,
        hit_type: "message".to_string(),
    }
}

pub fn enrich_native_fts_hit(
    hit: NativeFtsHit,
    self_wxid: &str,
    resolver: &ContactResolver,
) -> SearchHit {
    let direction = Direction::detect(&hit.sender, self_wxid);
    SearchHit {
        server_id: 0,
        talker_display_name: resolver.display_with_id(&hit.talker),
        talker: hit.talker,
        sender_display_name: resolver.display_with_id(&hit.sender),
        sender: hit.sender,
        direction,
        create_time: hit.create_time,
        sort_seq: hit.sort_seq,
        msg_type: hit.msg_type,
        sub_type: hit.sub_type,
        snippet: hit.snippet,
        hit_type: "message".to_string(),
    }
}

pub fn enrich_message_as_hit(
    msg: Message,
    talker: String,
    self_wxid: &str,
    resolver: &ContactResolver,
) -> SearchHit {
    let direction = Direction::detect(&msg.sender, self_wxid);
    let snippet = format_content(&msg);
    SearchHit {
        server_id: msg.server_id,
        talker_display_name: resolver.display_with_id(&talker),
        talker,
        sender_display_name: resolver.display_with_id(&msg.sender),
        sender: msg.sender,
        direction,
        create_time: msg.create_time,
        sort_seq: msg.sort_seq,
        msg_type: msg.msg_type,
        sub_type: msg.sub_type,
        snippet,
        hit_type: "message".to_string(),
    }
}

// --- Phase 2: sender-level projection helpers ---

/// Filter a single message by sender visibility, and redact quote references if needed.
///
/// Returns `None` if the message's sender is hidden in this group.
/// For Quote messages referencing a hidden sender, clears refer_sender/refer_content/raw_xml.
pub fn project_message_item(
    mut msg: EnrichedMessage,
    talker: &str,
    visibility: &VisibilityIndex,
) -> Option<EnrichedMessage> {
    if visibility.is_hidden_sender_in_group(talker, &msg.message.sender) {
        return None;
    }

    // Redact quote references to hidden senders
    if let wx_db::MessageContent::Quote {
        ref raw_xml,
        ref mut refer_sender,
        ref mut refer_content,
        ..
    } = msg.message.content
    {
        if let Some(fromusr) = extract_quote_fromusr(raw_xml) {
            if visibility.is_hidden_sender_in_group(talker, &fromusr) {
                *refer_sender = None;
                *refer_content = None;
                // Clear raw_xml and recalculate snippet
                if let wx_db::MessageContent::Quote {
                    ref mut raw_xml, ..
                } = msg.message.content
                {
                    *raw_xml = String::new();
                }
                msg.snippet = format_content(&msg.message);
            }
        }
    }

    Some(msg)
}

/// Filter a list of messages by sender visibility.
///
/// When `show_hidden` is true, returns the original list unchanged.
pub fn project_message_items(
    items: Vec<EnrichedMessage>,
    talker: &str,
    visibility: &VisibilityIndex,
    show_hidden: bool,
) -> Vec<EnrichedMessage> {
    if show_hidden {
        return items;
    }
    items
        .into_iter()
        .filter_map(|msg| project_message_item(msg, talker, visibility))
        .collect()
}

/// Redact session sender info if the last message sender is hidden.
///
/// For group chats where the last_msg_sender is a hidden sender:
/// - summary → "[消息已隐藏]"
/// - last_msg_sender → None
/// - last_sender_display_name → None
/// - direction → None
pub fn project_session_sender(session: &mut EnrichedSession, visibility: &VisibilityIndex) {
    if !is_group_chat(&session.session.username) {
        return;
    }
    if let Some(ref sender) = session.session.last_msg_sender {
        if visibility.is_hidden_sender_in_group(&session.session.username, sender) {
            session.session.summary = "[消息已隐藏]".to_string();
            session.session.last_msg_sender = None;
            session.session.last_sender_display_name = None;
            session.direction = None;
        }
    }
}

pub fn format_content(msg: &Message) -> String {
    match &msg.content {
        wx_db::MessageContent::Text(s) => s.clone(),
        wx_db::MessageContent::Image { md5 } => {
            format!("[image {}]", md5.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::Voice => "[voice]".into(),
        wx_db::MessageContent::Video { md5 } => {
            format!("[video {}]", md5.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::Emoji(s) => format!("[emoji {s}]"),
        wx_db::MessageContent::Location(s) => format!("[location {s}]"),
        wx_db::MessageContent::Link { title, .. } => {
            format!("[链接] {}", title.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::File { title, .. } => {
            format!("[文件] {}", title.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::MiniProgram { title, .. } => {
            format!("[小程序] {}", title.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::MergedMessages { title, .. } => {
            format!("[聊天记录] {}", title.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::Quote {
            reply_text,
            refer_sender,
            refer_content,
            ..
        } => {
            let reply = reply_text.as_deref().unwrap_or("");
            match (refer_sender.as_deref(), refer_content.as_deref()) {
                (Some(sender), Some(content)) => {
                    format!("[引用 @{sender}: {content}] {reply}")
                }
                _ => format!("[引用] {reply}"),
            }
        }
        wx_db::MessageContent::Transfer { amount_desc, .. } => {
            format!("[转账] {}", amount_desc.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::RedEnvelope { title, .. } => {
            format!("[红包] {}", title.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::ChannelVideo { title, .. } => {
            format!("[视频号] {}", title.as_deref().unwrap_or(""))
        }
        wx_db::MessageContent::Pat { .. } => "[拍一拍]".into(),
        wx_db::MessageContent::AppGeneric {
            sub_type, title, ..
        } => {
            let label = app_sub_type_display_label(*sub_type);
            let t = title.as_deref().unwrap_or("");
            format!("[{label}] {t}")
        }
        wx_db::MessageContent::System(s) => format!("[system] {s}"),
        wx_db::MessageContent::Revoke(s) => format!("[revoke] {s}"),
        wx_db::MessageContent::Unknown { msg_type, .. } => {
            format!("[type={msg_type}]")
        }
    }
}

/// Chinese display labels for app sub_types that fall through to `AppGeneric`.
///
/// Only covers sub_types NOT already handled by dedicated `MessageContent` variants
/// (link, file, mini-program, quote, transfer, etc.). The authoritative full mapping
/// is in `wx_db::msg_sub_type_label` (English).
fn app_sub_type_display_label(sub_type: u32) -> String {
    match sub_type {
        1 => "文本分享".into(),
        2 => "图片分享".into(),
        3 => "音频分享".into(),
        7 => "网页应用".into(),
        8 => "GIF表情".into(),
        10 => "位置共享".into(),
        13 => "品牌消息".into(),
        14 => "聊天备份".into(),
        15 => "聊天迁移".into(),
        16 => "卡券".into(),
        17 => "实时位置".into(),
        21 => "小程序推广".into(),
        24 => "笔记".into(),
        35 => "消息历史".into(),
        40 => "视频号转发".into(),
        44 => "直播商品".into(),
        53 => "群聊引用".into(),
        74 => "视频号文件".into(),
        87 => "群公告".into(),
        88 => "群笔记".into(),
        100 => "表情包".into(),
        101 => "广告".into(),
        107 => "微信链接".into(),
        113 => "视频号名片".into(),
        116 => "视频号橱窗".into(),
        117 => "视频号商品".into(),
        124 => "微信礼物".into(),
        2003 => "红包封面".into(),
        _ => format!("app type={sub_type}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn make_session(username: &str, last_msg_sender: Option<&str>) -> Session {
        Session {
            username: username.to_string(),
            summary: "hello".to_string(),
            sort_timestamp: 1,
            last_msg_type: Some(1),
            last_msg_sender: last_msg_sender.map(str::to_string),
            last_sender_display_name: None,
        }
    }

    #[test]
    fn enrich_session_detects_known_outgoing_direction() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_session(
            make_session("wxid_friend", Some("wxid_me")),
            "wxid_me",
            &resolver,
            None,
        );

        assert_eq!(enriched.direction, Some(Direction::Outgoing));
    }

    #[test]
    fn enrich_session_detects_known_incoming_direction() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_session(
            make_session("wxid_friend", Some("wxid_friend")),
            "wxid_me",
            &resolver,
            None,
        );

        assert_eq!(enriched.direction, Some(Direction::Incoming));
    }

    #[test]
    fn enrich_session_omits_direction_when_last_sender_missing() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_session(
            make_session("wxid_friend", None),
            "wxid_me",
            &resolver,
            None,
        );

        assert_eq!(enriched.direction, None);

        let json = serde_json::to_value(&enriched).unwrap();
        assert!(json.get("direction").is_none());
    }

    #[test]
    fn enrich_session_omits_direction_when_last_sender_empty_for_group() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_session(
            make_session("group@chatroom", Some("")),
            "wxid_me",
            &resolver,
            None,
        );

        assert_eq!(enriched.direction, None);

        let json = serde_json::to_value(&enriched).unwrap();
        assert!(json.get("direction").is_none());
    }

    #[test]
    fn message_level_direction_remains_required_in_json() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_message(
            Message {
                sort_seq: 1,
                server_id: 2,
                msg_type: 1,
                sub_type: 0,
                sender: "wxid_me".to_string(),
                talker: "wxid_friend".to_string(),
                create_time: 3,
                content: wx_db::MessageContent::Text("hello".to_string()),
                status: 0,
            },
            "wxid_me",
            &resolver,
        );

        let json = serde_json::to_value(&enriched).unwrap();
        assert_eq!(
            json.get("direction"),
            Some(&Value::String("outgoing".to_string()))
        );
    }

    #[test]
    fn legacy_non_wxid_self_id_outgoing_direction() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_message(
            Message {
                sort_seq: 1,
                server_id: 2,
                msg_type: 1,
                sub_type: 0,
                sender: "testuser001".to_string(),
                talker: "wxid_friend".to_string(),
                create_time: 3,
                content: wx_db::MessageContent::Text("hello".to_string()),
                status: 0,
            },
            "testuser001",
            &resolver,
        );

        assert_eq!(enriched.direction, Direction::Outgoing);
    }

    #[test]
    fn legacy_non_wxid_self_id_incoming_direction() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_message(
            Message {
                sort_seq: 1,
                server_id: 2,
                msg_type: 1,
                sub_type: 0,
                sender: "wxid_friend".to_string(),
                talker: "wxid_friend".to_string(),
                create_time: 3,
                content: wx_db::MessageContent::Text("hello".to_string()),
                status: 0,
            },
            "testuser001",
            &resolver,
        );

        assert_eq!(enriched.direction, Direction::Incoming);
    }

    #[test]
    fn legacy_non_wxid_session_outgoing_direction() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_session(
            make_session("wxid_friend", Some("testuser001")),
            "testuser001",
            &resolver,
            None,
        );

        assert_eq!(enriched.direction, Some(Direction::Outgoing));
    }

    #[test]
    fn legacy_non_wxid_session_incoming_direction() {
        let resolver = ContactResolver::empty();
        let enriched = enrich_session(
            make_session("wxid_friend", Some("wxid_friend")),
            "testuser001",
            &resolver,
            None,
        );

        assert_eq!(enriched.direction, Some(Direction::Incoming));
    }

    // --- Phase 2: projection helper tests ---

    fn make_message(sender: &str, talker: &str, content: wx_db::MessageContent) -> Message {
        Message {
            sort_seq: 1,
            server_id: 1,
            msg_type: 1,
            sub_type: 0,
            sender: sender.to_string(),
            talker: talker.to_string(),
            create_time: 1000,
            content,
            status: 0,
        }
    }

    fn make_enriched(
        sender: &str,
        talker: &str,
        content: wx_db::MessageContent,
    ) -> EnrichedMessage {
        let msg = make_message(sender, talker, content);
        let snippet = format_content(&msg);
        EnrichedMessage {
            message: msg,
            sender_display_name: sender.to_string(),
            direction: Direction::Incoming,
            snippet,
        }
    }

    fn vis_with_hidden_persons(persons: &[&str]) -> VisibilityIndex {
        VisibilityIndex::build(
            &persons.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &[],
            &ContactResolver::empty(),
        )
    }

    #[test]
    fn project_message_item_non_group_does_not_filter() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let msg = make_enriched(
            "wxid_spam",
            "wxid_spam",
            wx_db::MessageContent::Text("hi".into()),
        );
        assert!(project_message_item(msg, "wxid_spam", &vis).is_some());
    }

    #[test]
    fn project_message_item_group_hidden_sender_filtered() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let msg = make_enriched(
            "wxid_spam",
            "group@chatroom",
            wx_db::MessageContent::Text("spam".into()),
        );
        assert!(project_message_item(msg, "group@chatroom", &vis).is_none());
    }

    #[test]
    fn project_message_item_group_visible_sender_kept() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let msg = make_enriched(
            "wxid_normal",
            "group@chatroom",
            wx_db::MessageContent::Text("hi".into()),
        );
        assert!(project_message_item(msg, "group@chatroom", &vis).is_some());
    }

    #[test]
    fn project_message_item_quote_redaction_hidden_refer() {
        let vis = vis_with_hidden_persons(&["wxid_hidden"]);
        let raw_xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_hidden</fromusr><content>secret</content></refermsg></appmsg></msg>"#;
        let content = wx_db::MessageContent::Quote {
            reply_text: Some("my reply".to_string()),
            refer_sender: Some("Hidden User".to_string()),
            refer_content: Some("secret".to_string()),
            refer_type: Some(1),
            raw_xml: raw_xml.to_string(),
        };
        let msg = make_enriched("wxid_visible", "group@chatroom", content);
        let result = project_message_item(msg, "group@chatroom", &vis).unwrap();

        match &result.message.content {
            wx_db::MessageContent::Quote {
                reply_text,
                refer_sender,
                refer_content,
                raw_xml,
                ..
            } => {
                assert_eq!(reply_text.as_deref(), Some("my reply"));
                assert!(refer_sender.is_none());
                assert!(refer_content.is_none());
                assert!(raw_xml.is_empty());
            }
            _ => panic!("expected Quote"),
        }
        // Snippet should degrade to "[引用] my reply"
        assert!(result.snippet.contains("my reply"));
        assert!(!result.snippet.contains("Hidden User"));
    }

    #[test]
    fn project_message_item_quote_visible_refer_preserved() {
        let vis = vis_with_hidden_persons(&["wxid_hidden"]);
        let raw_xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_normal</fromusr><content>visible msg</content></refermsg></appmsg></msg>"#;
        let content = wx_db::MessageContent::Quote {
            reply_text: Some("ok".to_string()),
            refer_sender: Some("Normal User".to_string()),
            refer_content: Some("visible msg".to_string()),
            refer_type: Some(1),
            raw_xml: raw_xml.to_string(),
        };
        let msg = make_enriched("wxid_visible", "group@chatroom", content);
        let result = project_message_item(msg, "group@chatroom", &vis).unwrap();

        match &result.message.content {
            wx_db::MessageContent::Quote {
                refer_sender,
                refer_content,
                ..
            } => {
                assert_eq!(refer_sender.as_deref(), Some("Normal User"));
                assert_eq!(refer_content.as_deref(), Some("visible msg"));
            }
            _ => panic!("expected Quote"),
        }
    }

    #[test]
    fn project_message_item_quote_redaction_group_chatusr() {
        // Real WeChat group chat XML: <fromusr> is chatroom, <chatusr> is actual sender
        let vis = vis_with_hidden_persons(&["wxid_hidden"]);
        let raw_xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>group@chatroom</fromusr><chatusr>wxid_hidden</chatusr><displayname>Hidden User</displayname><content>secret</content></refermsg></appmsg></msg>"#;
        let content = wx_db::MessageContent::Quote {
            reply_text: Some("my reply".to_string()),
            refer_sender: Some("Hidden User".to_string()),
            refer_content: Some("secret".to_string()),
            refer_type: Some(1),
            raw_xml: raw_xml.to_string(),
        };
        let msg = make_enriched("wxid_visible", "group@chatroom", content);
        let result = project_message_item(msg, "group@chatroom", &vis).unwrap();

        match &result.message.content {
            wx_db::MessageContent::Quote {
                reply_text,
                refer_sender,
                refer_content,
                raw_xml,
                ..
            } => {
                assert_eq!(reply_text.as_deref(), Some("my reply"));
                assert!(refer_sender.is_none(), "refer_sender should be redacted");
                assert!(refer_content.is_none(), "refer_content should be redacted");
                assert!(raw_xml.is_empty(), "raw_xml should be cleared");
            }
            _ => panic!("expected Quote"),
        }
        assert!(result.snippet.contains("my reply"));
        assert!(!result.snippet.contains("Hidden User"));
    }

    #[test]
    fn project_message_items_show_hidden_bypasses() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let items = vec![
            make_enriched(
                "wxid_spam",
                "group@chatroom",
                wx_db::MessageContent::Text("spam".into()),
            ),
            make_enriched(
                "wxid_normal",
                "group@chatroom",
                wx_db::MessageContent::Text("hi".into()),
            ),
        ];
        let result = project_message_items(items, "group@chatroom", &vis, true);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn project_message_items_filters_hidden_sender() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let items = vec![
            make_enriched(
                "wxid_spam",
                "group@chatroom",
                wx_db::MessageContent::Text("spam".into()),
            ),
            make_enriched(
                "wxid_normal",
                "group@chatroom",
                wx_db::MessageContent::Text("hi".into()),
            ),
        ];
        let result = project_message_items(items, "group@chatroom", &vis, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message.sender, "wxid_normal");
    }

    #[test]
    fn project_session_sender_non_group_noop() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let mut session = EnrichedSession {
            session: Session {
                username: "wxid_spam".to_string(),
                summary: "hello".to_string(),
                sort_timestamp: 1,
                last_msg_type: Some(1),
                last_msg_sender: Some("wxid_spam".to_string()),
                last_sender_display_name: None,
            },
            display_name: "Spam".to_string(),
            direction: Some(Direction::Incoming),
            detected_at: None,
        };
        project_session_sender(&mut session, &vis);
        assert_eq!(session.session.summary, "hello");
        assert!(session.session.last_msg_sender.is_some());
    }

    #[test]
    fn project_session_sender_group_hidden_sender_redacted() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let mut session = EnrichedSession {
            session: Session {
                username: "group@chatroom".to_string(),
                summary: "spam message".to_string(),
                sort_timestamp: 1,
                last_msg_type: Some(1),
                last_msg_sender: Some("wxid_spam".to_string()),
                last_sender_display_name: Some("Spammer".to_string()),
            },
            display_name: "Group".to_string(),
            direction: Some(Direction::Incoming),
            detected_at: None,
        };
        project_session_sender(&mut session, &vis);
        assert_eq!(session.session.summary, "[消息已隐藏]");
        assert!(session.session.last_msg_sender.is_none());
        assert!(session.session.last_sender_display_name.is_none());
        assert!(session.direction.is_none());
    }

    #[test]
    fn project_session_sender_group_visible_sender_kept() {
        let vis = vis_with_hidden_persons(&["wxid_spam"]);
        let mut session = EnrichedSession {
            session: Session {
                username: "group@chatroom".to_string(),
                summary: "normal message".to_string(),
                sort_timestamp: 1,
                last_msg_type: Some(1),
                last_msg_sender: Some("wxid_normal".to_string()),
                last_sender_display_name: Some("Normal".to_string()),
            },
            display_name: "Group".to_string(),
            direction: Some(Direction::Incoming),
            detected_at: None,
        };
        project_session_sender(&mut session, &vis);
        assert_eq!(session.session.summary, "normal message");
        assert_eq!(
            session.session.last_msg_sender.as_deref(),
            Some("wxid_normal")
        );
    }
}
