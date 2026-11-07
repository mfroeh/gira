use std::{
    borrow::Borrow,
    collections, fs,
    io::{self, Read, Seek, Write},
    marker, path,
};

use serde::{Serialize, de::DeserializeOwned};

use crate::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // Creation
    #[error("create file")]
    CreateFile(#[source] io::Error),
    #[error("serialize key")]
    SerializeKey(#[source] bitcode::Error),
    #[error("serialize value")]
    SerializeValue(#[source] bitcode::Error),
    #[error("write bytes")]
    WriteBytes(#[source] io::Error),
    #[error("offset overflow")]
    OffsetOverflow(),
    #[error("flush table")]
    FlushTable(#[source] io::Error),
    #[error("fsync written table")]
    SyncTable(#[source] io::Error),

    // Lookup
    #[error("open file")]
    OpenFile(#[source] io::Error),
    #[error("read offsets")]
    ReadOffsets(#[source] io::Error),
    #[error("seek tuple start")]
    SeekTupleStart(#[source] io::Error),
    #[error("read key")]
    ReadKey(#[source] io::Error),
    #[error("seek tuple value")]
    SeekTupleValue(#[source] io::Error),
    #[error("read value")]
    ReadValue(#[source] io::Error),
    #[error("deserialize key")]
    DeserializeKey(#[source] bitcode::Error),
    #[error("deserialize value")]
    DeserializeValue(#[source] bitcode::Error),
}

pub(crate) struct SSTable<K, V> {
    offsets: Vec<u32>,
    file: io::BufReader<fs::File>,
    _pd: marker::PhantomData<fn() -> (K, V)>,
}

impl<K, V> SSTable<K, V>
where
    K: DeserializeOwned,
    V: DeserializeOwned,
{
    pub(crate) fn read(from: impl AsRef<path::Path>) -> Result<SSTable<K, V>, Error> {
        let file = fs::File::open(from).map_err(Error::OpenFile)?;
        let mut file = io::BufReader::new(file);

        file.seek(io::SeekFrom::End(-4))
            .map_err(Error::ReadOffsets)?;
        let len: u32 = file.read_le().map_err(Error::ReadOffsets)?;
        file.seek(io::SeekFrom::End(
            -4 - (len * size_of::<u32>() as u32) as i64,
        ))
        .map_err(Error::ReadOffsets)?;
        let mut offset_bytes = vec![0; len as usize * size_of::<u32>()];
        file.read_exact(&mut offset_bytes)
            .map_err(Error::ReadOffsets)?;
        let offsets: Vec<u32> = offset_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes.as_array::<4>().unwrap()))
            .collect();
        Ok(SSTable {
            offsets,
            file,
            _pd: marker::PhantomData,
        })
    }

    pub(crate) fn get_key(&mut self, idx: usize) -> Result<Option<K>, Error> {
        if idx >= self.offsets.len() {
            return Ok(None);
        }

        self.file
            .seek(io::SeekFrom::Start(self.offsets[idx] as u64))
            .map_err(Error::SeekTupleStart)?;
        let key_len: u32 = self.file.read_le().map_err(Error::ReadKey)?;
        self.file
            .seek(io::SeekFrom::Start(self.offsets[idx] as u64 + 8))
            .map_err(Error::ReadKey)?;
        let mut buf = vec![0; key_len as usize];
        self.file.read_exact(&mut buf).map_err(Error::ReadKey)?;
        let key = bitcode::deserialize(&buf).map_err(Error::DeserializeKey)?;
        Ok(Some(key))
    }

    pub(crate) fn get_value(&mut self, idx: usize) -> Result<Option<Value<V>>, Error> {
        if idx >= self.offsets.len() {
            return Ok(None);
        }

        self.file
            .seek(io::SeekFrom::Start(self.offsets[idx] as u64))
            .map_err(Error::SeekTupleStart)?;
        let key_len: u32 = self.file.read_le().map_err(Error::ReadKey)?;
        let value_len: u32 = self.file.read_le().map_err(Error::ReadValue)?;
        self.file
            .seek(io::SeekFrom::Start(
                self.offsets[idx] as u64 + 8 + key_len as u64,
            ))
            .map_err(Error::SeekTupleValue)?;

        let mut buf = vec![0; value_len as usize];
        self.file.read_exact(&mut buf).map_err(Error::ReadValue)?;
        let value: Value<V> = bitcode::deserialize(&buf).map_err(Error::DeserializeValue)?;
        Ok(Some(value))
    }

    pub(crate) fn binary_search<Q>(&mut self, key: &Q) -> Result<Option<Value<V>>, Error>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (mut l, mut r) = (0, self.offsets.len() - 1);
        while l <= r {
            let mid = (l + r) / 2;
            let seen: K = self.get_key(mid)?.unwrap();
            match key.cmp(seen.borrow()) {
                std::cmp::Ordering::Less => {
                    r = mid - 1;
                }
                std::cmp::Ordering::Equal => return self.get_value(mid),
                std::cmp::Ordering::Greater => {
                    l = mid + 1;
                }
            }
        }
        Ok(None)
    }
}

pub(crate) fn write<K: Serialize, V: Serialize>(
    to: impl AsRef<path::Path>,
    map: &mut collections::BTreeMap<K, Value<V>>,
) -> Result<(), Error> {
    let file = fs::File::create_new(to).map_err(Error::CreateFile)?;
    let mut buf_writer = io::BufWriter::new(file);
    let mut offsets: Vec<u32> = Vec::with_capacity(map.len());
    let mut off = 0;
    for (k, v) in map.iter() {
        let key = bitcode::serialize(k).map_err(Error::SerializeKey)?;
        // todo: assert the len of key and value
        let value = bitcode::serialize(v).map_err(Error::SerializeValue)?;
        let mut buf = Vec::with_capacity(4 + 4 + key.len() + value.len());
        buf.extend((key.len() as u32).to_le_bytes());
        buf.extend((value.len() as u32).to_le_bytes());
        buf.extend(key);
        buf.extend(value);
        buf_writer.write_all(&buf).map_err(Error::WriteBytes)?;
        offsets.push(off);
        off = off
            .checked_add(buf.len() as u32)
            .ok_or(Error::OffsetOverflow())?;
    }
    buf_writer
        .write_all(
            &offsets
                .into_iter()
                .flat_map(|o| o.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
        .map_err(Error::WriteBytes)?;

    buf_writer
        .write_all(&(map.len() as u32).to_le_bytes())
        .map_err(Error::WriteBytes)?;
    buf_writer
        .into_inner()
        .map_err(|e| Error::FlushTable(e.into_error()))?
        .sync_all()
        .map_err(Error::SyncTable)?;
    Ok(())
}
