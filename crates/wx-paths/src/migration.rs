use std::path::Path;

use crate::sudo::chown_to_sudo_user;

const OLD_CONFIG_DIR_NAME: &str = "wechat-utils";
const SENTINEL_FILE: &str = ".migrated";

pub(crate) enum MigrationOutcome {
    AlreadyMigrated,
    NoLegacyConfig,
    Migrated,
}

/// Migrate config files from legacy `~/.config/wechat-utils/` to the new
/// platform-correct config directory.
///
/// Strategy: **move then delete originals**. Files are first copied to the
/// new location, then originals are deleted. If copy fails, originals are
/// left untouched. If delete fails, both copies exist but new location wins.
///
/// Sentinel is only written when all operations succeed. If any step
/// fails, returns an error and the sentinel is NOT written, allowing retry.
pub(crate) fn ensure_config_migrated(
    home: &Path,
    new_config_root: &Path,
) -> Result<MigrationOutcome, std::io::Error> {
    let sentinel = new_config_root.join(SENTINEL_FILE);
    if sentinel.exists() {
        return Ok(MigrationOutcome::AlreadyMigrated);
    }

    let old_config = home.join(".config").join(OLD_CONFIG_DIR_NAME);
    if !old_config.exists() {
        std::fs::create_dir_all(new_config_root)?;
        chown_config_tree(home, new_config_root);
        std::fs::File::create(&sentinel)?;
        chown_to_sudo_user(&sentinel);
        return Ok(MigrationOutcome::NoLegacyConfig);
    }

    std::fs::create_dir_all(new_config_root)?;
    chown_config_tree(home, new_config_root);

    for file in &["keys.toml", "settings.toml"] {
        let src = old_config.join(file);
        let dst = new_config_root.join(file);
        if src.exists() && !dst.exists() {
            std::fs::copy(&src, &dst)?;
            chown_to_sudo_user(&dst);
            // Delete original after successful copy
            std::fs::remove_file(&src)?;
        }
    }

    std::fs::File::create(&sentinel)?;
    chown_to_sudo_user(&sentinel);
    Ok(MigrationOutcome::Migrated)
}

/// Chown all path segments from the new config root up to (but not including)
/// the home directory. This ensures intermediate directories created under
/// sudo (e.g. ~/Library/Application Support/wx-cli/) are owned by the
/// real user.
fn chown_config_tree(home: &Path, config_root: &Path) {
    let mut current = config_root.to_path_buf();
    while current.starts_with(home) && current != home {
        chown_to_sudo_user(&current);
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn migration_no_legacy_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let new_config = home.join("new_config");

        let result = ensure_config_migrated(home, &new_config).unwrap();
        assert!(matches!(result, MigrationOutcome::NoLegacyConfig));
        assert!(new_config.join(SENTINEL_FILE).exists());
    }

    #[test]
    fn migration_already_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let new_config = home.join("new_config");
        fs::create_dir_all(&new_config).unwrap();
        fs::File::create(new_config.join(SENTINEL_FILE)).unwrap();

        let result = ensure_config_migrated(home, &new_config).unwrap();
        assert!(matches!(result, MigrationOutcome::AlreadyMigrated));
    }

    #[test]
    fn migration_moves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // Create legacy config
        let old_config = home.join(".config").join(OLD_CONFIG_DIR_NAME);
        fs::create_dir_all(&old_config).unwrap();
        fs::write(old_config.join("keys.toml"), "test-keys").unwrap();
        fs::write(old_config.join("settings.toml"), "test-settings").unwrap();

        let new_config = home.join("new_config");
        let result = ensure_config_migrated(home, &new_config).unwrap();

        match result {
            MigrationOutcome::Migrated => {}
            _ => panic!("expected Migrated"),
        }

        // New files exist
        assert_eq!(
            fs::read_to_string(new_config.join("keys.toml")).unwrap(),
            "test-keys"
        );
        assert_eq!(
            fs::read_to_string(new_config.join("settings.toml")).unwrap(),
            "test-settings"
        );

        // Old files deleted
        assert!(!old_config.join("keys.toml").exists());
        assert!(!old_config.join("settings.toml").exists());

        // Sentinel exists
        assert!(new_config.join(SENTINEL_FILE).exists());
    }

    #[test]
    fn migration_skips_existing_dst() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // Create legacy config
        let old_config = home.join(".config").join(OLD_CONFIG_DIR_NAME);
        fs::create_dir_all(&old_config).unwrap();
        fs::write(old_config.join("keys.toml"), "old-keys").unwrap();

        // Create new config with existing file
        let new_config = home.join("new_config");
        fs::create_dir_all(&new_config).unwrap();
        fs::write(new_config.join("keys.toml"), "new-keys").unwrap();

        let result = ensure_config_migrated(home, &new_config).unwrap();
        match result {
            MigrationOutcome::Migrated => {}
            _ => panic!("expected Migrated"),
        }

        // New file unchanged
        assert_eq!(
            fs::read_to_string(new_config.join("keys.toml")).unwrap(),
            "new-keys"
        );
        // Old file still exists (wasn't migrated because dst exists)
        assert!(old_config.join("keys.toml").exists());
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let old_config = home.join(".config").join(OLD_CONFIG_DIR_NAME);
        fs::create_dir_all(&old_config).unwrap();
        fs::write(old_config.join("keys.toml"), "test").unwrap();

        let new_config = home.join("new_config");

        // First run
        ensure_config_migrated(home, &new_config).unwrap();
        // Second run
        let result = ensure_config_migrated(home, &new_config).unwrap();
        assert!(matches!(result, MigrationOutcome::AlreadyMigrated));
    }
}
