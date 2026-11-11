#![feature(rename_noreplace)]
#[cfg(not(unix))]
const _: () = compile_error!("this is a UNIX only crate");

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::io;
use std::io::ErrorKind::Interrupted;
use std::mem;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

mod log;
use log::Log;

mod sstable;
use sstable::SSTable;

mod record;

macro_rules! err {
    ("io",$path:expr,$error:expr) => {{
        let path: &Path = $path;
        Error::IO {
            path: path.to_path_buf(),
            error_msg: format!("{}", $error),
            error: $error,
        }
    }};
    ("io",$path:expr,$error:expr,$($arg:tt)*) => {
        {
        let path: &Path = $path;
            Error::IO {
            path: path.to_path_buf(),
            error_msg: format!("{}: {}", format!($($arg)*), $error),
            error: $error,
        } }
    };
    ("invalid_argument",$path:expr,$($arg:tt)*) => {
        {
            let path: &Path = $path;
            Error::InvalidArgument {
                path: path.to_path_buf(),
                msg: format!($($arg)*),
            }
        }
    };
    ("corrupted",$path:expr,$($arg:tt)*) => {
        {
            let path: &Path = $path;
            Error::Corrupted {
                path: path.to_path_buf(),
                msg: format!($($arg)*),
            }
        }
    };
    ("poisoned",$path:expr,$($arg:tt)*) => {
        {
        let path: &Path = $path;
        Error::Poisoned {
            path: path.to_path_buf(),
            msg: format!($($arg)*),
        }
        }
    };
    ("poisoned",$path:expr) => {
        {
        let path: &Path = $path;
        Error::Poisoned {
            path: path.to_path_buf(),
            msg: String::from(""),
        }
        }
    };
}

pub(crate) use err;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: IO: {error_msg}")]
    IO {
        path: PathBuf,
        #[source]
        error: io::Error,
        error_msg: String,
    },
    #[error("{path}: invalid argument: {msg}")]
    InvalidArgument { path: PathBuf, msg: String },
    #[error("{path}: corrupted: {msg}")]
    Corrupted { path: PathBuf, msg: String },
    #[error("{path}: poisoned: {msg}")]
    Poisoned { path: PathBuf, msg: String },
}

impl Error {
    fn io(path: impl AsRef<Path>) -> impl FnOnce(io::Error) -> Self {
        let path = path.as_ref().to_path_buf();
        move |e| err!("io", &path, e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum Value<V> {
    Tombstone,
    Some(V),
}

impl<V> Value<V> {
    fn as_ref(&self) -> Value<&V> {
        match self {
            Value::Tombstone => Value::Tombstone,
            Value::Some(v) => Value::Some(v),
        }
    }
}

impl<V> From<V> for Value<V> {
    fn from(value: V) -> Self {
        Self::Some(value)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Return<'a, V> {
    #[default]
    None,
    Borrow(&'a V),
    Owned(V),
}

impl<V> From<Value<V>> for Return<'_, V> {
    fn from(value: Value<V>) -> Self {
        match value {
            Value::Tombstone => Self::None,
            Value::Some(v) => Self::Owned(v),
        }
    }
}

impl<'a, V> From<Value<&'a V>> for Return<'a, V> {
    fn from(value: Value<&'a V>) -> Self {
        match value {
            Value::Tombstone => Self::None,
            Value::Some(v) => Self::Borrow(v),
        }
    }
}

#[derive(Debug)]
struct Config {
    max_in_memory_entries: usize,
    log_fsync_interval: u64,
}

#[derive(Debug)]
pub struct StoreBuilder {
    cfg: Config,
}

impl Default for StoreBuilder {
    fn default() -> Self {
        Self {
            cfg: Config {
                max_in_memory_entries: 1024,
                log_fsync_interval: 32,
            },
        }
    }
}

impl StoreBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open<K, V>(self, dir_path: impl AsRef<Path>) -> Result<Store<K, V>, Error>
    where
        K: Serialize + DeserializeOwned + Ord,
        V: Serialize + DeserializeOwned,
    {
        Store::open(dir_path, self.cfg)
    }

    pub fn max_in_memory_entries(mut self, new: usize) -> Self {
        self.cfg.max_in_memory_entries = new;
        self
    }

    pub fn log_fsync_interval(mut self, new: u64) -> Self {
        self.cfg.log_fsync_interval = new;
        self
    }
}

pub struct Store<K, V> {
    cfg: Config,
    dir: PathBuf,
    map: BTreeMap<K, Value<V>>,
    log: Log,
    sstables: Vec<SSTable<K, V>>,
    max_sstable_idx: u64,
    poisoned: bool,
}

impl<K, V> Store<K, V>
where
    K: Serialize + DeserializeOwned + Ord,
    V: Serialize + DeserializeOwned,
{
    fn open(dir_path: impl AsRef<Path>, cfg: Config) -> Result<Self, Error> {
        let dir_path = dir_path.as_ref().to_path_buf();

        let sstables_dir = dir_path.join("sstables");
        fs::create_dir_all(&sstables_dir).map_err(Error::io(&sstables_dir))?;
        let sstables = sstable::open_all(&sstables_dir)?;
        let max_idx = sstables.last().map(|t| t.idx).unwrap_or_default();

        let sstables_tmp_dir = dir_path.join("sstables_tmp");
        fs::create_dir_all(&sstables_tmp_dir).map_err(Error::io(sstables_tmp_dir))?;

        let logs_dir = dir_path.join("logs");
        fs::create_dir_all(&logs_dir).map_err(Error::io(&logs_dir))?;

        let mut map: BTreeMap<K, Value<V>> = BTreeMap::new();
        let log_idx = log::replay_all(&logs_dir, max_idx, &mut map)?;
        let log_path = logs_dir.join(log_idx.to_string());
        let log = Log::open(log_path, cfg.log_fsync_interval)?;

        Ok(Self {
            dir: dir_path,
            max_sstable_idx: max_idx,
            poisoned: false,
            cfg,
            map,
            log,
            sstables,
        })
    }

    pub fn put(&mut self, k: K, v: V) -> Result<Return<'_, V>, Error> {
        if self.poisoned {
            return Err(err!("poisoned", &self.dir));
        }
        self.log.append(&k, Value::Some(&v))?;
        let v: Return<'_, V> = self
            .map
            .insert(k, Value::from(v))
            .map(Return::from)
            .unwrap_or_default();
        if self.map.len() > self.cfg.max_in_memory_entries {
            self.flush()?;
        }
        Ok(v)
    }

    pub fn delete(&mut self, k: K) -> Result<(), Error> {
        if self.poisoned {
            return Err(err!("poisoned", &self.dir));
        }
        self.log.append::<K, V>(&k, Value::Tombstone)?;
        self.map.insert(k, Value::Tombstone);
        if self.map.len() > self.cfg.max_in_memory_entries {
            self.flush()?;
        }
        Ok(())
    }

    pub fn get<'a, Q>(&'a self, k: &Q) -> Result<Return<'a, V>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if let Some(v) = self.map.get(k) {
            return Ok(Return::from(v.as_ref()));
        }
        self.lookup_disk(k)
    }

    fn lookup_disk<Q>(&self, k: &Q) -> Result<Return<'_, V>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        for table in self.sstables.iter().rev() {
            if let Some(value) = table.binary_search(k)? {
                return Ok(Return::from(value));
            }
        }
        Ok(Return::None)
    }

    fn flush(&mut self) -> Result<(), Error> {
        let idx = self.max_sstable_idx + 1;
        self.max_sstable_idx += 1;

        let tmp_path = self.dir.join("sstables_tmp").join(idx.to_string());
        let sstables_dir = self.dir.join("sstables");
        let path = sstables_dir.join(idx.to_string());
        let res = sstable::flush(&tmp_path, &path, &self.map)
            .and(
                sync_directory(&sstables_dir)
                    .map_err(|e| err!("io", &path, e, "fsync {}", sstables_dir.display())),
            )
            .and(SSTable::open(path.clone(), idx));
        match res {
            Ok(table) => {
                self.map.clear();
                self.sstables.push(table);

                let new_log_path = self.dir.join("logs").join((idx + 1).to_string());
                let log = Log::open(new_log_path, self.cfg.log_fsync_interval).map_err(|e| {
                    self.poisoned = true;
                    err!("poisoned", &self.dir, "{e}")
                })?;
                let old_log = mem::replace(&mut self.log, log);
                let _ = old_log.delete();
                Ok(())
            }
            Err(err) => match err {
                Error::InvalidArgument { .. } => {
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(&tmp_path);
                    Err(err)
                }
                Error::IO { .. } | Error::Corrupted { .. } => {
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(&tmp_path);
                    Ok(())
                }
                Error::Poisoned { .. } => unreachable!(),
            },
        }
    }
}

pub(crate) fn sync_directory(dir: impl AsRef<Path>) -> io::Result<()> {
    let file = fs::File::open(dir)?;
    sync_file(&file, true)
}

pub(crate) fn sync_file(file: &File, metadata: bool) -> io::Result<()> {
    #[cfg(not(target_vendor = "apple"))]
    let fsync = |file: &File| {
        if metadata {
            unsafe { libc::fsync(file.as_raw_fd()) }
        } else {
            unsafe { libc::fdatasync(file.as_raw_fd()) }
        }
    };
    #[cfg(target_vendor = "apple")]
    let fsync = |file: &File| unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };

    loop {
        if fsync(file) == 0 {
            break;
        }

        let err = io::Error::last_os_error();
        if err.kind() != Interrupted {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        assert_matches, env, fs,
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    use super::*;

    static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn unique_tmp_path() -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "{}_{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            FILE_COUNTER.fetch_add(1, Ordering::AcqRel),
        ));
        path
    }

    pub(crate) fn open_rw_tmp_file() -> (File, PathBuf) {
        let path = unique_tmp_path();
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (file, path)
    }

    #[test]
    fn happy_in_memory_with_replay() {
        let path = dbg!(unique_tmp_path());
        let mut store: Store<String, i32> = StoreBuilder::new().open(&path).unwrap();
        store.put(String::from("abc"), 42).unwrap();
        assert_eq!(store.get("abc").unwrap(), Return::Borrow(&42));
        store.put(String::from("123"), 123).unwrap();
        assert_eq!(store.get("123").unwrap(), Return::Borrow(&123));
        store.delete(String::from("abc")).unwrap();
        assert_eq!(store.get("abc").unwrap(), Return::None);

        assert_eq!(
            store.map,
            map_macro::btree_map! { String::from("123") => Value::Some(123), String::from("abc") => Value::Tombstone }
        );

        // Replay from log.
        store = StoreBuilder::new().open(&path).unwrap();
        assert_eq!(store.get("123").unwrap(), Return::Borrow(&123));
        assert_eq!(
            store.map,
            map_macro::btree_map! { String::from("123") => Value::Some(123), String::from("abc") => Value::Tombstone  }
        );

        // Append some more.
        store.put(String::from("1337"), 1337).unwrap();
        assert_eq!(store.get("1337").unwrap(), Return::Borrow(&1337));
        assert_eq!(
            store.map,
            map_macro::btree_map! { String::from("123") => Value::Some(123), String::from("1337") => Value::Some(1337), String::from("abc") => Value::Tombstone  }
        );
    }

    #[test]
    fn happy_flush_single_table() {
        let path = dbg!(unique_tmp_path());
        let mut store: Store<String, i32> = StoreBuilder::new()
            .max_in_memory_entries(4)
            .open(&path)
            .unwrap();
        store.put(String::from("1"), 1).unwrap();
        store.put(String::from("2"), 2).unwrap();
        store.delete(String::from("2")).unwrap();
        store.put(String::from("3"), 3).unwrap();
        store.delete(String::from("4")).unwrap();
        // No flush, existing entry.
        store.put(String::from("4"), 44).unwrap();

        // Force flush to disk.
        store.put(String::from("5"), 5).unwrap();
        assert_eq!(store.map, map_macro::btree_map! {});

        // Lookup from disk.
        assert_matches!(store.get("1"), Ok(Return::Owned(1)));
        assert_matches!(store.get("2"), Ok(Return::None));
        assert_matches!(store.get("5"), Ok(Return::Owned(5)));
        assert_matches!(store.get("4"), Ok(Return::Owned(44)));
        assert_matches!(store.get("6"), Ok(Return::None));

        // In-memory takes precedence over disk.
        store.put(String::from("4"), 4).unwrap();
        store.put(String::from("1"), 11).unwrap();
        store.delete(String::from("5")).unwrap();
        assert_matches!(store.get("4"), Ok(Return::Borrow(&4)));
        assert_matches!(store.get("1"), Ok(Return::Borrow(&11)));
        assert_matches!(store.get("5"), Ok(Return::None));

        // Never deleted/inserted entries return None.
        assert_matches!(store.get("1337"), Ok(Return::None));
    }

    #[test]
    fn happy_flush_many_tables() {
        let path = dbg!(unique_tmp_path());
        let mut store: Store<_, _> = StoreBuilder::new()
            .max_in_memory_entries(4)
            .open(&path)
            .unwrap();
        // Force 100 flushes.
        let flushes = 100;
        for f in 1..=flushes {
            for i in 1..=store.cfg.max_in_memory_entries + 1 {
                store.put(i, f * i).unwrap();
            }
        }

        // Lookup for any existing succeeds and matches the newest write.
        for i in 1..=store.cfg.max_in_memory_entries + 1 {
            assert_eq!(store.get(&i).unwrap(), Return::Owned(i * flushes));
        }
    }

    #[test]
    fn happy_reopen_after_flush() {
        let path = dbg!(unique_tmp_path());
        let mut store: Store<String, i32> = StoreBuilder::new()
            .max_in_memory_entries(2)
            .open(&path)
            .unwrap();
        store.put(String::from("1"), 1).unwrap();
        store.put(String::from("2"), 2).unwrap();
        // Force flush to disk.
        store.put(String::from("3"), 3).unwrap();
        assert_eq!(store.map, map_macro::btree_map! {});
        // Reopen.
        store = StoreBuilder::new()
            .max_in_memory_entries(2)
            .open(&path)
            .unwrap();
        assert_eq!(store.map, map_macro::btree_map! {});

        store.put(String::from("4"), 4).unwrap();
        store.put(String::from("5"), 5).unwrap();
        // Flush next table.
        store.put(String::from("6"), 6).unwrap();
        assert_eq!(store.map, map_macro::btree_map! {});
        // Reopen.
        store = StoreBuilder::new()
            .max_in_memory_entries(2)
            .open(&path)
            .unwrap();
        assert_eq!(store.map, map_macro::btree_map! {});

        // Still finds all.
        for i in 1..=6 {
            assert_eq!(store.get(&i.to_string()).unwrap(), Return::Owned(i));
        }
    }
}
