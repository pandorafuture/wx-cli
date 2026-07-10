//! XML field extraction for app messages (type=49) and system messages (type=10000).
//!
//! Pure string-based extraction using `str::find` — no regex or XML parser dependencies.
//! All functions are `pub(crate)` and never panic on malformed input.

use crate::model::*;

// --- Helper functions ---

/// Extract the text content of a single XML tag from the given string.
///
/// Handles CDATA sections and HTML entity unescaping. Returns `None` if:
/// - The tag is not found
/// - The tag content is empty
pub(crate) fn extract_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);

    let start_idx = xml.find(&open)?;
    let content_start = start_idx + open.len();
    let end_idx = xml[content_start..].find(&close)?;
    let raw = &xml[content_start..content_start + end_idx];

    // Strip CDATA wrapper if present
    let text = if let Some(inner) = raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
    {
        inner
    } else {
        raw
    };

    if text.is_empty() {
        return None;
    }

    Some(unescape_xml_entities(text))
}

/// Unescape the five standard XML entities.
fn unescape_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Filter out the literal string "null" used by WeChat as a sentinel for empty values.
/// Apply only to display fields (title, des) where "null" is never a valid value.
fn filter_null_sentinel(value: Option<String>) -> Option<String> {
    value.filter(|s| s != "null")
}

// --- Extraction structs ---

pub(crate) struct QuoteFields {
    pub reply_text: Option<String>,
    pub refer_sender: Option<String>,
    pub refer_display_name: Option<String>,
    pub refer_content: Option<String>,
    pub refer_type: Option<u32>,
}

pub(crate) struct TransferFields {
    pub amount_desc: Option<String>,
    pub pay_memo: Option<String>,
    pub pay_sub_type: Option<u32>,
}

pub(crate) struct FileFields {
    pub title: Option<String>,
    pub file_ext: Option<String>,
    pub file_size: Option<u64>,
    pub md5: Option<String>,
}

// --- Extraction functions ---

/// Extract the wxid of the quoted message's original sender from raw_xml.
///
/// In group chats, `<fromusr>` contains the chatroom ID (not the sender wxid),
/// while `<chatusr>` contains the actual sender's wxid. This function prefers
/// `<chatusr>` when present, falling back to `<fromusr>` for private chats.
///
/// This is needed because `MessageContent::Quote.refer_sender` stores the display name
/// (merged from `refer_display_name.or(refer_sender)`), not the wxid.
pub fn extract_quote_fromusr(raw_xml: &str) -> Option<String> {
    let refermsg_block = extract_inner_block(raw_xml, "refermsg")?;
    extract_tag_text(refermsg_block, "chatusr")
        .or_else(|| extract_tag_text(refermsg_block, "fromusr"))
}

/// Extract common app fields: title, des, url.
pub(crate) fn extract_app_fields(xml: &str) -> (Option<String>, Option<String>, Option<String>) {
    (
        filter_null_sentinel(extract_tag_text(xml, "title")),
        filter_null_sentinel(extract_tag_text(xml, "des")),
        extract_tag_text(xml, "url"),
    )
}

/// Extract fields for quote/reply messages (sub_type=57).
pub(crate) fn extract_quote_fields(xml: &str) -> QuoteFields {
    let reply_text = extract_tag_text(xml, "title");

    // Extract refermsg block, then extract fields within it
    let refermsg_block = extract_inner_block(xml, "refermsg");
    let (refer_sender, refer_display_name, refer_content, refer_type) =
        if let Some(block) = refermsg_block {
            let sender = extract_tag_text(block, "fromusr");
            let display = extract_tag_text(block, "displayname");
            let content = extract_tag_text(block, "content");
            let rtype = extract_tag_text(block, "type").and_then(|s| s.parse::<u32>().ok());

            // If the referred message contains raw XML, convert to readable placeholder
            let content = match (&content, rtype) {
                (Some(xml), Some(49)) if xml.contains("<appmsg") => {
                    extract_tag_text(xml, "title").or(content)
                }
                (Some(xml), Some(3)) if xml.contains("<img") => Some("[图片]".to_string()),
                (Some(xml), Some(43)) if xml.contains("<videomsg") => Some("[视频]".to_string()),
                (Some(xml), Some(47)) if xml.contains("<emoji") => Some("[表情]".to_string()),
                _ => content,
            };

            (sender, display, content, rtype)
        } else {
            (None, None, None, None)
        };

    QuoteFields {
        reply_text,
        refer_sender,
        refer_display_name,
        refer_content,
        refer_type,
    }
}

/// Extract fields for transfer messages (sub_type=2000).
pub(crate) fn extract_transfer_fields(xml: &str) -> TransferFields {
    let pay_block = extract_inner_block(xml, "wcpayinfo");
    if let Some(block) = pay_block {
        TransferFields {
            amount_desc: extract_tag_text(block, "feedesc"),
            pay_memo: extract_tag_text(block, "pay_memo"),
            pay_sub_type: extract_tag_text(block, "paysubtype").and_then(|s| s.parse().ok()),
        }
    } else {
        TransferFields {
            amount_desc: None,
            pay_memo: None,
            pay_sub_type: None,
        }
    }
}

/// Extract fields for file messages (sub_type=6).
pub(crate) fn extract_file_fields(xml: &str) -> FileFields {
    FileFields {
        title: extract_tag_text(xml, "title"),
        file_ext: extract_tag_text(xml, "fileext"),
        file_size: extract_tag_text(xml, "totallen").and_then(|s| s.parse().ok()),
        md5: extract_tag_text(xml, "md5"),
    }
}

/// Extract the inner content (including nested tags) of a block element.
fn extract_inner_block<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let inner_start = start + open.len();
    let end = xml[inner_start..].find(&close)?;
    Some(&xml[inner_start..inner_start + end])
}

// --- Dispatch ---

/// Parse an app message (type=49) XML into a typed MessageContent variant.
pub(crate) fn dispatch_app_message(sub_type: u32, xml: &str) -> MessageContent {
    match sub_type {
        // Link (5) and link-like types (4, 7, 92 for music)
        APP_SUB_TYPE_LINK | 4 | 7 => {
            let (title, des, url) = extract_app_fields(xml);
            MessageContent::Link {
                sub_type,
                title,
                des,
                url,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_MUSIC => {
            let (title, des, url) = extract_app_fields(xml);
            MessageContent::Link {
                sub_type,
                title,
                des,
                url,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_FILE => {
            let f = extract_file_fields(xml);
            MessageContent::File {
                title: f.title,
                file_ext: f.file_ext,
                file_size: f.file_size,
                md5: f.md5,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_MINI_PROGRAM | APP_SUB_TYPE_MINI_PROGRAM_2 => {
            let (title, _, url) = extract_app_fields(xml);
            MessageContent::MiniProgram {
                sub_type,
                title,
                url,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_MERGED => {
            let title = filter_null_sentinel(extract_tag_text(xml, "title"));
            MessageContent::MergedMessages {
                title,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_QUOTE => {
            let q = extract_quote_fields(xml);
            MessageContent::Quote {
                reply_text: q.reply_text,
                refer_sender: q.refer_display_name.or(q.refer_sender),
                refer_content: q.refer_content,
                refer_type: q.refer_type,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_TRANSFER => {
            let t = extract_transfer_fields(xml);
            MessageContent::Transfer {
                amount_desc: t.amount_desc,
                pay_memo: t.pay_memo,
                pay_sub_type: t.pay_sub_type,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_RED_ENVELOPE | 2003 => {
            let title = filter_null_sentinel(extract_tag_text(xml, "title"));
            MessageContent::RedEnvelope {
                title,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_CHANNEL | APP_SUB_TYPE_CHANNEL_LIVE => {
            let title = filter_null_sentinel(extract_tag_text(xml, "title"))
                .or_else(|| filter_null_sentinel(extract_tag_text(xml, "des")));
            MessageContent::ChannelVideo {
                sub_type,
                title,
                raw_xml: xml.to_string(),
            }
        }
        APP_SUB_TYPE_PAT => MessageContent::Pat {
            raw_xml: xml.to_string(),
        },
        _ => {
            let (title, des, url) = extract_app_fields(xml);
            MessageContent::AppGeneric {
                sub_type,
                title,
                des,
                url,
                raw_xml: xml.to_string(),
            }
        }
    }
}

// --- System message extraction ---

/// Try to extract readable text from a system message (type=10000) that may
/// contain `<sysmsg type="revokemsg">` XML.
///
/// Returns `Some(readable_text)` if the content is a revokemsg sysmsg XML
/// and the `<content>` tag inside `<revokemsg>` contains text.
/// Returns `None` if the content is not sysmsg XML or extraction fails,
/// in which case the caller should use the original content as-is.
pub(crate) fn extract_system_message_text(content: &str) -> Option<String> {
    // Quick check: only process XML-like system messages
    if !content.contains("<sysmsg") {
        return None;
    }

    // Extract <content> from within the <revokemsg> block
    if content.contains("type=\"revokemsg\"") || content.contains("type='revokemsg'") {
        if let Some(block) = extract_inner_block(content, "revokemsg") {
            return extract_tag_text(block, "content");
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tag_text_plain() {
        assert_eq!(
            extract_tag_text("<title>Hello World</title>", "title"),
            Some("Hello World".to_string())
        );
    }

    #[test]
    fn extract_tag_text_cdata() {
        assert_eq!(
            extract_tag_text("<title><![CDATA[Hello]]></title>", "title"),
            Some("Hello".to_string())
        );
    }

    #[test]
    fn extract_tag_text_entities() {
        assert_eq!(
            extract_tag_text("<title>A&amp;B&lt;C&gt;D</title>", "title"),
            Some("A&B<C>D".to_string())
        );
    }

    #[test]
    fn extract_tag_text_empty() {
        assert_eq!(extract_tag_text("<title></title>", "title"), None);
    }

    #[test]
    fn extract_tag_text_missing() {
        assert_eq!(extract_tag_text("<xml>no title here</xml>", "title"), None);
    }

    #[test]
    fn extract_tag_text_cdata_with_entities() {
        // CDATA content should NOT be entity-unescaped (it's literal),
        // but we still run unescape for simplicity — in practice CDATA
        // won't contain XML entities.
        assert_eq!(
            extract_tag_text("<des><![CDATA[A&B]]></des>", "des"),
            Some("A&B".to_string())
        );
    }

    #[test]
    fn extract_app_fields_full() {
        let xml = r#"<msg><appmsg><title>Test Title</title><des>Description</des><url>https://example.com</url></appmsg></msg>"#;
        let (title, des, url) = extract_app_fields(xml);
        assert_eq!(title, Some("Test Title".to_string()));
        assert_eq!(des, Some("Description".to_string()));
        assert_eq!(url, Some("https://example.com".to_string()));
    }

    #[test]
    fn extract_app_fields_filters_null_title() {
        let xml = r#"<msg><appmsg><title>null</title><des>null</des><url>https://example.com</url></appmsg></msg>"#;
        let (title, des, url) = extract_app_fields(xml);
        assert_eq!(title, None);
        assert_eq!(des, None);
        assert_eq!(url, Some("https://example.com".to_string()));
    }

    #[test]
    fn extract_app_fields_preserves_non_null() {
        let xml = r#"<msg><appmsg><title>nullable</title></appmsg></msg>"#;
        let (title, _, _) = extract_app_fields(xml);
        assert_eq!(title, Some("nullable".to_string()));
    }

    #[test]
    fn extract_tag_text_returns_null_literal() {
        // extract_tag_text itself should NOT filter "null" — that's filter_null_sentinel's job
        assert_eq!(
            extract_tag_text("<title>null</title>", "title"),
            Some("null".to_string())
        );
    }

    #[test]
    fn extract_quote_fields_full() {
        let xml = r#"<msg><appmsg><title>reply text</title><refermsg><fromusr>wxid_alice</fromusr><displayname>Alice</displayname><content>original message</content><type>1</type></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.reply_text, Some("reply text".to_string()));
        assert_eq!(q.refer_sender, Some("wxid_alice".to_string()));
        assert_eq!(q.refer_display_name, Some("Alice".to_string()));
        assert_eq!(q.refer_content, Some("original message".to_string()));
        assert_eq!(q.refer_type, Some(1));
    }

    #[test]
    fn extract_quote_fields_nested_app() {
        let xml = r#"<msg><appmsg><title>reply text</title><refermsg><fromusr>wxid_bob</fromusr><displayname>Bob</displayname><type>49</type><content><msg><appmsg><title>被引原文</title><des>desc</des></appmsg></msg></content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("被引原文".to_string()));
        assert_eq!(q.refer_type, Some(49));
    }

    #[test]
    fn extract_quote_fields_nested_text() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_alice</fromusr><type>1</type><content>普通文本</content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("普通文本".to_string()));
        assert_eq!(q.refer_type, Some(1));
    }

    #[test]
    fn extract_transfer_fields_full() {
        let xml = r#"<msg><appmsg><wcpayinfo><feedesc>¥66.00</feedesc><pay_memo>lunch</pay_memo><paysubtype>1</paysubtype></wcpayinfo></appmsg></msg>"#;
        let t = extract_transfer_fields(xml);
        assert_eq!(t.amount_desc, Some("¥66.00".to_string()));
        assert_eq!(t.pay_memo, Some("lunch".to_string()));
        assert_eq!(t.pay_sub_type, Some(1));
    }

    #[test]
    fn extract_file_fields_full() {
        let xml = r#"<msg><appmsg><title>report.pdf</title><fileext>pdf</fileext><totallen>12345</totallen><md5>abc123</md5></appmsg></msg>"#;
        let f = extract_file_fields(xml);
        assert_eq!(f.title, Some("report.pdf".to_string()));
        assert_eq!(f.file_ext, Some("pdf".to_string()));
        assert_eq!(f.file_size, Some(12345));
        assert_eq!(f.md5, Some("abc123".to_string()));
    }

    #[test]
    fn dispatch_link_message() {
        let xml = r#"<msg><appmsg><title>Article</title><des>Desc</des><url>https://mp.weixin.qq.com</url></appmsg></msg>"#;
        match dispatch_app_message(5, xml) {
            MessageContent::Link {
                title, des, url, ..
            } => {
                assert_eq!(title, Some("Article".to_string()));
                assert_eq!(des, Some("Desc".to_string()));
                assert_eq!(url, Some("https://mp.weixin.qq.com".to_string()));
            }
            other => panic!("expected Link, got: {:?}", other),
        }
    }

    #[test]
    fn dispatch_channel_video_fallback_des() {
        let xml = r#"<msg><appmsg><des>今日新闻</des></appmsg></msg>"#;
        match dispatch_app_message(APP_SUB_TYPE_CHANNEL, xml) {
            MessageContent::ChannelVideo { title, .. } => {
                assert_eq!(title, Some("今日新闻".to_string()));
            }
            other => panic!("expected ChannelVideo, got: {:?}", other),
        }
    }

    #[test]
    fn dispatch_channel_video_title_preferred_over_des() {
        let xml = r#"<msg><appmsg><title>视频标题</title><des>描述</des></appmsg></msg>"#;
        match dispatch_app_message(APP_SUB_TYPE_CHANNEL, xml) {
            MessageContent::ChannelVideo { title, .. } => {
                assert_eq!(title, Some("视频标题".to_string()));
            }
            other => panic!("expected ChannelVideo, got: {:?}", other),
        }
    }

    #[test]
    fn dispatch_unknown_sub_type() {
        let xml = "<msg><appmsg><title>Unknown</title></appmsg></msg>";
        match dispatch_app_message(9999, xml) {
            MessageContent::AppGeneric { sub_type, .. } => {
                assert_eq!(sub_type, 9999);
            }
            other => panic!("expected AppGeneric, got: {:?}", other),
        }
    }

    #[test]
    fn extract_quote_refer_image() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_alice</fromusr><type>3</type><content>&lt;?xml version="1.0"?&gt;&lt;msg&gt;&lt;img aeskey="abc" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("[图片]".to_string()));
        assert_eq!(q.refer_type, Some(3));
    }

    #[test]
    fn extract_quote_refer_video() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_bob</fromusr><type>43</type><content>&lt;msg&gt;&lt;videomsg length="30" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("[视频]".to_string()));
        assert_eq!(q.refer_type, Some(43));
    }

    #[test]
    fn extract_quote_refer_emoji() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_carol</fromusr><type>47</type><content>&lt;msg&gt;&lt;emoji md5="abc123" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("[表情]".to_string()));
        assert_eq!(q.refer_type, Some(47));
    }

    #[test]
    fn extract_quote_refer_image_plain_text_preserved() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_alice</fromusr><type>3</type><content>just plain text</content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("just plain text".to_string()));
    }

    #[test]
    fn extract_quote_refer_image_with_wxid_prefix() {
        // In group chats, content may be prefixed with "wxid_xxx:\n"
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_alice</fromusr><type>3</type><content>wxid_someone:
&lt;?xml version="1.0"?&gt;&lt;msg&gt;&lt;img aeskey="def" /&gt;&lt;/msg&gt;</content></refermsg></appmsg></msg>"#;
        let q = extract_quote_fields(xml);
        assert_eq!(q.refer_content, Some("[图片]".to_string()));
    }

    // --- extract_quote_fromusr tests ---

    #[test]
    fn extract_quote_fromusr_normal() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_alice</fromusr><content>hi</content></refermsg></appmsg></msg>"#;
        assert_eq!(extract_quote_fromusr(xml), Some("wxid_alice".to_string()));
    }

    #[test]
    fn extract_quote_fromusr_missing_refermsg() {
        let xml = r#"<msg><appmsg><title>reply</title></appmsg></msg>"#;
        assert_eq!(extract_quote_fromusr(xml), None);
    }

    #[test]
    fn extract_quote_fromusr_missing_fromusr() {
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><content>hi</content></refermsg></appmsg></msg>"#;
        assert_eq!(extract_quote_fromusr(xml), None);
    }

    #[test]
    fn extract_quote_fromusr_prefers_chatusr_in_group() {
        // In group chats, <fromusr> is the chatroom ID, <chatusr> is the actual sender
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>group@chatroom</fromusr><chatusr>wxid_sender</chatusr><displayname>Sender</displayname><content>hi</content></refermsg></appmsg></msg>"#;
        assert_eq!(extract_quote_fromusr(xml), Some("wxid_sender".to_string()));
    }

    #[test]
    fn extract_quote_fromusr_falls_back_to_fromusr_without_chatusr() {
        // In private chats, only <fromusr> exists (no <chatusr>)
        let xml = r#"<msg><appmsg><title>reply</title><refermsg><fromusr>wxid_bob</fromusr><displayname>Bob</displayname><content>hi</content></refermsg></appmsg></msg>"#;
        assert_eq!(extract_quote_fromusr(xml), Some("wxid_bob".to_string()));
    }
}
