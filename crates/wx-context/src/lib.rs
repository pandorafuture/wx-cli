//! WeChat database context layer — account resolution, decrypt orchestration,
//! and encrypted direct-open support.
//!
//! When `raw_key` is available, use [`open_encrypted_db`] or
//! [`open_encrypted_db_with_pool`] to open encrypted databases directly
//! via SQLCipher's `sqlite3_key()` C API, bypassing the decrypt-cache pipeline.

mod account;
mod cache;
mod contact;
mod db_category;
mod decrypt_request;
mod decrypt_scope;
mod error;
mod file_lock;
mod fts_tokenizer;
mod kdf_cache;
mod patch_state;
mod progress;
pub mod shard_routing;
pub mod tokenizer;
mod visibility;
mod wal_patch;

pub use account::{AccountContext, ResolveParams};
pub use cache::PersistentCache;
pub use contact::{ContactResolver, Direction};
pub use db_category::{discover_db_files, DbCategory, DbFile};
pub use decrypt_request::DecryptRequest;
pub use decrypt_scope::DecryptScope;
pub use error::ContextError;
pub use fts_tokenizer::{
    open_fts_connection, open_fts_connection_with_key, register_mm_fts_tokenizer,
};
pub use progress::{DecryptProgress, DecryptStats};
pub use shard_routing::{route_shards_for_query, write_shard_metadata_sidecar};
pub use visibility::VisibilityIndex;

/// Read persisted per-database derived keys for this account.
/// Ephemeral `--key` contexts deliberately ignore the store because the supplied
/// raw key may not match its cached entries.
pub fn persisted_derived_keys(
    account: &AccountContext,
) -> Result<Vec<wx_decrypt::EncKeyPair>, ContextError> {
    if !account.writeback_enabled {
        return Ok(Vec::new());
    }
    let store = wx_keychain::KeyStore::load_default()?;
    Ok(match store.resolve_key_material(&account.account_id) {
        Some(wx_decrypt::KeyMaterial::EncKeys(pairs)) => pairs,
        Some(wx_decrypt::KeyMaterial::EncKey { key, salt }) => {
            vec![wx_decrypt::EncKeyPair { key, salt }]
        }
        _ => Vec::new(),
    })
}

/// Open encrypted WeChat DB directory directly (no pool, no FTS).
/// For one-shot commands: contacts, sessions, query, search, export.
pub fn open_encrypted_db(account: &AccountContext) -> Result<wx_db::WechatDb, ContextError> {
    let raw_key = account
        .raw_key
        .ok_or_else(|| ContextError::Cache("raw_key required for encrypted direct open".into()))?;
    let encrypted_root = account.data_dir.join("db_storage");
    let derived_keys = persisted_derived_keys(account)?;
    let db =
        wx_db::WechatDb::open_encrypted_with_key_cache(&encrypted_root, raw_key, &derived_keys)?;
    Ok(db)
}

/// Open only encrypted contact.db and session.db directly.
/// This avoids deriving keys for and scanning message shards for core-only commands.
pub fn open_encrypted_db_core(account: &AccountContext) -> Result<wx_db::WechatDb, ContextError> {
    let raw_key = account
        .raw_key
        .ok_or_else(|| ContextError::Cache("raw_key required for encrypted direct open".into()))?;
    let encrypted_root = account.data_dir.join("db_storage");
    let derived_keys = persisted_derived_keys(account)?;
    let db = wx_db::WechatDb::open_encrypted_core_with_key_cache(
        &encrypted_root,
        raw_key,
        &derived_keys,
    )?;
    Ok(db)
}

/// Open encrypted WeChat DB directory with connection pool and FTS tokenizer.
/// For long-running serve mode only.
pub fn open_encrypted_db_with_pool(
    account: &AccountContext,
) -> Result<wx_db::WechatDb, ContextError> {
    let raw_key = account
        .raw_key
        .ok_or_else(|| ContextError::Cache("raw_key required for encrypted direct open".into()))?;
    let encrypted_root = account.data_dir.join("db_storage");
    let derived_keys = persisted_derived_keys(account)?;
    let db = wx_db::WechatDb::open_encrypted_with_pool_and_key_cache(
        &encrypted_root,
        raw_key,
        &derived_keys,
        register_mm_fts_tokenizer,
    )?;
    Ok(db)
}
