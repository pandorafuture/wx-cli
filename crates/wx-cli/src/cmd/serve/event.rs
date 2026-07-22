use serde::Serialize;

use crate::schema::{EnrichedMessage, EnrichedSession};

/// SSE event types broadcast to connected clients.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    Session(SessionPayload),
    Message(MessagePayload),
    Heartbeat,
}

#[derive(Clone, Serialize)]
pub struct SessionPayload {
    #[serde(flatten)]
    pub enriched: EnrichedSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sort_seq: Option<i64>,
}

#[derive(Clone, Serialize)]
pub struct MessagePayload {
    pub talker: String,
    pub talker_display_name: String,
    pub messages: Vec<EnrichedMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_sort_seq: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use wx_context::Direction;

    #[test]
    fn session_payload_omits_direction_when_unknown() {
        let payload = SessionPayload {
            enriched: EnrichedSession {
                session: wx_db::Session {
                    username: "wxid_friend".to_string(),
                    summary: "hello".to_string(),
                    sort_timestamp: 1,
                    last_msg_type: Some(1),
                    last_msg_sender: None,
                    last_sender_display_name: None,
                },
                display_name: "wxid_friend".to_string(),
                avatar_url: None,
                direction: None,
                detected_at: None,
            },
            last_sort_seq: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("direction").is_none());
    }

    #[test]
    fn session_sse_event_omits_direction_when_unknown() {
        let event = SseEvent::Session(SessionPayload {
            enriched: EnrichedSession {
                session: wx_db::Session {
                    username: "wxid_friend".to_string(),
                    summary: "hello".to_string(),
                    sort_timestamp: 1,
                    last_msg_type: Some(1),
                    last_msg_sender: None,
                    last_sender_display_name: None,
                },
                display_name: "wxid_friend".to_string(),
                avatar_url: None,
                direction: None,
                detected_at: Some(2),
            },
            last_sort_seq: Some(3),
        });

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json.get("type"),
            Some(&Value::String("session".to_string()))
        );
        assert_eq!(json.get("detected_at"), Some(&Value::Number(2.into())));
        assert!(json.get("direction").is_none());
    }

    #[test]
    fn message_payload_keeps_message_direction() {
        let payload = MessagePayload {
            talker: "wxid_friend".to_string(),
            talker_display_name: "wxid_friend".to_string(),
            messages: vec![EnrichedMessage {
                message: wx_db::Message {
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
                sender_display_name: "wxid_me".to_string(),
                direction: Direction::Outgoing,
                snippet: "hello".to_string(),
            }],
            anchor_sort_seq: Some(1),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json.get("messages")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("direction")),
            Some(&Value::String("outgoing".to_string()))
        );
    }
}
