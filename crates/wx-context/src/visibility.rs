use std::collections::HashSet;

use crate::ContactResolver;

/// Compiled visibility index for talker-level and sender-level hiding.
///
/// Built once from account settings + ContactResolver.
/// `ignore_contacts` and `ignore_tags` cascade to both talker-level and sender-level hiding
/// through a single `hidden_persons` set.
pub struct VisibilityIndex {
    hidden_persons: HashSet<String>,
}

impl VisibilityIndex {
    /// Empty index that hides nothing.
    pub fn empty() -> Self {
        Self {
            hidden_persons: HashSet::new(),
        }
    }

    /// Build from raw rule lists and a ContactResolver.
    ///
    /// - `ignore_contacts`: direct wxid/chatroom IDs to hide
    /// - `ignore_tags`: tag names; any contact matching these tags is hidden
    ///
    /// Both `ignore_contacts` and tag-expanded contacts cascade to sender-level hiding
    /// within visible group chats.
    pub fn build(
        ignore_contacts: &[String],
        ignore_tags: &[String],
        resolver: &ContactResolver,
    ) -> Self {
        let mut hidden_persons: HashSet<String> = ignore_contacts.iter().cloned().collect();

        if !ignore_tags.is_empty() {
            let tag_set: HashSet<&str> = ignore_tags.iter().map(|s| s.as_str()).collect();
            for (wxid, labels) in resolver.all_labels() {
                if labels.iter().any(|l| tag_set.contains(l.as_str())) {
                    hidden_persons.insert(wxid.clone());
                }
            }
        }

        Self { hidden_persons }
    }

    /// Whether this talker is hidden.
    pub fn is_hidden_talker(&self, talker: &str) -> bool {
        self.hidden_persons.contains(talker)
    }

    /// Whether a target talker can be resolved (i.e. is not hidden).
    /// Used by the shared contact resolution entry point.
    pub fn can_resolve_target(&self, talker: &str) -> bool {
        !self.hidden_persons.contains(talker)
    }

    /// Whether media access is allowed for this talker.
    pub fn allows_media(&self, talker: &str) -> bool {
        !self.hidden_persons.contains(talker)
    }

    /// Whether this sender should be hidden within a visible group chat.
    ///
    /// Returns true only when:
    /// - talker ends with `@chatroom` (is a group)
    /// - talker is NOT in hidden_persons (the group itself is visible)
    /// - sender IS in hidden_persons
    pub fn is_hidden_sender_in_group(&self, talker: &str, sender: &str) -> bool {
        wx_db::is_group_chat(talker)
            && !self.hidden_persons.contains(talker)
            && self.hidden_persons.contains(sender)
    }

    /// Whether media access is allowed for this talker + sender combination.
    ///
    /// Hidden talkers OR hidden senders in visible groups cannot access media.
    pub fn allows_media_for_sender(&self, talker: &str, sender: &str) -> bool {
        !self.hidden_persons.contains(talker) && !self.is_hidden_sender_in_group(talker, sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;

    use rusqlite::{params, Connection};
    use tempfile::TempDir;
    use wx_db::{encode_extra_buffer_for_test, WechatDb};

    #[test]
    fn empty_index_hides_nothing() {
        let idx = VisibilityIndex::empty();
        assert!(!idx.is_hidden_talker("wxid_test"));
        assert!(idx.can_resolve_target("wxid_test"));
        assert!(idx.allows_media("wxid_test"));
    }

    #[test]
    fn direct_ignore_contacts() {
        let idx = VisibilityIndex {
            hidden_persons: vec!["wxid_hidden".to_string(), "group@chatroom".to_string()]
                .into_iter()
                .collect(),
        };
        assert!(idx.is_hidden_talker("wxid_hidden"));
        assert!(idx.is_hidden_talker("group@chatroom"));
        assert!(!idx.can_resolve_target("wxid_hidden"));
        assert!(!idx.allows_media("wxid_hidden"));
        assert!(!idx.is_hidden_talker("wxid_visible"));
        assert!(idx.can_resolve_target("wxid_visible"));
    }

    #[test]
    fn build_expands_ignore_tags_to_matching_contact_talkers_only() {
        let fixture = create_contact_fixture(&[
            ("wxid_tagged", "Tagged", Some("1")),
            ("team@chatroom", "Group", None),
            ("wxid_visible", "Visible", None),
        ]);
        let db = WechatDb::open(fixture.path()).unwrap();
        let resolver = ContactResolver::build(&db).unwrap();

        let idx = VisibilityIndex::build(&[], &["Sensitive".to_string()], &resolver);

        assert!(idx.is_hidden_talker("wxid_tagged"));
        assert!(!idx.is_hidden_talker("team@chatroom"));
        assert!(!idx.is_hidden_talker("wxid_visible"));
    }

    fn create_contact_fixture(entries: &[(&str, &str, Option<&str>)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let contact_dir = dir.path().join("contact");
        let session_dir = dir.path().join("session");
        let message_dir = dir.path().join("message");
        fs::create_dir_all(&contact_dir).unwrap();
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&message_dir).unwrap();

        let contact_db = contact_dir.join("contact.db");
        let conn = Connection::open(&contact_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE contact (
                username TEXT PRIMARY KEY,
                alias TEXT DEFAULT '',
                remark TEXT DEFAULT '',
                nick_name TEXT DEFAULT '',
                description TEXT DEFAULT NULL,
                extra_buffer BLOB DEFAULT NULL
            );
            CREATE TABLE contact_label (
                label_id_ TEXT,
                label_name_ TEXT,
                sort_order_ INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contact_label VALUES (?1, ?2, ?3)",
            params!["1", "Sensitive", 0],
        )
        .unwrap();

        for (username, nickname, label_ids_csv) in entries {
            let extra_buffer = label_ids_csv.map(|ids| {
                encode_extra_buffer_for_test(None, None, None, None, None, None, None, Some(ids))
            });
            conn.execute(
                "INSERT INTO contact (username, nick_name, extra_buffer) VALUES (?1, ?2, ?3)",
                params![username, nickname, extra_buffer],
            )
            .unwrap();
        }

        create_empty_session_db(&session_dir.join("session.db"));
        dir
    }

    fn create_empty_session_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE SessionTable (
                username TEXT,
                sort_timestamp INTEGER,
                summary TEXT,
                last_msg_type INTEGER DEFAULT NULL,
                last_msg_sender TEXT DEFAULT NULL,
                last_sender_display_name TEXT DEFAULT NULL
            );",
        )
        .unwrap();
    }

    // --- Sender-level hiding tests ---

    #[test]
    fn is_hidden_sender_in_group_non_chatroom_returns_false() {
        let idx = VisibilityIndex {
            hidden_persons: vec!["wxid_spam".to_string()].into_iter().collect(),
        };
        // Private chat — sender hiding does not apply
        assert!(!idx.is_hidden_sender_in_group("wxid_spam", "wxid_spam"));
    }

    #[test]
    fn is_hidden_sender_in_group_hidden_talker_returns_false() {
        let idx = VisibilityIndex {
            hidden_persons: vec!["group@chatroom".to_string(), "wxid_spam".to_string()]
                .into_iter()
                .collect(),
        };
        // Talker-level hiding already handles this group — sender check not needed
        assert!(!idx.is_hidden_sender_in_group("group@chatroom", "wxid_spam"));
    }

    #[test]
    fn is_hidden_sender_in_group_visible_group_hidden_sender() {
        let idx = VisibilityIndex {
            hidden_persons: vec!["wxid_spam".to_string()].into_iter().collect(),
        };
        assert!(idx.is_hidden_sender_in_group("group@chatroom", "wxid_spam"));
        assert!(!idx.is_hidden_sender_in_group("group@chatroom", "wxid_visible"));
    }

    #[test]
    fn allows_media_for_sender_covers_both_levels() {
        let idx = VisibilityIndex {
            hidden_persons: vec!["hidden_group@chatroom".to_string(), "wxid_spam".to_string()]
                .into_iter()
                .collect(),
        };
        // Hidden talker → no media
        assert!(!idx.allows_media_for_sender("hidden_group@chatroom", "wxid_anyone"));
        // Visible group + hidden sender → no media
        assert!(!idx.allows_media_for_sender("visible@chatroom", "wxid_spam"));
        // Visible group + visible sender → media allowed
        assert!(idx.allows_media_for_sender("visible@chatroom", "wxid_normal"));
        // Hidden person as talker in private chat → no media (unified model)
        assert!(!idx.allows_media_for_sender("wxid_spam", "wxid_spam"));
        // Non-hidden person in private chat → media allowed
        assert!(idx.allows_media_for_sender("wxid_normal", "wxid_normal"));
    }

    #[test]
    fn build_expands_ignore_tags_to_sender_level() {
        let fixture = create_contact_fixture(&[
            ("wxid_tagged", "Tagged", Some("1")),
            ("team@chatroom", "Group", None),
            ("wxid_visible", "Visible", None),
        ]);
        let db = WechatDb::open(fixture.path()).unwrap();
        let resolver = ContactResolver::build(&db).unwrap();

        let idx = VisibilityIndex::build(&[], &["Sensitive".to_string()], &resolver);

        // Tag-expanded contact should trigger sender-level hiding in visible groups
        assert!(idx.is_hidden_sender_in_group("team@chatroom", "wxid_tagged"));
        // Non-tagged contacts should not be hidden
        assert!(!idx.is_hidden_sender_in_group("team@chatroom", "wxid_visible"));
    }
}
