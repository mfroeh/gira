#![feature(read_array)]
#![cfg_attr(test, feature(hash_map_macro))]
use std::borrow;
use std::collections;
use std::hash;
use std::path;

use serde::Serialize;
use serde::de::DeserializeOwned;

mod log;

use crate::log::Entry;
use crate::log::Log;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("log")]
    LogError(#[from] log::Error),
}

pub struct KvStore<K, V> {
    store: collections::HashMap<K, V>,
    log: Log,
}

impl<K, V> KvStore<K, V>
where
    K: Serialize + DeserializeOwned + Eq + hash::Hash,
    V: Serialize + DeserializeOwned,
{
    pub fn with_log_path(path: impl AsRef<path::Path>) -> Result<Self, Error> {
        let store = log::replay_existing_log(&path)?;
        let log = log::Log::open(&path)?;
        Ok(Self { log, store })
    }

    pub fn put(&mut self, k: K, v: V) -> Result<Option<V>, Error> {
        self.log.append(Entry::Insert(&k, &v))?;
        Ok(self.store.insert(k, v))
    }

    pub fn delete(&mut self, k: &K) -> Result<Option<V>, Error> {
        self.log.append::<K, V>(Entry::Remove(k))?;
        Ok(self.store.remove(k))
    }

    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: borrow::Borrow<Q>,
        Q: Eq + hash::Hash + ?Sized,
    {
        self.store.get(k)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, hash_map,
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
    fn happy() {
        let path = unique_tmp_path();
        let mut store: KvStore<String, i32> = KvStore::with_log_path(&path).unwrap();
        store.put(String::from("abc"), 42).unwrap();
        assert_eq!(store.get("abc"), Some(&42));
        store.put(String::from("123"), 123).unwrap();
        assert_eq!(store.get("123"), Some(&123));
        store.delete(&String::from("abc")).unwrap();
        assert_eq!(store.get("abc"), None);

        assert_eq!(store.store, hash_map! { String::from("123") => 123 });

        // Replay from log.
        store = KvStore::with_log_path(&path).unwrap();
        assert_eq!(store.get("123"), Some(&123));
        assert_eq!(store.store, hash_map! { String::from("123") => 123 });

        // Append some more.
        store.put(String::from("1337"), 1337).unwrap();
        assert_eq!(store.get("1337"), Some(&1337));
        assert_eq!(
            store.store,
            hash_map! { String::from("123") => 123, String::from("1337") => 1337 }
        );
    }
}
