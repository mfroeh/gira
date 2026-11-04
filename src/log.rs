use std::{
    borrow, collections, env, fs, hash,
    io::{self, Read, Write},
    path, range,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("log is poisoned")]
    Poisoned,
    #[error("sync failed")]
    SyncFailed(#[source] io::Error),

    #[error("get current working directory")]
    GetCwd(#[source] io::Error),
    #[error("create new log")]
    CreateLog(#[source] io::Error),
    #[error("open existing log")]
    OpenLog(#[source] io::Error),
    #[error("open parent")]
    OpenParent(#[source] io::Error),

    #[error("read")]
    Read(#[from] ReadError),
    #[error("write")]
    Write(#[from] WriteError),
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("get file metadata")]
    FileMetadata(#[source] io::Error),
    #[error("read entry from log")]
    ReadEntry(#[source] io::Error),
    #[error("invalid magic number: {0}")]
    InvalidMagicNumber(u32),
    #[error("invalid entry length: got: {got}, valid: {valid:?}")]
    InvalidLength {
        got: u32,
        valid: range::RangeInclusive<usize>,
    },
    #[error("checksum mismatch: {length_checksum} (length) != {read_checksum} (read)")]
    LengthChecksumMismatch {
        length_checksum: u32,
        read_checksum: u32,
    },
    #[error("checksum mismatch: {payload_checksum} (payload) != {read_checksum} (read)")]
    PayloadChecksumMismatch {
        payload_checksum: u32,
        read_checksum: u32,
    },
    #[error("missing LF")]
    MissingLineFeed,
    #[error("deserialize entry from log")]
    DeserializeEntry(#[source] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("serialize log entry")]
    SerializeEntry(#[source] serde_json::Error),
    #[error("invalid entry length: got: {got}, valid: {valid:?}")]
    InvalidLength {
        got: u32,
        valid: range::RangeInclusive<usize>,
    },
    #[error("write entry to log")]
    WriteEntry(#[source] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Entry<K, V> {
    Insert(K, V),
    Remove(K),
}

const RECORD_MAGIC_NUMBER: u32 = 0xDEADBEEF;
const ENTRY_MAX_SIZE: usize = 1 << 16;
const _RECORD_MAX_SIZE: usize =
    size_of::<RecordHeader>() + ENTRY_MAX_SIZE + size_of::<RecordTail>() + 1;

#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct RecordHeader {
    magic: u32,
    payload_length: u32,
    length_checksum: u32,
}

#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct RecordTail {
    payload_checksum: u32,
}

impl<K: Serialize, V: Serialize> Entry<&K, &V> {
    fn write(&self, mut w: impl Write) -> Result<(), WriteError> {
        let payload = serde_json::to_string(self).map_err(WriteError::SerializeEntry)?;
        let json_len = payload.len();
        (1..=ENTRY_MAX_SIZE)
            .contains(&json_len)
            .ok_or(WriteError::InvalidLength {
                got: json_len as u32,
                valid: range::RangeInclusive::from(1..=ENTRY_MAX_SIZE),
            })?;
        let payload_checksum = crc32fast::hash(payload.as_bytes());
        let tail = RecordTail { payload_checksum };
        let payload_length = payload.len() as u32;
        let length_checksum = crc32fast::hash(&payload_length.to_le_bytes());
        let header = RecordHeader {
            magic: RECORD_MAGIC_NUMBER,
            payload_length,
            length_checksum,
        };
        let mut buf = Vec::with_capacity(
            size_of::<RecordHeader>() + payload.len() + size_of::<RecordTail>() + 1,
        );
        buf.extend_from_slice(bytemuck::bytes_of(&header));
        buf.extend_from_slice(payload.as_bytes());
        buf.extend_from_slice(bytemuck::bytes_of(&tail));
        buf.push(b'\n');
        w.write_all(&buf).map_err(WriteError::WriteEntry)
    }
}

impl<K: DeserializeOwned, V: DeserializeOwned> Entry<K, V> {
    fn read(mut w: impl Read) -> Result<(Self, usize), ReadError> {
        let raw_header: [u8; size_of::<RecordHeader>()] =
            w.read_array().map_err(ReadError::ReadEntry)?;
        let header: RecordHeader = bytemuck::cast(raw_header);

        if header.magic != RECORD_MAGIC_NUMBER {
            return Err(ReadError::InvalidMagicNumber(header.magic));
        }

        let got_checksum = crc32fast::hash(&header.payload_length.to_le_bytes());
        if got_checksum != header.length_checksum {
            return Err(ReadError::LengthChecksumMismatch {
                length_checksum: got_checksum,
                read_checksum: header.length_checksum,
            });
        }

        let json_len = header.payload_length as usize;
        (1..=ENTRY_MAX_SIZE)
            .contains(&json_len)
            .ok_or(ReadError::InvalidLength {
                got: json_len as u32,
                valid: range::RangeInclusive::from(1..=ENTRY_MAX_SIZE),
            })?;

        let buf_len = header.payload_length as usize + size_of::<RecordTail>() + 1;
        let mut buf = vec![0; buf_len];
        w.read_exact(&mut buf).map_err(ReadError::ReadEntry)?;

        if buf.pop().unwrap() as char != '\n' {
            return Err(ReadError::MissingLineFeed);
        }
        let (payload, tail) = buf.split_at(header.payload_length as usize);
        let tail: RecordTail = bytemuck::cast(*tail.as_array::<4>().unwrap());

        let got_checksum = crc32fast::hash(payload);
        if got_checksum != tail.payload_checksum {
            return Err(ReadError::PayloadChecksumMismatch {
                payload_checksum: got_checksum,
                read_checksum: tail.payload_checksum,
            });
        }

        let entry: Self = serde_json::from_slice(payload).map_err(ReadError::DeserializeEntry)?;
        Ok((entry, size_of::<RecordHeader>() + buf_len))
    }
}

impl<K, V> Entry<K, V> {
    #[cfg(test)]
    fn as_ref(&self) -> Entry<&K, &V> {
        match self {
            Entry::Insert(k, v) => Entry::Insert(k, v),
            Entry::Remove(k) => Entry::Remove(k),
        }
    }
}

pub(crate) fn replay_existing_log<K, V>(
    path: impl AsRef<path::Path>,
) -> Result<collections::HashMap<K, V>, Error>
where
    K: DeserializeOwned + Eq + hash::Hash,
    V: DeserializeOwned,
{
    let path = get_absolute(path.as_ref())?;
    match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => replay(file).map_err(Error::from),
        Err(err) => match err.kind() {
            io::ErrorKind::NotFound => Ok(collections::HashMap::new()),
            _ => Err(Error::OpenLog(err)),
        },
    }
}

fn replay<K, V>(mut file: fs::File) -> Result<collections::HashMap<K, V>, ReadError>
where
    K: DeserializeOwned + Eq + hash::Hash,
    V: DeserializeOwned,
{
    let mut store = collections::HashMap::new();
    let len = file.metadata().map_err(ReadError::FileMetadata)?.len();
    let mut off = 0;
    while off < len {
        match Entry::<K, V>::read(&mut file) {
            Ok((entry, read)) => {
                off += read as u64;
                match entry {
                    Entry::Insert(k, v) => store.insert(k, v),
                    Entry::Remove(k) => store.remove(&k),
                };
            }
            Err(err) => {
                let truncate = |file: &mut fs::File| {
                    file.set_len(off).expect("truncate log");
                    file.sync_all().expect("sync log");
                };

                use ReadError::*;
                match &err {
                    ReadEntry(io_err) => {
                        if io_err.kind() == io::ErrorKind::UnexpectedEof {
                            truncate(&mut file);
                            return Ok(store);
                        }
                    }
                    // Torn header write: always fail.
                    InvalidMagicNumber(_)
                    | LengthChecksumMismatch { .. }
                    | InvalidLength { .. } => {}
                    // Truncate only if torn/corrupted entry is last entry, otherwise fail.
                    PayloadChecksumMismatch { .. } | MissingLineFeed => {
                        let is_last_entry = file
                            .read_exact(&mut [0])
                            .is_err_and(|e| e.kind() == io::ErrorKind::UnexpectedEof);
                        if is_last_entry {
                            truncate(&mut file);
                            return Ok(store);
                        }
                    }
                    DeserializeEntry(_) => {}
                    FileMetadata(_) => unreachable!(),
                };
                return Err(err);
            }
        };
    }
    Ok(store)
}

#[derive(Debug)]
pub(crate) struct Log {
    poisoned: bool,
    _path: path::PathBuf,
    file: fs::File,
}

impl Log {
    pub(crate) fn open(path: impl AsRef<path::Path>) -> Result<Log, Error> {
        let path = get_absolute(path.as_ref())?;
        let file = if !fs::exists(&path).map_err(Error::OpenLog)? {
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(Error::CreateLog)?;
            // Ensure the file creation is flushed to disk.
            fs::File::open(path.parent().expect("no parent"))
                .map_err(Error::OpenParent)?
                .sync_all()
                .map_err(Error::SyncFailed)?;
            file
        } else {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(Error::OpenLog)?
        };

        Ok(Log {
            poisoned: false,
            file,
            _path: path.to_path_buf(),
        })
    }

    pub(crate) fn append<K: Serialize, V: Serialize>(
        &mut self,
        entry: Entry<&K, &V>,
    ) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        entry.write(&mut self.file).map_err(|e| {
            use WriteError::*;
            match e {
                SerializeEntry(_) | InvalidLength { .. } => e,
                WriteEntry(_) => {
                    self.poisoned = true;
                    e
                }
            }
        })?;
        self.file.sync_data().map_err(|e| {
            self.poisoned = true;
            Error::SyncFailed(e)
        })?;
        Ok(())
    }
}

fn get_absolute(path: &path::Path) -> Result<borrow::Cow<'_, path::Path>, Error> {
    if !path.is_absolute() {
        let mut absolute_path = env::current_dir().map_err(Error::GetCwd)?;
        absolute_path.push(path);
        Ok(borrow::Cow::Owned(absolute_path))
    } else {
        Ok(borrow::Cow::Borrowed(path))
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs::File, hash_map, io::Seek};

    use crate::tests::open_rw_tmp_file;

    use super::*;

    mod entry {
        use super::*;

        #[test]
        fn write() {
            let mut buf = Vec::new();
            let entry = Entry::Insert("abc", 42);
            entry.as_ref().write(&mut buf).unwrap();
            let json = serde_json::to_string(&entry).unwrap();
            assert_eq!(buf.len(), 4 + 4 + 4 + json.len() + 4 + 1);

            assert_eq!(
                u32::from_le_bytes(*buf[..4].as_array().unwrap()),
                RECORD_MAGIC_NUMBER
            );
            let want_json_length = (json.len() as u32).to_le_bytes();
            assert_eq!(&buf[4..8], &want_json_length);
            assert_eq!(
                &buf[8..12],
                crc32fast::hash(&want_json_length).to_le_bytes()
            );
            let got_payload_checksum =
                u32::from_le_bytes(*buf[buf.len() - 5..buf.len() - 1].as_array().unwrap());
            assert_eq!(crc32fast::hash(json.as_bytes()), got_payload_checksum);
            assert_eq!(buf[buf.len() - 1] as char, '\n');
        }

        #[test]
        fn read_after_write() {
            let mut buf = Vec::new();
            let entry = Entry::Insert("abc", 42);
            entry.as_ref().write(&mut buf).unwrap();

            let (entry, read) = Entry::<String, i32>::read(buf.as_slice()).unwrap();
            assert_eq!(entry, Entry::Insert(String::from("abc"), 42));
            assert_eq!(read, buf.len());
        }
    }

    #[test]
    fn log_append_happy() {
        let (file, path) = open_rw_tmp_file();
        let mut log = Log::open(&path).unwrap();
        log.append(Entry::Insert("abc", 42).as_ref()).unwrap();
        log.append(Entry::Insert("123", 123).as_ref()).unwrap();

        let res = replay::<String, i32>(file).unwrap();
        assert_eq!(
            res,
            hash_map! { String::from("abc") => 42, String::from("123") => 123 },
            "{path:?}",
        );
    }

    #[test]
    fn log_poison_on_append() {
        let path = "/dev/full";
        let mut log = Log::open(path).unwrap();
        assert_matches!(
            log.append(Entry::Insert("abc", 42).as_ref()),
            Err(Error::Write(WriteError::WriteEntry(..)))
        );

        assert_matches!(
            log.append(Entry::Insert("abc", 42).as_ref()),
            Err(Error::Poisoned)
        );
    }

    #[test]
    fn replay_happy() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        entry2.as_ref().write(&mut file).unwrap();

        let file = File::open(path).unwrap();
        let store = replay(file).unwrap();
        assert_eq!(
            store,
            hash_map! { String::from("abc") => 42, String::from("123") => 123 }
        );
    }

    #[test]
    fn replay_wrong_type() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        entry2.as_ref().write(&mut file).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<i32, i32>(file);
        assert_matches!(res, Err(ReadError::DeserializeEntry { .. }), "{path:?}",);
    }

    #[test]
    fn replay_first_invalid_magic() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(0)).unwrap();
        file.write_all(&0x13371337u32.to_le_bytes()).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(
            res,
            Err(ReadError::InvalidMagicNumber(0x13371337)),
            "{path:?}",
        );
    }

    #[test]
    fn replay_first_invalid_length() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(4)).unwrap();
        let botched_len = (ENTRY_MAX_SIZE as u32 + 1).to_le_bytes();
        file.write_all(&[botched_len, crc32fast::hash(&botched_len).to_le_bytes()].concat())
            .unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(res, Err(ReadError::InvalidLength { .. }), "{path:?}",);
    }

    #[test]
    fn replay_first_length_checksum_mismatch() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(8)).unwrap();
        file.write_all(&0x13371337u32.to_le_bytes()).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(
            res,
            Err(ReadError::LengthChecksumMismatch { .. }),
            "{path:?}",
        );
    }

    #[test]
    fn replay_first_payload_read_checksum_corrupted() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        let start_record_two = file.stream_position().unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(start_record_two - 5))
            .unwrap();
        file.write_all(&0x13371337u32.to_le_bytes()).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(
            res,
            Err(ReadError::PayloadChecksumMismatch {
                read_checksum: 0x13371337,
                ..
            }),
            "{path:?}",
        );
    }

    #[test]
    fn replay_first_payload_corrupted() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(size_of::<RecordHeader>() as u64))
            .unwrap();
        file.write_all(&0x13371337u32.to_le_bytes()).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(
            res,
            Err(ReadError::PayloadChecksumMismatch { .. }),
            "{path:?}",
        );
    }

    #[test]
    fn replay_first_missing_linefeed() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        let start_record_two = file.stream_position().unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(start_record_two - 1))
            .unwrap();
        file.write_all(&0x37u8.to_le_bytes()).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(res, Err(ReadError::MissingLineFeed), "{path:?}",);
    }

    #[test]
    fn replay_zero_filled_tail() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        entry.as_ref().write(&mut file).unwrap();
        file.write_all(&[0; 1337]).unwrap();

        let file = File::open(&path).unwrap();
        let res = replay::<String, i32>(file);
        assert_matches!(res, Err(ReadError::InvalidMagicNumber { .. }), "{path:?}",);
    }

    #[test]
    fn replay_last_checksum_mismatch() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        let start_record_two = file.stream_position().unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::Start(
            start_record_two + size_of::<RecordHeader>() as u64,
        ))
        .unwrap();
        file.write_all(&0x13371337u32.to_le_bytes()).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let res = replay::<String, i32>(file).unwrap();
        assert_eq!(res, hash_map! { String::from("abc") => 42 }, "{path:?}",);

        // Check file truncated
        let file = File::open(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), start_record_two);
    }

    #[test]
    fn replay_last_missing_linefeed() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        let start_record_two = file.stream_position().unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        file.seek(io::SeekFrom::End(-1)).unwrap();
        file.write_all(&0x37u8.to_le_bytes()).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let res = replay::<String, i32>(file).unwrap();
        assert_eq!(res, hash_map! { String::from("abc") => 42 }, "{path:?}",);

        // Check file truncated
        let file = File::open(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), start_record_two);
    }

    #[test]
    fn replay_last_unexpected_eof() {
        let (mut file, path) = open_rw_tmp_file();
        let entry = Entry::Insert("abc", 42);
        let entry2 = Entry::Insert("123", 123);
        entry.as_ref().write(&mut file).unwrap();
        let start_record_two = file.stream_position().unwrap();
        entry2.as_ref().write(&mut file).unwrap();
        let record_two_len = file.stream_position().unwrap() - start_record_two;

        for i in 1..record_two_len {
            // Simulate incomplete write.
            file.set_len(start_record_two + i).unwrap();
            assert_eq!(file.metadata().unwrap().len(), start_record_two + i);

            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();

            let res = replay::<String, i32>(file).unwrap();
            assert_eq!(res, hash_map! { String::from("abc") => 42 }, "{path:?}",);

            // Check file truncated
            let file = File::open(&path).unwrap();
            assert_eq!(file.metadata().unwrap().len(), start_record_two);

            // Appending still works as expected
            let mut log = Log::open(&path).unwrap();
            log.append(entry2.as_ref()).unwrap();
            let res = replay::<String, i32>(file).unwrap();
            assert_eq!(
                res,
                hash_map! { String::from("abc") => 42, String::from("123") => 123 },
                "{path:?}",
            );
        }
    }
}
