use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub struct Store {
    root: PathBuf,
    _lock: Option<File>,
    mutation: std::sync::Mutex<()>,
    event_lock: std::sync::Mutex<()>,
    write_lock: std::sync::Mutex<()>,
}
impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
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
        })
    }
    pub fn readonly(path: impl AsRef<Path>) -> Self {
        Self {
            root: path.as_ref().to_path_buf(),
            _lock: None,
            mutation: std::sync::Mutex::new(()),
            event_lock: std::sync::Mutex::new(()),
            write_lock: std::sync::Mutex::new(()),
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
        std::fs::rename(tmp, self.root.join(name))?;
        File::open(&self.root)?.sync_all()?;
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
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events.jsonl"))?;
        serde_json::to_writer(
            &mut f,
            &json!({"time_ms":crate::now_ms(),"kind":kind,"data":v}),
        )?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        Ok(())
    }
    pub fn pending(&self) -> Result<Option<Value>> {
        self.read("pending.json")
    }
    pub fn begin(&self, v: Value) -> Result<()> {
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
