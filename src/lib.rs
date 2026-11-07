#![feature(read_array, read_le, path_absolute_method)]
use std::borrow::Borrow;
use std::collections;
use std::fs;
use std::io;
use std::path;
use std::time;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

mod log;
mod sstable;
use sstable::SSTable;

use crate::Value::Tombstone;
use crate::log::Entry;
use crate::log::Log;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("log")]
    LogError(#[from] log::Error),
    #[error("sstable")]
    SSTable(#[from] sstable::Error),

    #[error("io")]
    IoTodo(#[from] io::Error),
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

    fn as_opt(&self) -> Option<&V> {
        match self {
            Value::Tombstone => None,
            Value::Some(v) => Some(v),
        }
    }

    fn map<U>(self, f: impl FnOnce(V) -> U) -> Value<U> {
        match self {
            Tombstone => Tombstone,
            Value::Some(v) => Value::Some(f(v)),
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
            Tombstone => Self::None,
            Value::Some(v) => Self::Owned(v),
        }
    }
}

impl<'a, V> From<Value<&'a V>> for Return<'a, V> {
    fn from(value: Value<&'a V>) -> Self {
        match value {
            Tombstone => Self::None,
            Value::Some(v) => Self::Borrow(v),
        }
    }
}

pub struct KvStore<K, V, const MAX_TABLE_ENTRIES: usize = 1024> {
    store: collections::BTreeMap<K, Value<V>>,
    log: Log,
    sstables: Vec<path::PathBuf>,
    dir_path: path::PathBuf,
}

impl<K, V, const MAX_TABLE_ENTRIES: usize> KvStore<K, V, MAX_TABLE_ENTRIES>
where
    K: Serialize + DeserializeOwned + Ord,
    V: Serialize + DeserializeOwned,
{
    pub fn with_dir(dir_path: impl AsRef<path::Path>) -> Result<Self, Error> {
        let dir_path = dir_path.as_ref().absolute()?;
        let sstables_dir = dir_path.join("sstables");
        fs::create_dir_all(&sstables_dir)?;

        let mut sstables = Vec::new();
        for f in fs::read_dir(&sstables_dir)? {
            let f = f?;
            sstables.push(f.path());
        }
        sstables.sort();

        let log_path = dir_path.join("log");
        let store = log::replay_existing_log(&log_path)?;
        let log = log::Log::open(&log_path)?;
        Ok(Self {
            store,
            log,
            dir_path,
            sstables,
        })
    }

    pub fn put(&mut self, k: K, v: V) -> Result<Return<'_, V>, Error> {
        self.log.append(Entry::Insert(&k, &v))?;
        let v: Return<'_, V> = self
            .store
            .insert(k, Value::from(v))
            .map(Return::from)
            .unwrap_or_default();
        if self.store.len() > MAX_TABLE_ENTRIES {
            self.flush()?;
            // TODO: Start the new log
        }
        Ok(v)
    }

    pub fn delete(&mut self, k: K) -> Result<(), Error> {
        self.log.append::<K, V>(Entry::Remove(&k))?;
        self.store.insert(k, Value::Tombstone);
        if self.store.len() > MAX_TABLE_ENTRIES {
            self.flush()?;
            // TODO: Start the new log
        }
        Ok(())
    }

    pub fn get<'a, Q>(&'a self, k: &Q) -> Result<Return<'a, V>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if let Some(v) = self.store.get(k) {
            return Ok(Return::from(v.as_ref()));
        }
        self.lookup_disk(k)
    }
}

impl<K, V, const MAX_TABLE_ENTRIES: usize> KvStore<K, V, MAX_TABLE_ENTRIES>
where
    K: Serialize + DeserializeOwned + Ord,
    V: Serialize + DeserializeOwned,
{
    fn flush(&mut self) -> Result<(), Error> {
        let new_table_path = self.dir_path.join(format!(
            "sstables/{}",
            time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros()
        ));
        crate::sstable::write(&new_table_path, &mut self.store)?;
        self.store.clear();
        self.sstables.push(new_table_path);
        Ok(())
    }

    fn lookup_disk<Q>(&self, k: &Q) -> Result<Return<'_, V>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        for path in self.sstables.iter().rev() {
            let mut sstable: SSTable<K, V> = SSTable::read(path)?;
            if let Some(value) = sstable.binary_search(k)? {
                return Ok(Return::from(value));
            }
        }

        Ok(Return::None)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    use super::*;

    static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn unique_tmp_path() -> path::PathBuf {
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

    pub(crate) fn open_rw_tmp_file() -> (fs::File, path::PathBuf) {
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
        let path = unique_tmp_path();
        let mut store: KvStore<String, i32> = KvStore::with_dir(&path).unwrap();
        store.put(String::from("abc"), 42).unwrap();
        assert_eq!(store.get("abc").unwrap(), Return::Borrow(&42));
        store.put(String::from("123"), 123).unwrap();
        assert_eq!(store.get("123").unwrap(), Return::Borrow(&123));
        store.delete(String::from("abc")).unwrap();
        assert_eq!(store.get("abc").unwrap(), Return::None);

        assert_eq!(
            store.store,
            map_macro::btree_map! { String::from("123") => Value::Some(123), String::from("abc") => Value::Tombstone }
        );

        // Replay from log.
        store = KvStore::with_dir(&path).unwrap();
        assert_eq!(store.get("123").unwrap(), Return::Borrow(&123));
        assert_eq!(
            store.store,
            map_macro::btree_map! { String::from("123") => Value::Some(123), String::from("abc") => Value::Tombstone  }
        );

        // Append some more.
        store.put(String::from("1337"), 1337).unwrap();
        assert_eq!(store.get("1337").unwrap(), Return::Borrow(&1337));
        assert_eq!(
            store.store,
            map_macro::btree_map! { String::from("123") => Value::Some(123), String::from("1337") => Value::Some(1337), String::from("abc") => Value::Tombstone  }
        );
    }

    #[test]
    fn happy_flush_single_table() {
        let path = dbg!(unique_tmp_path());
        let mut store: KvStore<String, i32, 4> = KvStore::with_dir(&path).unwrap();
        store.put(String::from("1"), 1).unwrap();
        store.put(String::from("2"), 2).unwrap();
        store.delete(String::from("2")).unwrap();
        store.put(String::from("3"), 3).unwrap();
        store.delete(String::from("4")).unwrap();
        // No flush, existing entry.
        store.put(String::from("4"), 44).unwrap();

        // Force flush to disk.
        store.put(String::from("5"), 5).unwrap();
        assert_eq!(store.store, map_macro::btree_map! {});

        // Lookup from disk.
        matches!(store.get("1"), Ok(Return::Owned(1)));
        matches!(store.get("2"), Ok(Return::None));
        matches!(store.get("5"), Ok(Return::Owned(5)));
        matches!(store.get("4"), Ok(Return::None));
        matches!(store.get("6"), Ok(Return::None));

        // In-memory takes precedence over disk.
        store.put(String::from("4"), 4).unwrap();
        store.put(String::from("1"), 11).unwrap();
        store.delete(String::from("5")).unwrap();
        matches!(store.get("4"), Ok(Return::Owned(4)));
        matches!(store.get("1"), Ok(Return::Owned(11)));
        matches!(store.get("5"), Ok(Return::None));

        // Never deleted/inserted entries return None.
        matches!(store.get("1337"), Ok(Return::None));
    }

    #[test]
    fn happy_flush_many_tables() {
        let path = dbg!(unique_tmp_path());
        const MAX_ENTRIES: usize = 4;
        let mut store: KvStore<_, _, MAX_ENTRIES> = KvStore::with_dir(&path).unwrap();
        // Force 100 flushes.
        let flushes = 100;
        for f in 1..=flushes {
            for i in 1..=MAX_ENTRIES + 1 {
                store.put(i, f * i).unwrap();
            }
        }

        // Lookup for any existing succeeds and matches the newest write.
        for i in 1..=MAX_ENTRIES + 1 {
            assert_eq!(store.get(&i).unwrap(), Return::Owned(i * flushes));
        }
    }
}
