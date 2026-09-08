use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

pub fn load_versioned<T, F>(
    path: &Path,
    current_version: u32,
    mut migrate: F,
) -> anyhow::Result<Option<T>>
where
    T: DeserializeOwned,
    F: FnMut(u32, serde_json::Value) -> anyhow::Result<serde_json::Value>,
{
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut value: serde_json::Value = serde_json::from_str(&raw)?;
    let initial_version = schema_version(&value)?;
    if initial_version > current_version {
        anyhow::bail!(
            "{} uses schema version {}, but this build supports up to {}",
            path.display(),
            initial_version,
            current_version
        );
    }

    let mut version = initial_version;
    while version < current_version {
        value = migrate(version, value)?;
        version += 1;
        set_schema_version(&mut value, version)?;
    }

    if initial_version != current_version {
        let backup = migration_backup_path(path, initial_version);
        if !backup.exists() {
            std::fs::write(&backup, raw)?;
        }
        save_value(path, &value)?;
    }

    Ok(Some(serde_json::from_value(value)?))
}

pub fn save_versioned<T: Serialize>(
    path: &Path,
    current_version: u32,
    value: &T,
) -> anyhow::Result<()> {
    let mut json = serde_json::to_value(value)?;
    set_schema_version(&mut json, current_version)?;
    save_value(path, &json)
}

fn schema_version(value: &serde_json::Value) -> anyhow::Result<u32> {
    let Some(object) = value.as_object() else {
        anyhow::bail!("versioned JSON root must be an object");
    };
    let Some(version) = object.get("schema_version") else {
        return Ok(0);
    };
    let Some(version) = version.as_u64() else {
        anyhow::bail!("schema_version must be a non-negative integer");
    };
    u32::try_from(version).map_err(Into::into)
}

fn set_schema_version(value: &mut serde_json::Value, version: u32) -> anyhow::Result<()> {
    let Some(object) = value.as_object_mut() else {
        anyhow::bail!("versioned JSON root must be an object");
    };
    object.insert("schema_version".to_string(), version.into());
    Ok(())
}

fn migration_backup_path(path: &Path, version: u32) -> PathBuf {
    let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("json");
    path.with_extension(format!("{extension}.v{version}.bak"))
}

fn save_value(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("json");
    let tmp = path.with_extension(format!("{extension}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Example {
        value: u32,
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kiralm-storage-{name}-{}.json",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn migrates_legacy_json_and_keeps_a_backup() {
        let path = test_path("migrate");
        let raw = r#"{"value":7}"#;
        std::fs::write(&path, raw).unwrap();

        let loaded = load_versioned::<Example, _>(&path, 1, |version, value| {
            assert_eq!(version, 0);
            Ok(value)
        })
        .unwrap()
        .unwrap();

        assert_eq!(loaded, Example { value: 7 });
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["schema_version"], 1);
        assert_eq!(
            std::fs::read_to_string(migration_backup_path(&path, 0)).unwrap(),
            raw
        );

        let _ = std::fs::remove_file(migration_backup_path(&path, 0));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_data_from_a_newer_build() {
        let path = test_path("future");
        std::fs::write(&path, r#"{"schema_version":2,"value":7}"#).unwrap();

        let error = load_versioned::<Example, _>(&path, 1, |_, value| Ok(value))
            .unwrap_err()
            .to_string();
        assert!(error.contains("supports up to 1"));
        assert!(!migration_backup_path(&path, 2).exists());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn saves_the_current_version() {
        let path = test_path("save");
        save_versioned(&path, 3, &Example { value: 9 }).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["schema_version"], 3);
        assert_eq!(saved["value"], 9);

        let _ = std::fs::remove_file(path);
    }
}
