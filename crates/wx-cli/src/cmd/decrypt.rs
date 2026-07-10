use std::path::PathBuf;

use wx_context::{AccountContext, DecryptRequest, PersistentCache, ResolveParams};
use wx_decrypt::KeyMaterial;

use crate::util::{find_db_files, print_cache_stats, print_detection_note};

pub fn cmd_decrypt(
    key_hex: Option<String>,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    output: Option<PathBuf>,
    incremental: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let params = &wx_decrypt::MACOS_4_1_7_31;

    // Use PersistentCache path when no explicit output is given
    if output.is_none() || incremental {
        let acct = AccountContext::resolve(&ResolveParams {
            account: account.as_deref(),
            data_dir: data_dir.as_deref(),
            key_hex: key_hex.as_deref(),
        })?;
        print_detection_note(&acct);

        let cache = PersistentCache::new(&acct, params)?;
        let stats = DecryptRequest::new()
            .all()
            .execute_with_progress(&cache, crate::util::decrypt_progress_callback)?;
        print_cache_stats(&stats);

        eprintln!(
            "Done: {} decrypted, {} cached, {} errors, {} WAL patched",
            stats.decrypted, stats.skipped, stats.errors, stats.wal_patched
        );
        eprintln!("Output: {}", cache.decrypted_root().display());
        return Ok(());
    }

    // Explicit output directory — full decrypt (legacy behavior)
    let output_dir = output.unwrap();

    let acct = AccountContext::resolve(&ResolveParams {
        account: account.as_deref(),
        data_dir: data_dir.as_deref(),
        key_hex: key_hex.as_deref(),
    })?;
    print_detection_note(&acct);

    let db_storage = acct.data_dir.join("db_storage");
    if !db_storage.exists() {
        return Err(format!("db_storage not found in {}", acct.data_dir.display()).into());
    }

    let db_files = find_db_files(&db_storage)?;
    if db_files.is_empty() {
        return Err("no .db files found in db_storage/".into());
    }

    eprintln!("Found {} database files.", db_files.len());

    let mut ok_count = 0u32;
    let mut skip_count = 0u32;
    let mut err_count = 0u32;
    let mut wal_ok_count = 0u32;
    let mut wal_err_count = 0u32;

    for db_path in &db_files {
        let rel = db_path.strip_prefix(&acct.data_dir).unwrap_or(db_path);
        let out_path = output_dir.join(rel);

        let db_result = match &acct.key_material {
            KeyMaterial::RawKey(key) => wx_decrypt::decrypt_db(db_path, &out_path, key, params),
            KeyMaterial::EncKey { key, salt } => {
                wx_decrypt::decrypt_db_direct(db_path, &out_path, key, salt, params)
            }
            KeyMaterial::EncKeys(pairs) => match wx_decrypt::read_main_db_salt_for_path(db_path) {
                Ok(db_salt) => match pairs.iter().find(|p| p.salt == db_salt) {
                    Some(pair) => wx_decrypt::decrypt_db_direct(
                        db_path, &out_path, &pair.key, &pair.salt, params,
                    ),
                    None => Err(wx_decrypt::DecryptError::NoMatchingEncKey),
                },
                Err(e) => Err(e),
            },
        };

        match db_result {
            Ok(()) => {
                eprintln!("  OK  {}", rel.display());
                ok_count += 1;

                let wal_path = db_path.with_extension("db-wal");
                if wal_path.exists() {
                    let wal_result = match &acct.key_material {
                        KeyMaterial::RawKey(key) => {
                            wx_decrypt::decrypt_wal(&wal_path, &out_path, key, params)
                        }
                        KeyMaterial::EncKey { key, salt } => {
                            wx_decrypt::decrypt_wal_direct(&wal_path, &out_path, key, salt, params)
                        }
                        KeyMaterial::EncKeys(pairs) => {
                            match wx_decrypt::read_main_db_salt_for_path(&wal_path) {
                                Ok(db_salt) => match pairs.iter().find(|p| p.salt == db_salt) {
                                    Some(pair) => wx_decrypt::decrypt_wal_direct(
                                        &wal_path, &out_path, &pair.key, &pair.salt, params,
                                    ),
                                    None => Err(wx_decrypt::DecryptError::NoMatchingEncKey),
                                },
                                Err(e) => Err(e),
                            }
                        }
                    };
                    match wal_result {
                        Ok(n) if n > 0 => {
                            eprintln!("  WAL {} — {n} frames patched", rel.display());
                            wal_ok_count += 1;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("  WAL ERR {} — {e}", rel.display());
                            wal_err_count += 1;
                        }
                    }
                }
            }
            Err(wx_decrypt::DecryptError::AlreadyDecrypted) => {
                eprintln!("  SKIP {}", rel.display());
                skip_count += 1;
            }
            Err(e) => {
                eprintln!("  ERR  {} — {e}", rel.display());
                err_count += 1;
            }
        }
    }

    eprint!("\nDone: {ok_count} decrypted, {skip_count} skipped, {err_count} errors");
    if wal_ok_count > 0 || wal_err_count > 0 {
        eprint!(", WAL: {wal_ok_count} patched / {wal_err_count} failed");
    }
    eprintln!(".");

    if err_count > 0 || wal_err_count > 0 {
        let mut parts = Vec::new();
        if err_count > 0 {
            parts.push(format!("{err_count} databases failed"));
        }
        if wal_err_count > 0 {
            parts.push(format!("{wal_err_count} WAL patches failed"));
        }
        return Err(parts.join(", ").into());
    }

    Ok(())
}
