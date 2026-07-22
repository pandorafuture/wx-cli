use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ContextError;

struct ResolvedContact {
    display_name: String,
    remark: String,
    nick_name: String,
    alias: String,
    user_name: String,
    phone: Option<String>,
    memo: Option<String>,
    signature: Option<String>,
    region: Option<String>,
    labels: Vec<String>,
    avatar_url: Option<String>,
}

pub struct ContactResolver {
    contacts: HashMap<String, ResolvedContact>,
}

impl ContactResolver {
    pub fn empty() -> Self {
        Self {
            contacts: HashMap::new(),
        }
    }

    /// 从 WechatDb 构建映射表。
    /// 显示名优先级：remark > nick_name > alias > username。
    pub fn build(db: &wx_db::WechatDb) -> Result<Self, ContextError> {
        const BATCH: usize = 10_000;
        let mut contacts = HashMap::new();
        let mut offset = 0;

        loop {
            let query = wx_db::ContactQuery::new().limit(BATCH).offset(offset);
            let result = db.query_contacts(&query)?;
            let count = result.items.len();

            for c in result.items {
                let display_name = effective_display_name(&c);
                let wxid = c.user_name.clone();
                contacts.insert(
                    wxid,
                    ResolvedContact {
                        display_name,
                        remark: c.remark,
                        nick_name: c.nick_name,
                        alias: c.alias,
                        user_name: c.user_name,
                        phone: c.phone,
                        memo: c.memo,
                        signature: c.signature,
                        region: c.region,
                        labels: c.labels,
                        avatar_url: c.avatar_url,
                    },
                );
            }

            if count < BATCH {
                break;
            }
            offset += count;
        }

        Ok(Self { contacts })
    }

    /// 解析 wxid → 显示名，未找到返回 None。
    pub fn resolve(&self, wxid: &str) -> Option<&str> {
        self.contacts.get(wxid).map(|r| r.display_name.as_str())
    }

    /// 解析 wxid → 显示名，未找到时返回原始 wxid。
    pub fn display_name<'a>(&'a self, wxid: &'a str) -> &'a str {
        self.resolve(wxid).unwrap_or(wxid)
    }

    /// Check if a wxid exists in the contact index.
    pub fn contains(&self, wxid: &str) -> bool {
        self.contacts.contains_key(wxid)
    }

    /// Get labels for a contact. Returns empty slice if not found.
    pub fn labels(&self, wxid: &str) -> &[String] {
        self.contacts
            .get(wxid)
            .map(|r| r.labels.as_slice())
            .unwrap_or(&[])
    }

    /// Resolve a wxid to its preferred avatar URL.
    pub fn avatar_url(&self, wxid: &str) -> Option<&str> {
        self.contacts
            .get(wxid)
            .and_then(|contact| contact.avatar_url.as_deref())
    }

    /// Iterate all contacts with their wxid and labels.
    /// Used by VisibilityIndex to expand ignore_tags.
    pub fn all_labels(&self) -> impl Iterator<Item = (&String, &[String])> {
        self.contacts
            .iter()
            .map(|(wxid, r)| (wxid, r.labels.as_slice()))
    }

    /// 格式化为 "显示名（wxid）"，显示名与 wxid 相同时只返回 wxid。
    pub fn display_with_id(&self, wxid: &str) -> String {
        match self.resolve(wxid) {
            Some(name) if name != wxid => format!("{name}（{wxid}）"),
            _ => wxid.to_string(),
        }
    }

    /// 反向模糊匹配：从所有字段查找候选 wxid。
    /// 返回 (display_name, wxid) 列表。
    pub fn find_candidates(&self, keyword: &str) -> Vec<(&str, &str)> {
        let kw_lower = keyword.to_lowercase();
        self.contacts
            .iter()
            .filter(|(_, r)| {
                r.display_name.to_lowercase().contains(&kw_lower)
                    || r.remark.to_lowercase().contains(&kw_lower)
                    || r.nick_name.to_lowercase().contains(&kw_lower)
                    || r.alias.to_lowercase().contains(&kw_lower)
                    || r.user_name.to_lowercase().contains(&kw_lower)
                    || r.phone
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&kw_lower))
                    || r.memo
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&kw_lower))
                    || r.signature
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&kw_lower))
                    || r.region
                        .as_deref()
                        .is_some_and(|s| s.to_lowercase().contains(&kw_lower))
                    || r.labels
                        .iter()
                        .any(|l| l.to_lowercase().contains(&kw_lower))
            })
            .map(|(wxid, r)| (r.display_name.as_str(), wxid.as_str()))
            .collect()
    }
}

fn effective_display_name(c: &wx_db::Contact) -> String {
    if !c.remark.is_empty() {
        c.remark.clone()
    } else if !c.nick_name.is_empty() {
        c.nick_name.clone()
    } else if !c.alias.is_empty() {
        c.alias.clone()
    } else {
        c.user_name.clone()
    }
}

/// 消息方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
}

impl Direction {
    /// 从 sender 与 self_wxid 的比较判断方向。
    pub fn detect(sender: &str, self_wxid: &str) -> Self {
        if sender == self_wxid {
            Direction::Outgoing
        } else {
            Direction::Incoming
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Incoming => "incoming",
            Direction::Outgoing => "outgoing",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_resolver(entries: &[(&str, &str)]) -> ContactResolver {
        let contacts = entries
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    ResolvedContact {
                        display_name: v.to_string(),
                        remark: v.to_string(),
                        nick_name: String::new(),
                        alias: String::new(),
                        user_name: k.to_string(),
                        phone: None,
                        memo: None,
                        signature: None,
                        region: None,
                        labels: Vec::new(),
                        avatar_url: None,
                    },
                )
            })
            .collect();
        ContactResolver { contacts }
    }

    #[allow(clippy::type_complexity)]
    fn make_resolver_full(
        entries: &[(
            &str,         // wxid
            &str,         // display_name / remark
            &str,         // nick_name
            &str,         // alias
            Option<&str>, // phone
            Option<&str>, // memo
            &[&str],      // labels
        )],
    ) -> ContactResolver {
        let contacts = entries
            .iter()
            .map(|(wxid, remark, nick, alias, phone, memo, labels)| {
                (
                    wxid.to_string(),
                    ResolvedContact {
                        display_name: remark.to_string(),
                        remark: remark.to_string(),
                        nick_name: nick.to_string(),
                        alias: alias.to_string(),
                        user_name: wxid.to_string(),
                        phone: phone.map(|s| s.to_string()),
                        memo: memo.map(|s| s.to_string()),
                        signature: None,
                        region: None,
                        labels: labels.iter().map(|s| s.to_string()).collect(),
                        avatar_url: None,
                    },
                )
            })
            .collect();
        ContactResolver { contacts }
    }

    #[test]
    fn display_name_found() {
        let r = make_resolver(&[("wxid_abc", "张三")]);
        assert_eq!(r.display_name("wxid_abc"), "张三");
    }

    #[test]
    fn display_name_not_found() {
        let r = make_resolver(&[]);
        assert_eq!(r.display_name("wxid_abc"), "wxid_abc");
    }

    #[test]
    fn display_with_id_format() {
        let r = make_resolver(&[("wxid_abc", "张三")]);
        assert_eq!(r.display_with_id("wxid_abc"), "张三（wxid_abc）");
    }

    #[test]
    fn display_with_id_same_as_wxid() {
        let r = make_resolver(&[("wxid_abc", "wxid_abc")]);
        assert_eq!(r.display_with_id("wxid_abc"), "wxid_abc");
    }

    #[test]
    fn find_candidates_case_insensitive() {
        let r = make_resolver(&[("wxid_a", "Alice"), ("wxid_b", "Bob")]);
        let c = r.find_candidates("ali");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0], ("Alice", "wxid_a"));
    }

    #[test]
    fn find_candidates_matches_alias() {
        let r = make_resolver_full(&[(
            "wxid_a",
            "Alice Remark",
            "Alice Nick",
            "alice_alias",
            None,
            None,
            &[],
        )]);
        let c = r.find_candidates("alice_alias");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, "wxid_a");
    }

    #[test]
    fn find_candidates_matches_nick_name() {
        let r =
            make_resolver_full(&[("wxid_a", "Alice Remark", "Alice Nick", "", None, None, &[])]);
        let c = r.find_candidates("Nick");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, "wxid_a");
    }

    #[test]
    fn find_candidates_matches_phone() {
        let r = make_resolver_full(&[("wxid_a", "Alice", "", "", Some("13800138000"), None, &[])]);
        let c = r.find_candidates("138001");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, "wxid_a");
    }

    #[test]
    fn find_candidates_matches_label() {
        let r = make_resolver_full(&[("wxid_a", "Alice", "", "", None, None, &["体育生", "同事"])]);
        let c = r.find_candidates("体育");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, "wxid_a");
    }

    #[test]
    fn find_candidates_matches_memo() {
        let r = make_resolver_full(&[("wxid_a", "Alice", "", "", None, Some("QQ 442007516"), &[])]);
        let c = r.find_candidates("442007");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, "wxid_a");
    }

    #[test]
    fn direction_detect() {
        assert_eq!(Direction::detect("wxid_me", "wxid_me"), Direction::Outgoing);
        assert_eq!(
            Direction::detect("wxid_other", "wxid_me"),
            Direction::Incoming
        );
    }

    #[test]
    fn direction_detect_legacy_non_wxid_self_id() {
        assert_eq!(
            Direction::detect("testuser001", "testuser001"),
            Direction::Outgoing
        );
        assert_eq!(
            Direction::detect("wxid_friend", "testuser001"),
            Direction::Incoming
        );
    }
}
