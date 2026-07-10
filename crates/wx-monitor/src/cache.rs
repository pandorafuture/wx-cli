use std::path::{Path, PathBuf};
use std::time::SystemTime;

use wx_decrypt::{CryptoParams, KeyMaterial};

use crate::error::MonitorError;

/// Outcome of a cache update cycle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UpdateKind {
    /// The main .db file changed; a full re-decrypt was performed.
    FullDecrypt,
    /// Only the WAL file changed; patched in-place.
    WalPatched,
    /// Neither file changed.
    NoChange,
}

/// RAII cache that manages decrypted copies in a temp directory.
///
/// Mirrors the `db_storage/` layout so that `WechatDb::open()` can operate
/// on the decrypted root.
pub(crate) struct DecryptCache {
    dir: tempfile::TempDir,
    encrypted_session_dir: PathBuf,
    key_material: KeyMaterial,
    params: &'static CryptoParams,
    last_db_mtime: Option<SystemTime>,
    last_wal_mtime: Option<SystemTime>,
}

/// SQLite database file header (first 16 bytes).
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

impl DecryptCache {
    pub fn new(
        encrypted_session_dir: PathBuf,
        key_material: KeyMaterial,
        params: &'static CryptoParams,
    ) -> Result<Self, MonitorError> {
        let dir = tempfile::TempDir::new()?;
        let root = dir.path();

        // Create directory layout matching db_storage/
        std::fs::create_dir_all(root.join("session"))?;
        std::fs::create_dir_all(root.join("contact"))?;
        std::fs::create_dir_all(root.join("message"))?;

        // Write placeholder SQLite files so WechatDb::open() succeeds
        let minimal = make_minimal_sqlite();
        std::fs::write(root.join("contact").join("contact.db"), &minimal)?;
        std::fs::write(root.join("message").join("message_0.db"), &minimal)?;

        Ok(Self {
            dir,
            encrypted_session_dir,
            key_material,
            params,
            last_db_mtime: None,
            last_wal_mtime: None,
        })
    }

    /// Root path for `WechatDb::open()`.
    pub fn decrypted_root(&self) -> &Path {
        self.dir.path()
    }

    /// Perform the initial full decrypt of session.db (and WAL if present).
    pub fn initial_decrypt(&mut self) -> Result<(), MonitorError> {
        let enc_db = self.encrypted_session_dir.join("session.db");
        let dec_db = self.dir.path().join("session").join("session.db");

        self.do_decrypt_db(&enc_db, &dec_db)?;
        self.last_db_mtime = file_mtime(&enc_db);

        // Patch WAL if it exists
        let wal = self.encrypted_session_dir.join("session.db-wal");
        if wal.exists() {
            let _ = self.do_decrypt_wal(&wal, &dec_db);
            self.last_wal_mtime = file_mtime(&wal);
        }

        Ok(())
    }

    /// Check for changes and update the decrypted cache accordingly.
    pub fn update(&mut self) -> Result<UpdateKind, MonitorError> {
        let enc_db = self.encrypted_session_dir.join("session.db");
        let dec_db = self.dir.path().join("session").join("session.db");
        let wal = self.encrypted_session_dir.join("session.db-wal");

        let db_mtime = file_mtime(&enc_db);
        let wal_mtime = if wal.exists() { file_mtime(&wal) } else { None };

        if db_mtime != self.last_db_mtime {
            // Full re-decrypt (DB changed, WAL may also have changed)
            self.do_decrypt_db(&enc_db, &dec_db)?;
            if wal.exists() {
                let _ = self.do_decrypt_wal(&wal, &dec_db);
            }
            self.last_db_mtime = db_mtime;
            self.last_wal_mtime = wal_mtime;
            Ok(UpdateKind::FullDecrypt)
        } else if wal_mtime != self.last_wal_mtime {
            // WAL-only patch
            if wal.exists() {
                self.do_decrypt_wal(&wal, &dec_db)?;
            }
            self.last_wal_mtime = wal_mtime;
            Ok(UpdateKind::WalPatched)
        } else {
            Ok(UpdateKind::NoChange)
        }
    }

    fn do_decrypt_db(&self, src: &Path, dst: &Path) -> Result<(), wx_decrypt::DecryptError> {
        wx_decrypt::dispatch_decrypt_db(src, dst, &self.key_material, self.params)
    }

    fn do_decrypt_wal(&self, wal: &Path, dst: &Path) -> Result<usize, wx_decrypt::DecryptError> {
        wx_decrypt::dispatch_decrypt_wal(wal, dst, &self.key_material, self.params)
    }
}

/// Build a minimal valid SQLite database (single 4096-byte page with header).
fn make_minimal_sqlite() -> Vec<u8> {
    let mut page = vec![0u8; 4096];
    // SQLite header
    page[..16].copy_from_slice(SQLITE_HEADER);
    // Page size = 4096 (big-endian at offset 16)
    page[16] = 0x10;
    page[17] = 0x00;
    // File format write version = 1
    page[18] = 1;
    // File format read version = 1
    page[19] = 1;
    // Reserved space per page = 0
    page[20] = 0;
    // Max embedded payload fraction = 64
    page[21] = 64;
    // Min embedded payload fraction = 32
    page[22] = 32;
    // Leaf payload fraction = 32
    page[23] = 32;
    // Page count = 1 (big-endian at offset 28)
    page[31] = 1;
    // Schema format number = 4 (offset 44)
    page[47] = 4;
    // Text encoding = UTF-8 = 1 (offset 56)
    page[59] = 1;
    page
}

/// Get file mtime as `SystemTime`, or `None` if unavailable.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_decrypt::{KeyMaterial, MACOS_4_1_7_31};

    #[test]
    fn new_creates_correct_layout() {
        let enc_dir = tempfile::TempDir::new().unwrap();
        let cache = DecryptCache::new(
            enc_dir.path().to_path_buf(),
            KeyMaterial::RawKey([0u8; 32]),
            &MACOS_4_1_7_31,
        )
        .unwrap();

        let root = cache.decrypted_root();
        assert!(root.join("session").is_dir());
        assert!(root.join("contact").is_dir());
        assert!(root.join("message").is_dir());
        assert!(root.join("contact").join("contact.db").is_file());
        assert!(root.join("message").join("message_0.db").is_file());

        // Placeholder files should be valid SQLite
        let data = std::fs::read(root.join("contact").join("contact.db")).unwrap();
        assert_eq!(&data[..16], SQLITE_HEADER);
        assert_eq!(data.len(), 4096);
    }

    #[test]
    fn update_returns_no_change_when_nothing_changed() {
        let enc_dir = tempfile::TempDir::new().unwrap();
        let session_dir = enc_dir.path().join("session_dir");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Create a minimal encrypted session.db
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.join("session.db"), &raw_key);

        let mut cache = DecryptCache::new(
            session_dir.clone(),
            KeyMaterial::RawKey(raw_key),
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();

        // Verify the decrypted file exists and is valid SQLite
        let dec_db = cache.decrypted_root().join("session").join("session.db");
        assert!(dec_db.is_file());
        let data = std::fs::read(&dec_db).unwrap();
        assert_eq!(&data[..16], SQLITE_HEADER);

        // No changes → NoChange
        assert_eq!(cache.update().unwrap(), UpdateKind::NoChange);
    }

    #[test]
    fn update_detects_full_decrypt_on_db_change() {
        let session_dir = tempfile::TempDir::new().unwrap();
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        let mut cache = DecryptCache::new(
            session_dir.path().to_path_buf(),
            KeyMaterial::RawKey(raw_key),
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();

        // Wait to ensure mtime difference
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Re-write the encrypted DB (simulates DB checkpoint)
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        assert_eq!(cache.update().unwrap(), UpdateKind::FullDecrypt);
    }

    #[test]
    fn update_detects_wal_patched_on_wal_change() {
        let session_dir = tempfile::TempDir::new().unwrap();
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        let mut cache = DecryptCache::new(
            session_dir.path().to_path_buf(),
            KeyMaterial::RawKey(raw_key),
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();
        assert_eq!(cache.update().unwrap(), UpdateKind::NoChange);

        // Wait to ensure mtime difference
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Create a minimal valid WAL file (header only, no frames).
        // decrypt_wal will return Ok(0) — no frames patched — but mtime changed.
        let wal_path = session_dir.path().join("session.db-wal");
        let mut wal_header = [0u8; 32];
        wal_header[0..4].copy_from_slice(&0x377f_0682u32.to_be_bytes()); // WAL_MAGIC_BE
        std::fs::write(&wal_path, wal_header).unwrap();

        assert_eq!(cache.update().unwrap(), UpdateKind::WalPatched);

        // Subsequent call with no changes → NoChange
        assert_eq!(cache.update().unwrap(), UpdateKind::NoChange);
    }

    // ---- test helper: build encrypted session.db ----

    /// Build a minimal encrypted session.db that `decrypt_db` can process.
    fn build_encrypted_session_db(path: &Path, raw_key: &[u8; 32]) -> PathBuf {
        use aes::cipher::{BlockModeEncrypt, KeyIvInit};
        use hmac::{Hmac, Mac};
        use sha2::Sha512;

        let params = &MACOS_4_1_7_31;
        let salt: [u8; 16] = [0x01; 16];
        let iv: [u8; 16] = [0x42; 16];

        // Derive keys
        let enc_key = derive_enc_key(raw_key, &salt, params);
        let mac_key = derive_mac_key(&enc_key, &salt);

        // Build plaintext page 0: a minimal SQLite page (without the 16-byte header,
        // since decrypt_db will prepend it)
        let data_size = params.page_size - params.reserve - params.salt_size; // 4000
        let mut plaintext = vec![0u8; data_size];
        // Page size at offset 0 (which maps to file offset 16): 0x10 0x00 = 4096
        plaintext[0] = 0x10;
        plaintext[1] = 0x00;
        // Write/read format versions
        plaintext[2] = 1;
        plaintext[3] = 1;
        // Max embedded payload fraction = 64
        plaintext[5] = 64;
        // Min embedded payload fraction = 32
        plaintext[6] = 32;
        // Leaf payload fraction = 32
        plaintext[7] = 32;
        // Page count = 1 at offset 12 (file offset 28)
        plaintext[15] = 1;
        // Schema format = 4 at offset 28 (file offset 44)
        plaintext[31] = 4;
        // Text encoding = UTF-8 at offset 40 (file offset 56)
        plaintext[43] = 1;

        // Pad to AES block size (already aligned: 4000 is divisible by 16)

        // Encrypt
        type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
        let mut ciphertext = plaintext.clone();
        let encryptor = Aes256CbcEnc::new((&enc_key).into(), (&iv).into());
        encryptor
            .encrypt_padded::<aes::cipher::block_padding::NoPadding>(&mut ciphertext, data_size)
            .unwrap();

        // Assemble page: salt + ciphertext + IV + HMAC
        let mut page = Vec::with_capacity(params.page_size);
        page.extend_from_slice(&salt);
        page.extend_from_slice(&ciphertext);
        // Reserve area: IV(16) + HMAC(64)
        page.extend_from_slice(&iv);
        page.resize(params.page_size, 0); // zero-fill HMAC area

        // Compute HMAC over: page[salt_size..page_size - reserve + iv_size] + page_num(1, LE)
        let hmac_data_end = params.page_size - params.reserve + params.iv_size;
        let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(&mac_key).unwrap();
        mac.update(&page[params.salt_size..hmac_data_end]);
        mac.update(&1u32.to_le_bytes()); // page number is 1-indexed
        let hmac_result = mac.finalize().into_bytes();

        // Place HMAC
        let hmac_start = params.page_size - params.reserve + params.iv_size;
        page[hmac_start..hmac_start + params.hmac_size]
            .copy_from_slice(&hmac_result[..params.hmac_size]);

        std::fs::write(path, &page).unwrap();
        path.to_path_buf()
    }

    fn derive_enc_key(raw_key: &[u8; 32], salt: &[u8; 16], params: &CryptoParams) -> [u8; 32] {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha512>(raw_key, salt, params.kdf_iter, &mut key);
        key
    }

    fn derive_mac_key(enc_key: &[u8; 32], salt: &[u8; 16]) -> [u8; 32] {
        let mut mac_salt = [0u8; 16];
        for (i, b) in salt.iter().enumerate() {
            mac_salt[i] = b ^ 0x3a;
        }
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha512>(enc_key, &mac_salt, 2, &mut key);
        key
    }

    /// Derive enc_key + salt from a raw key, for building EncKey test fixtures.
    fn derive_enc_key_and_salt(raw_key: &[u8; 32]) -> ([u8; 32], [u8; 16]) {
        let salt: [u8; 16] = [0x01; 16];
        let enc_key = derive_enc_key(raw_key, &salt, &MACOS_4_1_7_31);
        (enc_key, salt)
    }

    #[test]
    fn enc_key_initial_decrypt_succeeds() {
        let session_dir = tempfile::TempDir::new().unwrap();
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        let (enc_key, salt) = derive_enc_key_and_salt(&raw_key);
        let key_material = KeyMaterial::EncKey { key: enc_key, salt };

        let mut cache = DecryptCache::new(
            session_dir.path().to_path_buf(),
            key_material,
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();

        // Verify decrypted file is valid SQLite
        let dec_db = cache.decrypted_root().join("session").join("session.db");
        assert!(dec_db.is_file());
        let data = std::fs::read(&dec_db).unwrap();
        assert_eq!(&data[..16], SQLITE_HEADER);
    }

    #[test]
    fn enc_key_update_detects_changes() {
        let session_dir = tempfile::TempDir::new().unwrap();
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        let (enc_key, salt) = derive_enc_key_and_salt(&raw_key);
        let key_material = KeyMaterial::EncKey { key: enc_key, salt };

        let mut cache = DecryptCache::new(
            session_dir.path().to_path_buf(),
            key_material,
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();
        assert_eq!(cache.update().unwrap(), UpdateKind::NoChange);

        // Wait for mtime difference then re-write
        std::thread::sleep(std::time::Duration::from_millis(1100));
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);
        assert_eq!(cache.update().unwrap(), UpdateKind::FullDecrypt);
    }

    #[test]
    fn enc_keys_initial_decrypt_succeeds() {
        use wx_decrypt::EncKeyPair;

        let session_dir = tempfile::TempDir::new().unwrap();
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        let (enc_key, salt) = derive_enc_key_and_salt(&raw_key);
        // Wrap in EncKeys (single pair) — the canonical format from capture_key_mach
        let key_material = KeyMaterial::EncKeys(vec![EncKeyPair { key: enc_key, salt }]);

        let mut cache = DecryptCache::new(
            session_dir.path().to_path_buf(),
            key_material,
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();

        let dec_db = cache.decrypted_root().join("session").join("session.db");
        assert!(dec_db.is_file());
        let data = std::fs::read(&dec_db).unwrap();
        assert_eq!(
            &data[..16],
            SQLITE_HEADER,
            "EncKeys path should produce valid SQLite"
        );
    }

    #[test]
    fn enc_keys_update_detects_changes() {
        use wx_decrypt::EncKeyPair;

        let session_dir = tempfile::TempDir::new().unwrap();
        let raw_key = [0xABu8; 32];
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);

        let (enc_key, salt) = derive_enc_key_and_salt(&raw_key);
        let key_material = KeyMaterial::EncKeys(vec![EncKeyPair { key: enc_key, salt }]);

        let mut cache = DecryptCache::new(
            session_dir.path().to_path_buf(),
            key_material,
            &MACOS_4_1_7_31,
        )
        .unwrap();
        cache.initial_decrypt().unwrap();
        assert_eq!(cache.update().unwrap(), UpdateKind::NoChange);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        build_encrypted_session_db(&session_dir.path().join("session.db"), &raw_key);
        assert_eq!(cache.update().unwrap(), UpdateKind::FullDecrypt);
    }
}
