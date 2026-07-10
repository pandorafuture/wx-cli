use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::KeychainError;
use wx_decrypt::{EncKeyPair, KeyMaterial};

/// Serializable enc_key + salt pair for the new per-DB format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncKeyEntry {
    pub enc_key: String,
    pub salt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountKey {
    pub account_id: String,
    pub data_key: String,
    pub extracted_at: DateTime<Utc>,
    pub wechat_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_wxid: Option<String>,
    /// V2 image AES key (16-byte hex string, independent of data_key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_aes_key: Option<String>,
    /// Legacy single pre-derived encryption key (64-char hex = 32 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enc_key: Option<String>,
    /// Legacy DB salt associated with the enc_key (32-char hex = 16 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enc_key_salt: Option<String>,
    /// Per-DB enc_key + salt pairs (new canonical format).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enc_keys: Vec<EncKeyEntry>,
}

impl AccountKey {
    pub fn display_name(&self) -> String {
        match &self.nickname {
            Some(nick) => format!("{} ({})", self.account_id, nick),
            None => format!("{} (昵称未知)", self.account_id),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KeyStore {
    #[serde(default)]
    pub accounts: HashMap<String, AccountKey>,
}

impl KeyStore {
    /// Default path: `<config_dir>/keys.toml`
    pub fn default_path() -> Result<PathBuf, KeychainError> {
        let ap = wx_paths::AppPaths::new().map_err(|e| KeychainError::Other(e.to_string()))?;
        Ok(ap.keys_file())
    }

    /// Load from the default path, creating an empty store if the file doesn't exist.
    pub fn load_default() -> Result<Self, KeychainError> {
        let ap = wx_paths::AppPaths::new().map_err(|e| KeychainError::Other(e.to_string()))?;
        ap.migrate_config()
            .map_err(|e| KeychainError::Other(format!("config migration failed: {}", e)))?;
        let path = ap.keys_file();
        Self::load(&path)
    }

    /// Load from a specific path.
    pub fn load(path: &Path) -> Result<Self, KeychainError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        toml::from_str(&content)
            .map_err(|e| KeychainError::Store(format!("failed to parse {}: {}", path.display(), e)))
    }

    /// Save to the default path.
    pub fn save_default(&self) -> Result<(), KeychainError> {
        let path = Self::default_path()?;
        self.save(&path)
    }

    /// Save to a specific path, creating parent directories as needed.
    ///
    /// Uses atomic write (write to `.tmp` sibling, then `rename`) to prevent
    /// partial TOML files on crash.
    pub fn save(&self, path: &Path) -> Result<(), KeychainError> {
        let parent_existed = path.parent().is_none_or(|p| p.exists());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| KeychainError::Store(format!("failed to serialize: {}", e)))?;

        let tmp_path = path.with_extension("toml.tmp");
        fs::write(&tmp_path, &content)?;
        fs::rename(&tmp_path, path).inspect_err(|_| {
            // Clean up the temp file on rename failure.
            let _ = fs::remove_file(&tmp_path);
        })?;

        wx_paths::sudo::chown_to_sudo_user(path);
        if !parent_existed {
            if let Some(parent) = path.parent() {
                wx_paths::sudo::chown_to_sudo_user(parent);
            }
        }
        Ok(())
    }

    /// Insert or update a key for an account.
    ///
    /// If `hex_key` differs from the existing `data_key`, derived key fields
    /// (`enc_key`, `enc_key_salt`, `enc_keys`) are cleared because they were
    /// derived from the old key and are no longer valid.
    pub fn set(
        &mut self,
        account_id: &str,
        hex_key: &str,
        version: &str,
        nickname: Option<String>,
        base_wxid: Option<String>,
    ) {
        let existing = self.accounts.get(account_id);
        let existing_image_key = existing.and_then(|k| k.image_aes_key.clone());

        let data_key_changed = existing.map(|k| k.data_key != hex_key).unwrap_or(false);

        let (existing_enc_key, existing_enc_key_salt, existing_enc_keys) = if data_key_changed {
            // data_key changed — derived keys are stale, clear them.
            (None, None, Vec::new())
        } else {
            (
                existing.and_then(|k| k.enc_key.clone()),
                existing.and_then(|k| k.enc_key_salt.clone()),
                existing.map(|k| k.enc_keys.clone()).unwrap_or_default(),
            )
        };

        self.accounts.insert(
            account_id.to_string(),
            AccountKey {
                account_id: account_id.to_string(),
                data_key: hex_key.to_string(),
                extracted_at: Utc::now(),
                wechat_version: version.to_string(),
                nickname,
                base_wxid,
                image_aes_key: existing_image_key,
                enc_key: existing_enc_key,
                enc_key_salt: existing_enc_key_salt,
                enc_keys: existing_enc_keys,
            },
        );
    }

    /// Set or update the V2 image AES key for an account.
    pub fn set_image_key(&mut self, account_id: &str, image_key_hex: &str) {
        if let Some(entry) = self.accounts.get_mut(account_id) {
            entry.image_aes_key = Some(image_key_hex.to_string());
        } else {
            // Create a minimal entry if account doesn't exist yet.
            self.accounts.insert(
                account_id.to_string(),
                AccountKey {
                    account_id: account_id.to_string(),
                    data_key: String::new(),
                    extracted_at: Utc::now(),
                    wechat_version: String::new(),
                    nickname: None,
                    base_wxid: None,
                    image_aes_key: Some(image_key_hex.to_string()),
                    enc_key: None,
                    enc_key_salt: None,
                    enc_keys: Vec::new(),
                },
            );
        }
    }

    /// Set or update the pre-derived enc_key and salt from Mach VM scan.
    ///
    /// Preserves existing `data_key` and `image_aes_key` if the account already exists.
    pub fn set_enc_key(
        &mut self,
        account_id: &str,
        enc_key_hex: &str,
        salt_hex: &str,
        version: &str,
        nickname: Option<String>,
        base_wxid: Option<String>,
    ) {
        if let Some(entry) = self.accounts.get_mut(account_id) {
            entry.enc_key = Some(enc_key_hex.to_string());
            entry.enc_key_salt = Some(salt_hex.to_string());
            entry.extracted_at = Utc::now();
            entry.wechat_version = version.to_string();
            if nickname.is_some() {
                entry.nickname = nickname;
            }
            if base_wxid.is_some() {
                entry.base_wxid = base_wxid;
            }
        } else {
            self.accounts.insert(
                account_id.to_string(),
                AccountKey {
                    account_id: account_id.to_string(),
                    data_key: String::new(),
                    extracted_at: Utc::now(),
                    wechat_version: version.to_string(),
                    nickname,
                    base_wxid,
                    image_aes_key: None,
                    enc_key: Some(enc_key_hex.to_string()),
                    enc_key_salt: Some(salt_hex.to_string()),
                    enc_keys: Vec::new(),
                },
            );
        }
    }

    /// Set or update multiple per-DB enc_key + salt pairs from Mach VM scan.
    ///
    /// Deduplicates and sorts entries by `(salt, enc_key)` for stable output.
    /// Clears legacy `enc_key` / `enc_key_salt` fields, making `enc_keys` the canonical format.
    /// Preserves existing `data_key` and `image_aes_key`.
    pub fn set_enc_keys(
        &mut self,
        account_id: &str,
        pairs: &[EncKeyPair],
        version: &str,
        nickname: Option<String>,
        base_wxid: Option<String>,
    ) {
        // Deduplicate and sort
        let mut entries: Vec<EncKeyEntry> = pairs
            .iter()
            .map(|p| EncKeyEntry {
                enc_key: hex::encode(p.key),
                salt: hex::encode(p.salt),
            })
            .collect();
        entries.sort_by(|a, b| a.salt.cmp(&b.salt).then_with(|| a.enc_key.cmp(&b.enc_key)));
        entries.dedup();

        if let Some(entry) = self.accounts.get_mut(account_id) {
            entry.enc_keys = entries;
            entry.enc_key = None;
            entry.enc_key_salt = None;
            entry.extracted_at = Utc::now();
            entry.wechat_version = version.to_string();
            if nickname.is_some() {
                entry.nickname = nickname;
            }
            if base_wxid.is_some() {
                entry.base_wxid = base_wxid;
            }
        } else {
            self.accounts.insert(
                account_id.to_string(),
                AccountKey {
                    account_id: account_id.to_string(),
                    data_key: String::new(),
                    extracted_at: Utc::now(),
                    wechat_version: version.to_string(),
                    nickname,
                    base_wxid,
                    image_aes_key: None,
                    enc_key: None,
                    enc_key_salt: None,
                    enc_keys: entries,
                },
            );
        }
    }

    /// Resolve the best available key material for an account.
    ///
    /// Priority: `enc_keys` (new per-DB format) > `enc_key` + `enc_key_salt` (legacy)
    /// > `data_key` (raw key).
    ///
    /// **Note:** `wx-context` overrides this priority — it always prefers `RawKey`
    /// when `data_key` is present, because `RawKey` can decrypt any DB regardless of
    /// salt, whereas `EncKeys` only cover DBs whose salt was cached in memory at scan
    /// time. Direct callers of this method should be aware of this limitation.
    pub fn resolve_key_material(&self, account_id: &str) -> Option<KeyMaterial> {
        let entry = self.accounts.get(account_id)?;

        // Prefer new per-DB enc_keys format.
        if !entry.enc_keys.is_empty() {
            let pairs: Vec<EncKeyPair> = entry
                .enc_keys
                .iter()
                .filter_map(|e| {
                    let key_bytes = hex::decode(&e.enc_key).ok()?;
                    let salt_bytes = hex::decode(&e.salt).ok()?;
                    if key_bytes.len() == 32 && salt_bytes.len() == 16 {
                        let mut key = [0u8; 32];
                        let mut salt = [0u8; 16];
                        key.copy_from_slice(&key_bytes);
                        salt.copy_from_slice(&salt_bytes);
                        Some(EncKeyPair { key, salt })
                    } else {
                        None
                    }
                })
                .collect();
            if !pairs.is_empty() {
                return Some(KeyMaterial::EncKeys(pairs));
            }
        }

        // Legacy single enc_key path.
        if let (Some(ek), Some(es)) = (&entry.enc_key, &entry.enc_key_salt) {
            if !ek.is_empty() && !es.is_empty() {
                if let (Ok(key_bytes), Ok(salt_bytes)) = (hex::decode(ek), hex::decode(es)) {
                    if key_bytes.len() == 32 && salt_bytes.len() == 16 {
                        let mut key = [0u8; 32];
                        let mut salt = [0u8; 16];
                        key.copy_from_slice(&key_bytes);
                        salt.copy_from_slice(&salt_bytes);
                        return Some(KeyMaterial::EncKey { key, salt });
                    }
                }
            }
        }

        // Fall back to raw data_key.
        if !entry.data_key.is_empty() {
            if let Ok(key_bytes) = hex::decode(&entry.data_key) {
                if key_bytes.len() == 32 {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&key_bytes);
                    return Some(KeyMaterial::RawKey(key));
                }
            }
        }

        None
    }

    /// Get the hex key for an account.
    pub fn get(&self, account_id: &str) -> Option<&AccountKey> {
        self.accounts.get(account_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_old_keys_toml_without_nickname_base_wxid() {
        let toml_str = r#"
[accounts.wxid_test_1234]
account_id = "wxid_test_1234"
data_key = "aabbccdd"
extracted_at = "2025-01-01T00:00:00Z"
wechat_version = "4.1.7.31"
"#;
        let store: KeyStore = toml::from_str(toml_str).unwrap();
        let key = store.get("wxid_test_1234").unwrap();
        assert_eq!(key.nickname, None);
        assert_eq!(key.base_wxid, None);
        assert_eq!(key.display_name(), "wxid_test_1234 (昵称未知)");
    }

    #[test]
    fn test_keys_toml_with_nickname() {
        let toml_str = r#"
[accounts.wxid_test_1234]
account_id = "wxid_test_1234"
data_key = "aabbccdd"
extracted_at = "2025-01-01T00:00:00Z"
wechat_version = "4.1.7.31"
nickname = "TestUser"
base_wxid = "wxid_test"
"#;
        let store: KeyStore = toml::from_str(toml_str).unwrap();
        let key = store.get("wxid_test_1234").unwrap();
        assert_eq!(key.nickname.as_deref(), Some("TestUser"));
        assert_eq!(key.base_wxid.as_deref(), Some("wxid_test"));
        assert_eq!(key.display_name(), "wxid_test_1234 (TestUser)");
    }

    #[test]
    fn test_image_aes_key_roundtrip() {
        let mut store = KeyStore::default();
        store.set("wxid_abc", "aabbccdd", "4.1.7.31", None, None);
        assert_eq!(store.get("wxid_abc").unwrap().image_aes_key, None);

        store.set_image_key("wxid_abc", "6162636465666768696a6b6c6d6e6f70");
        let serialized = toml::to_string_pretty(&store).unwrap();
        let deserialized: KeyStore = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized
                .get("wxid_abc")
                .unwrap()
                .image_aes_key
                .as_deref(),
            Some("6162636465666768696a6b6c6d6e6f70"),
        );
        // data_key preserved
        assert_eq!(deserialized.get("wxid_abc").unwrap().data_key, "aabbccdd");
    }

    #[test]
    fn test_set_preserves_existing_image_key() {
        let mut store = KeyStore::default();
        store.set("wxid_abc", "aabbccdd", "4.1.7.31", None, None);
        store.set_image_key("wxid_abc", "deadbeef12345678");

        // Re-setting data key should preserve existing image key
        store.set(
            "wxid_abc",
            "newdatakey",
            "4.1.7.33",
            Some("Nick".into()),
            None,
        );
        assert_eq!(
            store.get("wxid_abc").unwrap().image_aes_key.as_deref(),
            Some("deadbeef12345678"),
        );
        assert_eq!(store.get("wxid_abc").unwrap().data_key, "newdatakey");
    }

    #[test]
    fn test_old_toml_without_image_key() {
        // Backward compatibility: old TOML without image_aes_key field
        let toml_str = r#"
[accounts.wxid_old]
account_id = "wxid_old"
data_key = "oldkey"
extracted_at = "2025-01-01T00:00:00Z"
wechat_version = "4.1.7.31"
"#;
        let store: KeyStore = toml::from_str(toml_str).unwrap();
        assert_eq!(store.get("wxid_old").unwrap().image_aes_key, None);
    }

    #[test]
    fn test_old_toml_without_enc_key_fields() {
        let toml_str = r#"
[accounts.wxid_old]
account_id = "wxid_old"
data_key = "aabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccdd"
extracted_at = "2025-01-01T00:00:00Z"
wechat_version = "4.1.7.31"
"#;
        let store: KeyStore = toml::from_str(toml_str).unwrap();
        let key = store.get("wxid_old").unwrap();
        assert_eq!(key.enc_key, None);
        assert_eq!(key.enc_key_salt, None);
    }

    #[test]
    fn test_set_clears_enc_key_when_data_key_changes() {
        let mut store = KeyStore::default();
        store.set_enc_key(
            "wxid_abc",
            "aa".repeat(32).as_str(),
            "bb".repeat(16).as_str(),
            "4.1.7.31",
            None,
            None,
        );

        // data_key was empty; setting a new data_key should clear derived fields
        store.set(
            "wxid_abc",
            "cc".repeat(32).as_str(),
            "4.1.8.0",
            Some("Nick".into()),
            None,
        );
        let entry = store.get("wxid_abc").unwrap();
        assert_eq!(entry.enc_key, None, "enc_key cleared on data_key change");
        assert_eq!(
            entry.enc_key_salt, None,
            "enc_key_salt cleared on data_key change"
        );
        assert_eq!(entry.data_key, "cc".repeat(32));
    }

    #[test]
    fn test_set_enc_key_preserves_data_key_and_image_key() {
        let mut store = KeyStore::default();
        let data_key = "dd".repeat(32);
        store.set("wxid_abc", &data_key, "4.1.7.31", None, None);
        store.set_image_key("wxid_abc", "deadbeef12345678");

        store.set_enc_key(
            "wxid_abc",
            "ee".repeat(32).as_str(),
            "ff".repeat(16).as_str(),
            "4.1.8.0",
            Some("Nick".into()),
            None,
        );

        let entry = store.get("wxid_abc").unwrap();
        assert_eq!(entry.data_key, data_key);
        assert_eq!(entry.image_aes_key.as_deref(), Some("deadbeef12345678"));
        assert_eq!(entry.enc_key.as_deref(), Some(&*"ee".repeat(32)));
        assert_eq!(entry.enc_key_salt.as_deref(), Some(&*"ff".repeat(16)));
    }

    #[test]
    fn test_resolve_key_material_prefers_enc_key() {
        let mut store = KeyStore::default();
        let data_key = "ab".repeat(32);
        let enc_key = "cd".repeat(32);
        let enc_salt = "ef".repeat(16);

        store.set("wxid_abc", &data_key, "4.1.7.31", None, None);
        store.set_enc_key("wxid_abc", &enc_key, &enc_salt, "4.1.7.31", None, None);

        let km = store.resolve_key_material("wxid_abc").unwrap();
        match km {
            KeyMaterial::EncKey { key, salt } => {
                assert_eq!(hex::encode(key), enc_key);
                assert_eq!(hex::encode(salt), enc_salt);
            }
            _ => panic!("expected EncKey variant"),
        }
    }

    #[test]
    fn test_resolve_key_material_falls_back_to_raw_key() {
        let mut store = KeyStore::default();
        let data_key = "ab".repeat(32);
        store.set("wxid_abc", &data_key, "4.1.7.31", None, None);

        let km = store.resolve_key_material("wxid_abc").unwrap();
        match km {
            KeyMaterial::RawKey(key) => {
                assert_eq!(hex::encode(key), data_key);
            }
            _ => panic!("expected RawKey variant"),
        }
    }

    #[test]
    fn test_resolve_key_material_none_when_no_keys() {
        let mut store = KeyStore::default();
        // Create entry with empty data_key and no enc_key
        store.set_image_key("wxid_abc", "deadbeef12345678");
        assert!(store.resolve_key_material("wxid_abc").is_none());
    }

    #[test]
    fn test_resolve_key_material_nonexistent_account() {
        let store = KeyStore::default();
        assert!(store.resolve_key_material("wxid_nope").is_none());
    }

    #[test]
    fn test_enc_key_roundtrip_serialization() {
        let mut store = KeyStore::default();
        let enc_key = "ab".repeat(32);
        let enc_salt = "cd".repeat(16);
        store.set_enc_key(
            "wxid_abc",
            &enc_key,
            &enc_salt,
            "4.1.7.31",
            Some("Test".into()),
            None,
        );

        let serialized = toml::to_string_pretty(&store).unwrap();
        let deserialized: KeyStore = toml::from_str(&serialized).unwrap();

        let entry = deserialized.get("wxid_abc").unwrap();
        assert_eq!(entry.enc_key.as_deref(), Some(&*enc_key));
        assert_eq!(entry.enc_key_salt.as_deref(), Some(&*enc_salt));
        assert_eq!(entry.nickname.as_deref(), Some("Test"));

        // resolve_key_material should work on deserialized store
        let km = deserialized.resolve_key_material("wxid_abc").unwrap();
        assert!(matches!(km, KeyMaterial::EncKey { .. }));
    }

    #[test]
    fn test_set_enc_keys_dedup_and_sort() {
        let mut store = KeyStore::default();
        let pairs = vec![
            EncKeyPair {
                key: [0xBBu8; 32],
                salt: [0x02u8; 16],
            },
            EncKeyPair {
                key: [0xAAu8; 32],
                salt: [0x01u8; 16],
            },
            EncKeyPair {
                key: [0xAAu8; 32],
                salt: [0x01u8; 16],
            }, // duplicate
        ];
        store.set_enc_keys("wxid_test", &pairs, "4.1.8.0", None, None);

        let entry = store.get("wxid_test").unwrap();
        assert_eq!(entry.enc_keys.len(), 2, "duplicates should be removed");
        assert_eq!(
            entry.enc_keys[0].salt,
            hex::encode([0x01u8; 16]),
            "sorted by salt"
        );
        assert_eq!(entry.enc_keys[1].salt, hex::encode([0x02u8; 16]));
        assert!(entry.enc_key.is_none(), "legacy enc_key cleared");
        assert!(entry.enc_key_salt.is_none(), "legacy enc_key_salt cleared");
    }

    #[test]
    fn test_set_enc_keys_preserves_data_key_and_image_key() {
        let mut store = KeyStore::default();
        let data_key = "dd".repeat(32);
        store.set("wxid_test", &data_key, "4.1.7.31", None, None);
        store.set_image_key("wxid_test", "deadbeef12345678");

        let pairs = vec![EncKeyPair {
            key: [0xAAu8; 32],
            salt: [0x01u8; 16],
        }];
        store.set_enc_keys("wxid_test", &pairs, "4.1.8.0", Some("Nick".into()), None);

        let entry = store.get("wxid_test").unwrap();
        assert_eq!(entry.data_key, data_key);
        assert_eq!(entry.image_aes_key.as_deref(), Some("deadbeef12345678"));
        assert_eq!(entry.enc_keys.len(), 1);
    }

    #[test]
    fn test_resolve_key_material_enc_keys_preferred() {
        let mut store = KeyStore::default();
        let data_key = "ab".repeat(32);
        store.set("wxid_test", &data_key, "4.1.7.31", None, None);

        let pairs = vec![
            EncKeyPair {
                key: [0xAAu8; 32],
                salt: [0x01u8; 16],
            },
            EncKeyPair {
                key: [0xBBu8; 32],
                salt: [0x02u8; 16],
            },
        ];
        store.set_enc_keys("wxid_test", &pairs, "4.1.8.0", None, None);

        let km = store.resolve_key_material("wxid_test").unwrap();
        match km {
            KeyMaterial::EncKeys(ref p) => {
                assert_eq!(p.len(), 2);
                assert_eq!(p[0].salt, [0x01u8; 16]);
                assert_eq!(p[1].salt, [0x02u8; 16]);
            }
            _ => panic!("expected EncKeys variant"),
        }
    }

    #[test]
    fn test_resolve_key_material_legacy_enc_key_still_works() {
        let mut store = KeyStore::default();
        let enc_key = "cd".repeat(32);
        let enc_salt = "ef".repeat(16);
        store.set_enc_key("wxid_test", &enc_key, &enc_salt, "4.1.7.31", None, None);

        let km = store.resolve_key_material("wxid_test").unwrap();
        assert!(matches!(km, KeyMaterial::EncKey { .. }));
    }

    #[test]
    fn test_enc_keys_roundtrip_serialization() {
        let mut store = KeyStore::default();
        let pairs = vec![
            EncKeyPair {
                key: [0xAAu8; 32],
                salt: [0x01u8; 16],
            },
            EncKeyPair {
                key: [0xBBu8; 32],
                salt: [0x02u8; 16],
            },
        ];
        store.set_enc_keys("wxid_test", &pairs, "4.1.8.0", Some("Test".into()), None);

        let serialized = toml::to_string_pretty(&store).unwrap();
        let deserialized: KeyStore = toml::from_str(&serialized).unwrap();

        let km = deserialized.resolve_key_material("wxid_test").unwrap();
        match km {
            KeyMaterial::EncKeys(ref p) => assert_eq!(p.len(), 2),
            _ => panic!("expected EncKeys variant after roundtrip"),
        }
    }

    #[test]
    fn set_with_different_data_key_clears_enc_keys() {
        let mut store = KeyStore::default();
        let original_key = "aa".repeat(32);
        store.set("wxid_abc", &original_key, "4.1.7.31", None, None);

        // Populate derived fields via set_enc_keys
        let pairs = vec![EncKeyPair {
            key: [0xCCu8; 32],
            salt: [0x01u8; 16],
        }];
        store.set_enc_keys("wxid_abc", &pairs, "4.1.7.31", None, None);
        // Also set legacy enc_key manually
        {
            let entry = store.accounts.get_mut("wxid_abc").unwrap();
            entry.enc_key = Some("dd".repeat(32));
            entry.enc_key_salt = Some("ee".repeat(16));
        }

        // Now set() with a DIFFERENT data_key
        let new_key = "ff".repeat(32);
        store.set("wxid_abc", &new_key, "4.1.8.0", None, None);

        let entry = store.get("wxid_abc").unwrap();
        assert_eq!(entry.data_key, new_key);
        assert_eq!(entry.enc_key, None, "legacy enc_key should be cleared");
        assert_eq!(
            entry.enc_key_salt, None,
            "legacy enc_key_salt should be cleared"
        );
        assert!(entry.enc_keys.is_empty(), "enc_keys should be cleared");
    }

    #[test]
    fn set_with_same_data_key_preserves_enc_keys() {
        let mut store = KeyStore::default();
        let data_key = "aa".repeat(32);
        store.set("wxid_abc", &data_key, "4.1.7.31", None, None);

        // Populate derived fields
        let pairs = vec![EncKeyPair {
            key: [0xCCu8; 32],
            salt: [0x01u8; 16],
        }];
        store.set_enc_keys("wxid_abc", &pairs, "4.1.7.31", None, None);
        // Also set legacy enc_key manually
        {
            let entry = store.accounts.get_mut("wxid_abc").unwrap();
            entry.enc_key = Some("dd".repeat(32));
            entry.enc_key_salt = Some("ee".repeat(16));
        }

        // Now set() with the SAME data_key
        store.set("wxid_abc", &data_key, "4.1.8.0", Some("Nick".into()), None);

        let entry = store.get("wxid_abc").unwrap();
        assert_eq!(entry.data_key, data_key);
        assert_eq!(
            entry.enc_key.as_deref(),
            Some(&*"dd".repeat(32)),
            "legacy enc_key should be preserved"
        );
        assert_eq!(
            entry.enc_key_salt.as_deref(),
            Some(&*"ee".repeat(16)),
            "legacy enc_key_salt should be preserved"
        );
        assert_eq!(entry.enc_keys.len(), 1, "enc_keys should be preserved");
        assert_eq!(entry.enc_keys[0].enc_key, hex::encode([0xCCu8; 32]));
    }

    #[test]
    fn test_set_enc_keys_overwrites_stale_base_wxid() {
        let mut store = KeyStore::default();
        // Initial entry with stale base_wxid
        store.set(
            "testuser001_1662",
            &"aa".repeat(32),
            "4.1.7.31",
            None,
            Some("testuser001_1662".into()), // stale: same as directory name
        );

        let entry = store.get("testuser001_1662").unwrap();
        assert_eq!(entry.base_wxid.as_deref(), Some("testuser001_1662"));

        // Writeback with canonical base_wxid
        let pairs = vec![EncKeyPair {
            key: [0xAAu8; 32],
            salt: [0x01u8; 16],
        }];
        store.set_enc_keys(
            "testuser001_1662",
            &pairs,
            "4.1.8.0",
            None,
            Some("testuser001".into()), // canonical value
        );

        let entry = store.get("testuser001_1662").unwrap();
        assert_eq!(
            entry.base_wxid.as_deref(),
            Some("testuser001"),
            "stale base_wxid should be overwritten with canonical value"
        );
    }
}
