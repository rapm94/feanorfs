use anyhow::{bail, Context, Result};
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MigrationJournal {
    pub(crate) stores: BTreeMap<String, StoreJournal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct StoreJournal {
    pub(crate) key: String,
    pub(crate) source_path: String,
    pub(crate) db_fingerprint: String,
    pub(crate) wal_fingerprint: String,
    pub(crate) shm_fingerprint: String,
    pub(crate) archive_path: String,
    pub(crate) phase: StorePhase,
    pub(crate) db_archived: bool,
    pub(crate) wal_archived: bool,
    pub(crate) shm_archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorePhase {
    #[default]
    Discovered,
    Imported,
    Verified,
    Archived,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum Fault {
    None,
    BeforeTargetWrite,
    AfterTargetWrite,
    BeforeArchive,
    MidSidecarArchive,
}

impl Fault {
    pub(crate) fn inject(&self, point: &str) -> Result<()> {
        match (self, point) {
            (Fault::BeforeTargetWrite, "before_target_write") => {
                bail!("injected: before_target_write")
            }
            (Fault::AfterTargetWrite, "after_target_write") => {
                bail!("injected: after_target_write")
            }
            (Fault::BeforeArchive, "before_archive") => bail!("injected: before_archive"),
            (Fault::MidSidecarArchive, "mid_sidecar") => bail!("injected: mid_sidecar"),
            _ => Ok(()),
        }
    }
}

pub(crate) fn load_journal(root: &Path) -> Result<MigrationJournal> {
    let p = root.join(".feanorfs/metadata-migration.json");
    if !p.exists() {
        Ok(MigrationJournal {
            stores: BTreeMap::new(),
        })
    } else {
        serde_json::from_str(&std::fs::read_to_string(&p)?).context("parse journal")
    }
}

pub(crate) fn save_journal(root: &Path, j: &MigrationJournal) -> Result<()> {
    let json = serde_json::to_string_pretty(j)?;
    let mut awf = AtomicWriteFile::open(root.join(".feanorfs/metadata-migration.json"))?;
    awf.write_all(json.as_bytes())?;
    awf.commit()?;
    Ok(())
}

pub(crate) fn set_phase(root: &Path, key: &str, phase: StorePhase) -> Result<()> {
    let mut j = load_journal(root)?;
    if let Some(s) = j.stores.get_mut(key) {
        s.phase = phase;
    }
    save_journal(root, &j)
}

pub(crate) fn set_archive_path(root: &Path, key: &str, path: &str) -> Result<()> {
    let mut j = load_journal(root)?;
    if let Some(s) = j.stores.get_mut(key) {
        s.archive_path = path.to_string();
    }
    save_journal(root, &j)
}

pub(crate) fn get_flag(root: &Path, key: &str, flag: &str) -> Result<bool> {
    let j = load_journal(root)?;
    Ok(match flag {
        "db" => j.stores.get(key).map(|s| s.db_archived).unwrap_or(false),
        "wal" => j.stores.get(key).map(|s| s.wal_archived).unwrap_or(false),
        "shm" => j.stores.get(key).map(|s| s.shm_archived).unwrap_or(false),
        _ => false,
    })
}

pub(crate) fn set_flag(root: &Path, key: &str, flag: &str, v: bool) -> Result<()> {
    let mut j = load_journal(root)?;
    if let Some(s) = j.stores.get_mut(key) {
        match flag {
            "db" => s.db_archived = v,
            "wal" => s.wal_archived = v,
            "shm" => s.shm_archived = v,
            _ => {}
        }
    }
    save_journal(root, &j)
}

pub(crate) fn reset_store(key: &str, journal: &mut MigrationJournal) {
    let e = journal.stores.entry(key.to_string()).or_default();
    e.phase = StorePhase::Discovered;
    e.db_archived = false;
    e.wal_archived = false;
    e.shm_archived = false;
}

pub(crate) fn archive_cache_db(db_path: &Path, key: &str, root: &Path, fault: Fault) -> Result<()> {
    let stem = db_path.file_stem().context("stem")?.to_string_lossy();
    let archive_base = db_path.with_file_name(format!("{stem}.migrated-v1.db"));
    if db_path.exists() && !get_flag(root, key, "db")? {
        archive_file(db_path, &archive_base)?;
        set_flag(root, key, "db", true)?;
    }
    for (sfx, flag) in &[("-wal", "wal"), ("-shm", "shm")] {
        let src = PathBuf::from(format!("{}{sfx}", db_path.to_string_lossy()));
        let dst = PathBuf::from(format!("{}{sfx}", archive_base.to_string_lossy()));
        if src.exists() && !get_flag(root, key, flag)? {
            fault.inject("mid_sidecar")?;
            archive_file(&src, &dst)?;
            set_flag(root, key, flag, true)?;
        }
    }
    set_phase(root, key, StorePhase::Archived)?;
    set_archive_path(root, key, &archive_base.to_string_lossy())?;
    Ok(())
}

fn archive_file(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        let h1 = blake3::hash(&std::fs::read(dst)?).to_hex().to_string();
        let h2 = blake3::hash(&std::fs::read(src)?).to_hex().to_string();
        if h1 != h2 {
            bail!("archive {} exists with different content", dst.display());
        }
        std::fs::remove_file(src)?;
    } else {
        std::fs::rename(src, dst)?;
    }
    Ok(())
}

pub(crate) fn fingerprint_component(path: &Path) -> Result<String> {
    Ok(blake3::hash(&std::fs::read(path)?).to_hex().to_string())
}

pub(crate) fn fingerprint_component_if_exists(path: &Path) -> Result<String> {
    if path.exists() {
        fingerprint_component(path)
    } else {
        Ok(String::new())
    }
}

pub(crate) async fn table_exists(pool: &sqlx::SqlitePool, name: &str) -> Result<bool> {
    Ok(
        !sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name=?")
            .bind(name)
            .fetch_all(pool)
            .await?
            .is_empty(),
    )
}

pub(crate) async fn col_exists(pool: &sqlx::SqlitePool, table: &str, col: &str) -> Result<bool> {
    Ok(sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await?
        .iter()
        .any(|r| {
            let n: String = r.get("name");
            n == col
        }))
}
