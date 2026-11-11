use std::{fs::File, io::Write, os::unix::fs::FileExt, path::Path};

use serde::{Serialize, de::DeserializeOwned};

use crate::{Error, Value, err};

const RECORD_MAGIC_NUMBER: u32 = 0xDEADBEEF;
const RECORD_TUPLE_MAX_SIZE: u64 = u32::MAX as u64;

#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct DiskRecordHeader {
    magic: u32,
    key_len: u32,
    value_len: u32,
    key_checksum: u32,
    value_checksum: u32,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Record<K, V> {
    pub(crate) key: K,
    pub(crate) value: Value<V>,
}

pub(crate) fn write<K: Serialize, V: Serialize>(
    key: &K,
    value: Value<&V>,
    mut writer: impl Write,
    path: &Path,
) -> Result<u64, Error> {
    let (key, value, key_len, value_len) = {
        let key = bitcode::serialize(key)
            .map_err(|e| err!("invalid_argument", path, "serialize key: {e}"))?;
        let value = bitcode::serialize(&value)
            .map_err(|e| err!("invalid_argument", path, "serialize value: {e}"))?;
        let key_len = key.len();
        let value_len = value.len();

        let size = (key_len + value_len) as u64;
        if size > RECORD_TUPLE_MAX_SIZE {
            return Err(err!(
                "invalid_argument",
                path,
                "serialized tuple size: {size} > {RECORD_TUPLE_MAX_SIZE}"
            ));
        }
        (key, value, key_len as u32, value_len as u32)
    };
    let header = DiskRecordHeader {
        magic: RECORD_MAGIC_NUMBER,
        key_checksum: crc32fast::hash(&key),
        value_checksum: crc32fast::hash(&value),
        key_len,
        value_len,
    };
    let header = bytemuck::bytes_of(&header);
    let checksum = crc32fast::hash(header);

    let mut buf = header.to_vec();
    buf.extend(checksum.to_le_bytes());
    buf.extend(key);
    buf.extend(value);
    writer.write_all(&buf).map_err(Error::io(path))?;
    Ok(buf.len() as u64)
}

impl<K: DeserializeOwned, V: DeserializeOwned> Record<K, V> {
    pub(crate) fn read(file: &File, off: u64, path: &Path) -> Result<(Self, u64), Error> {
        let (header, off) = Self::read_and_validate_header(file, off, path)?;
        let mut buf = vec![0; header.key_len as usize + header.value_len as usize];
        file.read_exact_at(&mut buf, off).map_err(Error::io(path))?;
        let (key, value) = buf.split_at(header.key_len as usize);

        if crc32fast::hash(key) != header.key_checksum {
            return Err(err!("corrupted", path, "key checksum mismatch"));
        }
        if crc32fast::hash(value) != header.value_checksum {
            return Err(err!("corrupted", path, "value checksum mismatch"));
        }
        let key: K = bitcode::deserialize(key)
            .map_err(|e| err!("invalid_argument", path, "deserialize key {e}"))?;
        let value: Value<V> = bitcode::deserialize(value)
            .map_err(|e| err!("invalid_argument", path, "deserialize value {e}"))?;

        Ok((Record { key, value }, off + buf.len() as u64))
    }

    pub(crate) fn read_key(reader: &File, off: u64, path: &Path) -> Result<K, Error> {
        let (header, off) = Self::read_and_validate_header(reader, off, path)?;
        let mut buf = vec![0; header.key_len as usize];
        reader
            .read_exact_at(&mut buf, off)
            .map_err(Error::io(path))?;

        if crc32fast::hash(&buf) != header.key_checksum {
            return Err(err!("corrupted", path, "key checksum mismatch"));
        }
        let key: K = bitcode::deserialize(&buf)
            .map_err(|e| err!("invalid_argument", path, "deserialize key {e}"))?;
        Ok(key)
    }

    pub(crate) fn read_value(reader: &File, off: u64, path: &Path) -> Result<Value<V>, Error> {
        let (header, off) = Self::read_and_validate_header(reader, off, path)?;
        let mut buf = vec![0; header.value_len as usize];
        reader
            .read_exact_at(&mut buf, off + header.key_len as u64)
            .map_err(Error::io(path))?;

        if crc32fast::hash(&buf) != header.value_checksum {
            return Err(err!("corrupted", path, "value checksum mismatch"));
        }
        let value: Value<V> = bitcode::deserialize(&buf)
            .map_err(|e| err!("invalid_argument", path, "deserialize value {e}"))?;
        Ok(value)
    }

    fn read_and_validate_header(
        reader: &File,
        off: u64,
        path: &Path,
    ) -> Result<(DiskRecordHeader, u64), Error> {
        let mut buf = [0u8; size_of::<DiskRecordHeader>() + size_of::<u32>()];
        reader
            .read_exact_at(&mut buf, off)
            .map_err(Error::io(path))?;
        let header: [u8; size_of::<DiskRecordHeader>()] =
            buf[..size_of::<DiskRecordHeader>()].try_into().unwrap();
        let checksum: [u8; size_of::<u32>()] =
            buf[size_of::<DiskRecordHeader>()..].try_into().unwrap();

        let read_checksum = u32::from_le_bytes(checksum);
        if crc32fast::hash(&header) != read_checksum {
            return Err(err!("corrupted", path, "header checksum mismatch"));
        }

        let header: DiskRecordHeader = bytemuck::cast(header);
        if header.magic != RECORD_MAGIC_NUMBER {
            return Err(err!(
                "corrupted",
                path,
                "invalid magic number {}",
                header.magic
            ));
        }
        Ok((header, off + buf.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, path::PathBuf};

    use crate::tests::open_rw_tmp_file;

    use super::*;

    #[test]
    fn write_happy() {
        let mut buf = Vec::new();
        write(
            &String::from("abc"),
            Value::Some(&42),
            &mut buf,
            &PathBuf::from("abc"),
        )
        .unwrap();

        let want_key = bitcode::serialize("abc").unwrap();
        let want_value = bitcode::serialize(&Value::Some(42)).unwrap();
        let want_header = DiskRecordHeader {
            magic: RECORD_MAGIC_NUMBER,
            key_len: want_key.len() as u32,
            value_len: want_value.len() as u32,
            key_checksum: crc32fast::hash(&want_key),
            value_checksum: crc32fast::hash(&want_value),
        };
        let want_header_bytes = bytemuck::bytes_of(&want_header);
        let want_header_checksum_bytes = crc32fast::hash(want_header_bytes).to_le_bytes();

        assert_eq!(
            buf.len(),
            size_of::<DiskRecordHeader>() + size_of::<u32>() + want_key.len() + want_value.len()
        );

        assert_eq!(&buf[..size_of::<DiskRecordHeader>()], want_header_bytes);
        let mut off = size_of::<DiskRecordHeader>();
        assert_eq!(
            &buf[off..off + size_of::<u32>()],
            want_header_checksum_bytes
        );
        off += size_of::<u32>();
        assert_eq!(&buf[off..off + want_key.len()], want_key);
        off += want_key.len();
        assert_eq!(&buf[off..], want_value);
    }

    #[test]
    fn read_key_happy() {
        let (mut file, path) = open_rw_tmp_file();
        write(
            &String::from("abc"),
            Value::Some(&42),
            &mut file,
            &PathBuf::from("abc"),
        )
        .unwrap();

        let file = File::open(&path).unwrap();
        let key = Record::<String, i32>::read_key(&file, 0, &path).unwrap();
        assert_eq!(key, "abc");
    }

    #[test]
    fn read_value_happy() {
        let (mut file, path) = open_rw_tmp_file();
        write(
            &String::from("abc"),
            Value::Some(&42),
            &mut file,
            &PathBuf::from("abc"),
        )
        .unwrap();

        let file = File::open(&path).unwrap();
        let value = Record::<String, i32>::read_value(&file, 0, &path).unwrap();
        assert_eq!(value, Value::Some(42));
    }

    #[test]
    fn read_happy() {
        let (mut file, path) = open_rw_tmp_file();
        write(
            &String::from("abc"),
            Value::Some(&42),
            &mut file,
            &PathBuf::from("abc"),
        )
        .unwrap();
        let len = file.metadata().unwrap().len();

        let file = File::open(&path).unwrap();
        let (record, new_off) = Record::<String, i32>::read(&file, 0, &path).unwrap();
        assert_eq!(
            record,
            Record {
                key: String::from("abc"),
                value: Value::Some(42)
            }
        );
        assert_eq!(new_off, len);
    }

    #[test]
    fn read_type_mismatch() {
        let (mut file, path) = open_rw_tmp_file();
        write(
            &String::from("abc"),
            Value::Some(&42),
            &mut file,
            &PathBuf::from("abc"),
        )
        .unwrap();

        let file = File::open(&path).unwrap();
        assert_matches!(
            Record::<i64, i32>::read(&file, 0, &path),
            Err(Error::InvalidArgument { .. })
        );
    }

    #[test]
    fn read_header_checksum_mismatch() {
        let (mut file, path) = open_rw_tmp_file();
        write(
            &String::from("abc"),
            Value::Some(&42),
            &mut file,
            &PathBuf::from("abc"),
        )
        .unwrap();
        file.write_all_at(&0x13371337u32.to_le_bytes(), 0).unwrap();

        let file = File::open(&path).unwrap();
        assert_matches!(
            Record::<String, i32>::read(&file, 0, &path),
            Err(Error::Corrupted { .. })
        );
    }
}
