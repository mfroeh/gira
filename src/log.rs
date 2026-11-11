use std::{
    borrow::{self, Cow},
    collections::{self, BTreeMap},
    env,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    Error, Value, err,
    record::{self, Record},
    sync_directory, sync_file,
};

pub(crate) fn replay_all<K, V>(
    dir: &Path,
    max_sstable_idx: u64,
    into: &mut BTreeMap<K, Value<V>>,
) -> Result<u64, Error>
where
    K: DeserializeOwned + Eq + Ord,
    V: DeserializeOwned,
{
    let mut unflushed_logs = Vec::new();
    for f in fs::read_dir(dir).map_err(Error::io(dir))? {
        let f = f.map_err(Error::io(dir))?;
        match f
            .path()
            .file_name()
            .and_then(|fname| fname.to_str())
            .map(|s| s.parse::<u64>())
        {
            None | Some(Err(_)) => {
                return Err(err!("corrupted", &f.path(), "unexpected file name"));
            }
            Some(Ok(idx)) => {
                if idx <= max_sstable_idx {
                    let _ = fs::remove_file(f.path());
                } else {
                    unflushed_logs.push((idx, f.path()));
                }
            }
        }
    }
    unflushed_logs.sort_by_key(|&(idx, _)| idx);

    let mut max_unflushed_idx = None;
    for (idx, path) in unflushed_logs {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(Error::io(&path))?;
        replay(file, &path, into)?;
        max_unflushed_idx = Some(idx);
    }
    Ok(max_unflushed_idx.unwrap_or(max_sstable_idx + 1))
}

fn replay<K, V>(
    mut file: File,
    path: &Path,
    into: &mut collections::BTreeMap<K, Value<V>>,
) -> Result<(), Error>
where
    K: DeserializeOwned + Eq + Ord,
    V: DeserializeOwned,
{
    let len = file.metadata().map_err(Error::io(path))?.len();
    let mut off = 0;
    while off < len {
        let res = Record::<K, V>::read(&file, off, path);
        match res {
            Ok((record, new_off)) => {
                off = new_off;
                into.insert(record.key, record.value);
            }
            Err(err) => {
                let truncate = |file: &mut File| -> Result<(), Error> {
                    file.set_len(off)
                        .map_err(|e| err!("io", path, e, "truncate"))?;
                    sync_file(file, false).map_err(|e| err!("io", path, e, "fsync"))
                };

                match &err {
                    Error::IO { error, .. } => {
                        if ErrorKind::UnexpectedEof == error.kind() {
                            truncate(&mut file)?;
                            return Ok(());
                        }
                    }
                    // TODO: Try to handle some of these gracefully (e.g. zero-filled tail)
                    Error::Corrupted { .. } | Error::InvalidArgument { .. } => {
                        return Err(err);
                    }
                    Error::Poisoned { .. } => unreachable!(),
                };
                return Err(err);
            }
        };
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Log {
    poisoned: bool,
    path: PathBuf,
    file: File,
}

impl Log {
    pub(crate) fn open(path: PathBuf) -> Result<Log, Error> {
        let path = get_absolute(&path).map_err(Error::io(&path))?;

        let file = match File::create_new(&path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                OpenOptions::new().append(true).open(&path)
            }
            Ok(_) => {
                // Ensure the file creation is flushed to disk.
                sync_directory(path.parent().expect("no parent")).map_err(Error::io(&path))?;
                OpenOptions::new().append(true).open(&path)
            }
            Err(e) => Err(e),
        }
        .map_err(Error::io(&path))?;

        Ok(Log {
            poisoned: false,
            path: path.to_path_buf(),
            file,
        })
    }

    pub(crate) fn append<K: Serialize, V: Serialize>(
        &mut self,
        key: &K,
        value: Value<&V>,
    ) -> Result<(), Error> {
        if self.poisoned {
            return Err(err!("poisoned", &self.path));
        }
        record::write(key, value, &mut self.file, &self.path).map_err(|e| match &e {
            Error::InvalidArgument { .. } => e,
            Error::IO { .. } => {
                self.poisoned = true;
                e
            }
            Error::Corrupted { .. } | Error::Poisoned { .. } => unreachable!(),
        })?;

        sync_file(&self.file, false).map_err(|e| {
            self.poisoned = true;
            err!("poisoned", &self.path, "fsync: {e}")
        })
    }

    pub(crate) fn delete(self) -> Result<(), Error> {
        fs::remove_file(&self.path).map_err(Error::io(self.path))
    }
}

fn get_absolute(path: &Path) -> io::Result<Cow<'_, Path>> {
    if !path.is_absolute() {
        let mut absolute_path = env::current_dir()?;
        absolute_path.push(path);
        Ok(borrow::Cow::Owned(absolute_path))
    } else {
        Ok(borrow::Cow::Borrowed(path))
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, collections::BTreeMap, os::unix::fs::FileExt};

    use crate::tests::{open_rw_tmp_file, unique_tmp_path};

    use super::*;

    #[test]
    fn append_replay_happy() {
        let path = unique_tmp_path();
        let mut log = Log::open(path.clone()).unwrap();
        log.append(&String::from("abc"), Value::Some(&42)).unwrap();
        log.append(&String::from("123"), Value::Some(&123)).unwrap();

        let file = File::open(&path).unwrap();
        let mut map = BTreeMap::new();
        replay(file, &path, &mut map).unwrap();
        assert_eq!(
            map,
            map_macro::btree_map! {
                String::from("abc") => Value::Some(42),
                String::from("123") => Value::Some(123)
            },
        );
    }

    #[test]
    #[cfg(not(target_vendor = "apple"))]
    fn log_append_poison_on_write_failure() {
        let path = PathBuf::from("/dev/full");
        let mut log = Log::open(path).unwrap();
        assert_matches!(
            log.append(&String::from("abc"), Value::Some(&42)),
            Err(Error::IO { .. })
        );
        assert!(log.poisoned);

        assert_matches!(
            log.append(&String::from("abc"), Value::Some(&42)),
            Err(Error::Poisoned { .. })
        );
    }

    #[test]
    fn replay_type_mismatch() {
        let path = unique_tmp_path();
        let mut log = Log::open(path.clone()).unwrap();
        log.append(&String::from("abc"), Value::Some(&42)).unwrap();
        log.append(&String::from("123"), Value::Some(&123)).unwrap();

        let file = File::open(&path).unwrap();
        assert_matches!(
            replay(file, &path, &mut BTreeMap::<i64, Value<i32>>::new()),
            Err(Error::InvalidArgument { .. }),
        );
    }

    #[test]
    fn replay_first_corrupted() {
        let (file, path) = open_rw_tmp_file();
        let mut log = Log::open(path.clone()).unwrap();
        log.append(&String::from("abc"), Value::Some(&42)).unwrap();
        log.append(&String::from("123"), Value::Some(&123)).unwrap();
        file.write_all_at(&0x13371337u32.to_le_bytes(), 0).unwrap();

        let file = File::open(&path).unwrap();
        assert_matches!(
            replay(file, &path, &mut BTreeMap::<String, Value<i32>>::new()),
            Err(Error::Corrupted { .. }),
        );
    }

    #[test]
    fn replay_second_corrupted() {
        let (file, path) = open_rw_tmp_file();
        let mut log = Log::open(path.clone()).unwrap();
        log.append(&String::from("abc"), Value::Some(&42)).unwrap();
        let second_start = file.metadata().unwrap().len();
        log.append(&String::from("123"), Value::Some(&123)).unwrap();
        file.write_all_at(&0x13371337u32.to_le_bytes(), second_start)
            .unwrap();

        let file = File::open(&path).unwrap();
        assert_matches!(
            replay(file, &path, &mut BTreeMap::<String, Value<i32>>::new()),
            Err(Error::Corrupted { .. }),
        );
    }

    #[test]
    fn replay_second_unexpected_eof() {
        let (file, path) = open_rw_tmp_file();
        let mut log = Log::open(path.clone()).unwrap();
        log.append(&String::from("abc"), Value::Some(&42)).unwrap();
        let second_start = file.metadata().unwrap().len();
        log.append(&String::from("123"), Value::Some(&123)).unwrap();
        let file_len = file.metadata().unwrap().len();
        let second_len = file_len - second_start;

        for i in 1..second_len {
            // Simulate incomplete write.
            let file = OpenOptions::new()
                .write(true)
                .read(true)
                .open(&path)
                .unwrap();
            assert_eq!(file.metadata().unwrap().len(), file_len);
            file.set_len(second_start + i).unwrap();

            let mut map = BTreeMap::new();
            replay::<String, i32>(file, &path, &mut map).unwrap();
            assert_eq!(
                map,
                map_macro::btree_map! { String::from("abc") => Value::Some(42) },
                "{path:?}",
            );

            // File was truncated.
            assert_eq!(
                File::open(&path).unwrap().metadata().unwrap().len(),
                second_start
            );

            // Appending still works as expected.
            let mut log = Log::open(path.clone()).unwrap();
            log.append(&String::from("123"), Value::Some(&123)).unwrap();
            let mut map = BTreeMap::new();
            replay::<String, i32>(File::open(&path).unwrap(), &path, &mut map).unwrap();
            assert_eq!(
                map,
                map_macro::btree_map! {
                    String::from("abc") => Value::Some(42),
                    String::from("123") => Value::Some(123)
                },
                "{path:?}",
            );
        }
    }
}
