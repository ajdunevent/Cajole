use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::Alarm;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to read alarm store at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write alarm store at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse alarm store at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("no alarm with id {0}")]
    NotFound(Uuid),
}

/// On-disk representation of the alarm set: a version tag plus the list, so
/// the format can be migrated later.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    alarms: Vec<Alarm>,
}

fn default_version() -> u32 {
    1
}

pub struct AlarmStore {
    path: PathBuf,
    alarms: Vec<Alarm>,
}

impl AlarmStore {
    /// Loads the store from `path`, creating an empty one if the file does not
    /// exist.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();

        if !path.exists() {
            return Ok(AlarmStore {
                path,
                alarms: Vec::new(),
            });
        }

        let contents = fs::read_to_string(&path).map_err(|source| StoreError::Read {
            path: path.clone(),
            source,
        })?;

        let file: StoreFile =
            serde_json::from_str(&contents).map_err(|source| StoreError::Parse {
                path: path.clone(),
                source,
            })?;

        Ok(AlarmStore {
            path,
            alarms: file.alarms,
        })
    }

    pub fn alarms(&self) -> &[Alarm] {
        &self.alarms
    }

    pub fn get(&self, id: Uuid) -> Option<&Alarm> {
        self.alarms.iter().find(|a| a.id == id)
    }

    /// Inserts a new alarm or replaces the one with the same id, then
    /// persists.
    pub fn upsert(&mut self, alarm: Alarm) -> Result<(), StoreError> {
        if let Some(existing) = self.alarms.iter_mut().find(|a| a.id == alarm.id) {
            *existing = alarm;
        } else {
            self.alarms.push(alarm);
        }
        self.save()
    }

    pub fn remove(&mut self, id: Uuid) -> Result<(), StoreError> {
        let before = self.alarms.len();
        self.alarms.retain(|a| a.id != id);
        if self.alarms.len() == before {
            return Err(StoreError::NotFound(id));
        }
        self.save()
    }

    /// Writes the state to a sibling temp file, then renames it over `path`.
    /// The rename is atomic on one filesystem, so a crash mid-write cannot
    /// leave a corrupt store.
    fn save(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source,
            })?;
        }

        let file = StoreFile {
            version: 1,
            alarms: self.alarms.clone(),
        };
        let serialized =
            serde_json::to_string_pretty(&file).expect("Alarm serialization cannot fail");

        let tmp_path = tmp_path_for(&self.path);
        fs::write(&tmp_path, serialized).map_err(|source| StoreError::Write {
            path: self.path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &self.path).map_err(|source| StoreError::Write {
            path: self.path.clone(),
            source,
        })?;

        Ok(())
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let file_name = tmp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".to_string());
    tmp.set_file_name(format!("{file_name}.tmp"));
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Alarm;
    use chrono::NaiveTime;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempdir();
        let path = dir.join("alarms.json");

        let alarm = Alarm::new("Wake up", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        let id = alarm.id;

        {
            let mut store = AlarmStore::load(&path).unwrap();
            store.upsert(alarm).unwrap();
        }

        let reloaded = AlarmStore::load(&path).unwrap();
        assert_eq!(reloaded.alarms().len(), 1);
        assert_eq!(reloaded.get(id).unwrap().label, "Wake up");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_file_loads_as_empty_store() {
        let dir = tempdir();
        let path = dir.join("nonexistent.json");
        let store = AlarmStore::load(&path).unwrap();
        assert!(store.alarms().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn removing_unknown_id_errors() {
        let dir = tempdir();
        let path = dir.join("alarms.json");
        let mut store = AlarmStore::load(&path).unwrap();
        let result = store.remove(Uuid::new_v4());
        assert!(matches!(result, Err(StoreError::NotFound(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    // Temp-dir helper that avoids a dev-dependency. Each test gets a unique
    // directory under the OS temp dir.
    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cajole-store-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
