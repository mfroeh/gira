use std::{
    borrow::Borrow,
    collections::BTreeMap,
    fs::{self, File},
    io::{BufWriter, Write},
    marker::PhantomData,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    Error, Value, err,
    record::{self, Record},
    sync_file,
};

pub(crate) struct SSTable<K, V> {
    pub(crate) idx: u64,
    offsets: Vec<u64>,
    file: File,
    path: PathBuf,
    _pd: PhantomData<fn() -> (K, V)>,
}

impl<K, V> SSTable<K, V>
where
    K: DeserializeOwned,
    V: DeserializeOwned,
{
    pub(crate) fn open(path: PathBuf, idx: u64) -> Result<SSTable<K, V>, Error> {
        let file = File::open(&path).map_err(Error::io(&path))?;
        let meta = file.metadata().map_err(Error::io(&path))?;

        let mut buf = [0u8; size_of::<u64>() + 2 * size_of::<u32>()];
        let read_off = (meta.len().checked_sub(buf.len() as u64)).ok_or(err!(
            "corrupted",
            &path,
            "file is < {} bytes long",
            buf.len()
        ))?;
        file.read_exact_at(&mut buf, read_off)
            .map_err(Error::io(&path))?;
        let len: u64 = u64::from_le_bytes(*buf[0..8].as_array::<8>().unwrap());
        let offsets_checksum: u32 = u32::from_le_bytes(*buf[8..12].as_array::<4>().unwrap());
        let len_checksum: u32 = u32::from_le_bytes(*buf[12..].as_array::<4>().unwrap());

        if crc32fast::hash(&buf[0..8]) != len_checksum {
            return Err(err!("corrupted", &path, "length checksum mismatch"));
        }

        let mut offset_bytes = vec![0u8; len as usize * size_of::<u64>()];
        let read_off = (meta
            .len()
            .checked_sub((buf.len() + offset_bytes.len()) as u64))
        .ok_or(err!(
            "corrupted",
            &path,
            "file len is {}, but record count is {}",
            meta.len(),
            len
        ))?;
        file.read_exact_at(&mut offset_bytes, read_off)
            .map_err(Error::io(&path))?;

        if crc32fast::hash(&offset_bytes) != offsets_checksum {
            return Err(err!("corrupted", &path, "offsets checksum mismatch"));
        }

        let offsets: Vec<u64> = offset_bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| u64::from_le_bytes(*bytes.as_array::<8>().unwrap()))
            .collect();

        Ok(SSTable {
            path: path.to_path_buf(),
            _pd: PhantomData,
            idx,
            offsets,
            file,
        })
    }

    pub(crate) fn get_key(&self, idx: usize) -> Result<Option<K>, Error> {
        if idx >= self.offsets.len() {
            return Ok(None);
        }

        let key: K = Record::<K, V>::read_key(&self.file, self.offsets[idx], &self.path)?;
        Ok(Some(key))
    }

    pub(crate) fn get_value(&self, idx: usize) -> Result<Option<Value<V>>, Error> {
        if idx >= self.offsets.len() {
            return Ok(None);
        }

        let value: Value<V> =
            Record::<K, V>::read_value(&self.file, self.offsets[idx], &self.path)?;
        Ok(Some(value))
    }

    pub(crate) fn binary_search<Q>(&self, key: &Q) -> Result<Option<Value<V>>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if self.is_empty() {
            return Ok(None);
        }

        let (mut l, mut r) = (0, self.offsets.len() - 1);
        while l <= r {
            let mid = (l + r) / 2;
            let seen: K = self.get_key(mid)?.unwrap();
            match key.cmp(seen.borrow()) {
                std::cmp::Ordering::Less => {
                    let Some(new_r) = mid.checked_sub(1) else {
                        return Ok(None);
                    };
                    r = new_r;
                }
                std::cmp::Ordering::Equal => return self.get_value(mid),
                std::cmp::Ordering::Greater => {
                    let Some(new_l) = mid.checked_add(1) else {
                        return Ok(None);
                    };
                    l = new_l;
                }
            }
        }
        Ok(None)
    }
}

impl<K, V> SSTable<K, V> {
    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub(crate) struct SSTableIter<'a, K, V> {
    table: &'a SSTable<K, V>,
    idx: usize,
    _pd: PhantomData<fn() -> (K, V)>,
}

impl<K: DeserializeOwned, V: DeserializeOwned> Iterator for SSTableIter<'_, K, V> {
    type Item = Result<Record<K, V>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.table.len() {
            None
        } else {
            let off = self.table.offsets[self.idx];
            self.idx += 1;
            // TODO: Can do chunking on reads on file side to improve performance.
            Some(
                Record::<K, V>::read(&self.table.file, off, &self.table.path)
                    .map(|(record, _)| record),
            )
        }
    }
}

impl<'a, K: DeserializeOwned, V: DeserializeOwned> IntoIterator for &'a SSTable<K, V> {
    type Item = Result<Record<K, V>, Error>;

    type IntoIter = SSTableIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        SSTableIter {
            table: self,
            idx: 0,
            _pd: PhantomData,
        }
    }
}

pub(crate) fn open_all<K: DeserializeOwned, V: DeserializeOwned>(
    dir: &Path,
) -> Result<Vec<SSTable<K, V>>, Error> {
    let mut sstables = Vec::new();
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
                let table = SSTable::open(f.path(), idx)?;
                sstables.push(table);
            }
        }
    }
    sstables.sort_by_key(|table| table.idx);
    Ok(sstables)
}

pub(crate) fn flush<K: Serialize, V: Serialize>(
    tmp_path: &Path,
    path: &Path,
    map: &BTreeMap<K, Value<V>>,
) -> Result<(), Error> {
    let file = File::create_new(tmp_path).map_err(Error::io(tmp_path))?;
    write(file, tmp_path, map)?;

    fs::rename_noreplace(tmp_path, path).map_err(Error::io(&path))?;
    Ok(())
}

fn write<K: Serialize, V: Serialize>(
    file: File,
    path: &Path,
    map: &BTreeMap<K, Value<V>>,
) -> Result<(), Error> {
    let mut writer = BufWriter::with_capacity(8096, file);
    let mut offsets: Vec<u64> = Vec::with_capacity(map.len());
    let mut off = 0;
    for (k, v) in map.iter() {
        let written = record::write(k, v.as_ref(), &mut writer, path)?;
        offsets.push(off);
        off = off
            .checked_add(written)
            .ok_or(err!("invalid_argument", path, "offset overflow"))?;
    }

    let offsets_bytes = offsets
        .into_iter()
        .flat_map(|o| o.to_le_bytes())
        .collect::<Vec<u8>>();
    writer.write_all(&offsets_bytes).map_err(Error::io(path))?;

    let len_bytes = (map.len() as u64).to_le_bytes();
    writer.write_all(&len_bytes).map_err(Error::io(path))?;

    // checksums
    writer
        .write_all(&crc32fast::hash(&offsets_bytes).to_le_bytes())
        .map_err(Error::io(path))?;
    writer
        .write_all(&crc32fast::hash(&len_bytes).to_le_bytes())
        .map_err(Error::io(path))?;

    match writer.into_inner() {
        Err(e) => {
            let e = e.into_error();
            Err(err!("io", path, e))
        }
        Ok(file) => sync_file(&file, false).map_err(Error::io(path)),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        record::DiskRecordHeader,
        sstable,
        tests::{open_rw_tmp_file, unique_tmp_path},
    };

    use super::*;

    #[test]
    fn write_happy() {
        let map: BTreeMap<String, Value<i32>> = map_macro::btree_map! {
            String::from("123") => Value::Some(123),
            String::from("456") => Value::Tombstone,
        };
        let expected_lengths = [
            size_of::<DiskRecordHeader>()
                + size_of::<u32>()
                + bitcode::serialize(&String::from("123")).unwrap().len()
                + bitcode::serialize(&Value::Some(123)).unwrap().len(),
            size_of::<DiskRecordHeader>()
                + size_of::<u32>()
                + bitcode::serialize(&String::from("456")).unwrap().len()
                + bitcode::serialize(&Value::<i32>::Tombstone).unwrap().len(),
        ]
        .map(|x| x as u64);

        let (file, path) = open_rw_tmp_file();
        write(file, &path, &map).unwrap();

        let file = File::open(&path).unwrap();
        let meta = file.metadata().unwrap();

        let expected_offset_table_len: u64 = 2 * size_of::<u64>() as u64;
        let expected_len_and_checksums_len: u64 = (size_of::<u64>() + 2 * size_of::<u32>()) as u64;
        assert_eq!(
            meta.len(),
            expected_lengths.iter().cloned().sum::<u64>()
                + expected_offset_table_len
                + expected_len_and_checksums_len
        );
    }

    #[test]
    fn write_read_happy() {
        let map: BTreeMap<String, Value<i32>> = map_macro::btree_map! {
            String::from("456") => Value::Tombstone,
            String::from("123") => Value::Some(123),
        };

        let (file, path) = open_rw_tmp_file();
        write(file, &path, &map).unwrap();
        let table: SSTable<String, i32> = SSTable::open(path, 1).unwrap();

        assert_eq!(table.get_key(0).unwrap().unwrap(), String::from("123"));
        assert_eq!(table.get_key(1).unwrap().unwrap(), String::from("456"));
        assert_eq!(table.get_value(0).unwrap().unwrap(), Value::Some(123));
        assert_eq!(table.get_value(1).unwrap().unwrap(), Value::Tombstone);
        assert_eq!(table.get_value(2).unwrap(), None);
    }

    #[test]
    fn binary_search_happy() {
        let map: BTreeMap<i32, Value<i32>> = map_macro::btree_map! {
            1 => Value::Some(1),
            2 => Value::Some(2),
            3 => Value::Some(3),
            4 => Value::Some(4),
            5 => Value::Some(5),
            6 => Value::Some(6),
            7 => Value::Tombstone,
        };

        let (file, path) = open_rw_tmp_file();
        write(file, &path, &map).unwrap();
        let table: SSTable<i32, i32> = SSTable::open(path, 1).unwrap();

        assert_eq!(table.binary_search(&0).unwrap(), None);
        assert_eq!(table.binary_search(&-10).unwrap(), None);
        assert_eq!(table.binary_search(&7).unwrap().unwrap(), Value::Tombstone);
        for i in 1..=6 {
            assert_eq!(table.binary_search(&i).unwrap().unwrap(), Value::Some(i));
        }
    }
}
