use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn redact_signatures(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("raw_transaction");
            map.remove("signature");
            for value in map.values_mut() {
                redact_signatures(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_signatures),
        _ => {}
    }
}

pub struct Store {
    root: PathBuf,
    _lock: Option<File>,
    mutation: std::sync::Mutex<()>,
    event_lock: std::sync::Mutex<()>,
    write_lock: std::sync::Mutex<()>,
    archive_bytes: std::sync::Mutex<Option<u64>>,
    pub policy: crate::config::StorageConfig,
}
impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_policy(path, crate::config::StorageConfig::default())
    }
    pub fn open_with_policy(
        path: impl AsRef<Path>,
        policy: crate::config::StorageConfig,
    ) -> Result<Self> {
        ensure!(
            policy.event_segment_bytes > 0
                && policy.event_retained_segments > 0
                && policy.terminal_orders_keep > 0
                && policy.order_archive_max_bytes > 0,
            "invalid store policy"
        );
        std::fs::create_dir_all(&path)?;
        let root = path.as_ref().to_path_buf();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("process.lock"))?;
        lock.try_lock_exclusive()
            .context("state directory already in use")?;
        Ok(Self {
            root,
            _lock: Some(lock),
            mutation: std::sync::Mutex::new(()),
            event_lock: std::sync::Mutex::new(()),
            write_lock: std::sync::Mutex::new(()),
            archive_bytes: std::sync::Mutex::new(None),
            policy,
        })
    }
    pub fn readonly(path: impl AsRef<Path>) -> Self {
        Self {
            root: path.as_ref().to_path_buf(),
            _lock: None,
            mutation: std::sync::Mutex::new(()),
            event_lock: std::sync::Mutex::new(()),
            write_lock: std::sync::Mutex::new(()),
            archive_bytes: std::sync::Mutex::new(None),
            policy: crate::config::StorageConfig::default(),
        }
    }
    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let p = self.root.join(name);
        if !p.exists() {
            return Ok(None);
        }
        let value: Value = serde_json::from_slice(&std::fs::read(p)?)?;
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(value)?))
    }
    pub fn write<T: Serialize>(&self, name: &str, v: &T) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("write lock poisoned"))?;
        ensure!(self._lock.is_some(), "read-only store cannot mutate state");
        let tmp = self.root.join(format!("{name}.tmp"));
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(v)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, self.root.join(name))?;
        File::open(tmp.parent().context("state parent")?)?.sync_all()?;
        Ok(())
    }
    pub fn update<T: Serialize + DeserializeOwned + Default>(
        &self,
        name: &str,
        change: impl FnOnce(&mut T) -> Result<()>,
    ) -> Result<()> {
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let mut value = self.read(name)?.unwrap_or_default();
        change(&mut value)?;
        self.write(name, &value)
    }
    pub fn event(&self, kind: &str, v: impl Serialize) -> Result<()> {
        let _guard = self
            .event_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("event lock poisoned"))?;
        ensure!(self._lock.is_some(), "read-only store cannot append events");
        let mut record =
            serde_json::to_vec(&json!({"time_ms":crate::now_ms(),"kind":kind,"data":v}))?;
        record.push(b'\n');
        ensure!(
            record.len() as u64 <= self.policy.event_segment_bytes,
            "event exceeds segment capacity"
        );
        self.rotate_events(record.len() as u64)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events.jsonl"))?;
        f.write_all(&record)?;
        f.sync_all()?;
        Ok(())
    }
    fn rotate_events(&self, extra: u64) -> Result<()> {
        let active = self.root.join("events.jsonl");
        let size = if active.exists() {
            active.metadata()?.len()
        } else {
            0
        };
        let dir = self.root.join("event_archive");
        if size > 0 && size.saturating_add(extra) > self.policy.event_segment_bytes {
            std::fs::create_dir_all(&dir)?;
            let name = format!(
                "events-{:020}-{}.jsonl",
                crate::now_ms(),
                uuid::Uuid::new_v4().simple()
            );
            std::fs::rename(&active, dir.join(name))?;
            File::open(&dir)?.sync_all()?;
            File::open(&self.root)?.sync_all()?;
        }
        if dir.exists() {
            let mut files = vec![];
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_file()
                    && name.starts_with("events-")
                    && name.ends_with(".jsonl")
                {
                    files.push(entry.path());
                }
            }
            files.sort();
            let remove = files
                .len()
                .saturating_sub(self.policy.event_retained_segments);
            for file in files.into_iter().take(remove) {
                std::fs::remove_file(file)?;
            }
            if remove > 0 {
                File::open(&dir)?.sync_all()?;
            }
        }
        Ok(())
    }
    fn archive_name(id: &str) -> Result<String> {
        ensure!(
            id.len() == 34 && id.starts_with("0x") && hex::decode(&id[2..]).is_ok(),
            "invalid archived client ID"
        );
        Ok(format!("order_archive/{}.json", id.to_ascii_lowercase()))
    }
    pub fn archived_order(&self, id: &str) -> Result<Option<Value>> {
        self.read(&Self::archive_name(id)?)
    }
    /// Write immutable cold history before removing a terminal order from the hot ledger.
    /// Archives are budgeted and never automatically deleted; they also prevent Cloid reuse.
    pub fn archive_order(&self, id: &str, value: &Value) -> Result<()> {
        ensure!(self.is_writable(), "read-only store cannot archive orders");
        if let Some(old) = self.archived_order(id)? {
            ensure!(
                old == *value,
                "archived order differs; stop before pruning active ledger"
            );
            return Ok(());
        }
        let dir = self.root.join("order_archive");
        std::fs::create_dir_all(&dir)?;
        let mut cache = self
            .archive_bytes
            .lock()
            .map_err(|_| anyhow::anyhow!("archive lock poisoned"))?;
        // Another archiver may have committed while this caller waited for the lock.
        if let Some(old) = self.archived_order(id)? {
            ensure!(
                old == *value,
                "archived order differs; immutable history cannot be overwritten"
            );
            return Ok(());
        }
        let bytes = match *cache {
            Some(bytes) => bytes,
            None => {
                let mut bytes = 0u64;
                for entry in std::fs::read_dir(&dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        bytes = bytes.saturating_add(entry.metadata()?.len());
                    }
                }
                bytes
            }
        };
        let added = serde_json::to_vec_pretty(value)?.len() as u64;
        ensure!(
            bytes.saturating_add(added) <= self.policy.order_archive_max_bytes,
            "order archive capacity reached; preserve history and provision/migrate storage before resuming"
        );
        // Rescan on the next attempt if rename/fsync returns an uncertain filesystem result.
        *cache = None;
        self.write(&Self::archive_name(id)?, value)?;
        File::open(&self.root)?.sync_all()?;
        *cache = Some(bytes + added);
        if bytes + added >= self.policy.order_archive_max_bytes * 4 / 5 {
            tracing::warn!(
                bytes = bytes + added,
                limit = self.policy.order_archive_max_bytes,
                "order archive exceeds 80% capacity"
            );
        }
        Ok(())
    }
    pub fn pending(&self) -> Result<Option<Value>> {
        self.read("pending.json")
    }
    pub fn begin(&self, v: Value) -> Result<()> {
        ensure!(
            fs2::available_space(&self.root)? >= self.policy.min_free_bytes,
            "state disk reserve reached; no new operation can be prepared"
        );
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        ensure!(
            self.pending()?.is_none(),
            "unresolved operation: run reconcile before any new mutation"
        );
        self.write("pending.json", &Some(v.clone()))?;
        self.event("operation_begin", json!({"venue":v["venue"],"hash":v["hash"],"nonce":v["nonce"],"operation":v["operation"],"action":v["request"]["action"]}))?;
        tracing::info!(venue=%v["venue"], hash=%v["hash"], nonce=%v["nonce"], operation=%v["operation"], action=%v["request"]["action"], "operation durably prepared");
        Ok(())
    }
    pub fn finish(&self, v: impl Serialize) -> Result<()> {
        tracing::info!(result=%serde_json::to_value(&v)?, "operation result confirmed");
        self.event("operation_result", v)?;
        self.write("pending.json", &Option::<Value>::None)
    }
    pub fn is_writable(&self) -> bool {
        self._lock.is_some()
    }
    pub fn next_nonce(&self) -> Result<u64> {
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let last = self.read::<u64>("nonce.json")?.unwrap_or(0);
        let n = crate::now_ms().max(last + 1);
        self.write("nonce.json", &n)?;
        Ok(n)
    }
}
