# gira
A durable, crash-tolerant key-value store based on a [log-structured merge-tree (LSM)](https://en.wikipedia.org/wiki/Log-structured_merge-tree).

Current scope: a learning project. Single-threaded and single-process, no range scans, and compaction always merges the two oldest tables.

[![Rust](https://github.com/mfroeh/gira/actions/workflows/rust.yml/badge.svg)](https://github.com/mfroeh/gira/actions/workflows/rust.yml)

## Usage
```rust
let mut store: Store<String, i32> = StoreBuilder::new()
    .max_in_memory_entries(4096)
    .open(&path)
    .unwrap();
// Once put/delete returns Ok, the write is durable.
store.put(String::from("1"), 1).unwrap();
store.put(String::from("2"), 2).unwrap();
store.delete(String::from("2")).unwrap();
store.get("1").unwrap();
```

## Durability and Crash Tolerance
* A `put`/`delete` that returns `Ok` is durable: it is appended to the write-ahead log (WAL), followed by `fdatasync`. The `logs` directory is fsynced whenever a new log is created, and a failed `fdatasync` poisons the log.
* On open, every WAL whose `id` is greater than the highest SSTable `id` is replayed. Until the memtable is flushed to an SSTable, the WAL keeps its writes durable.
* When the memtable exceeds the configured number of entries, it is flushed to an SSTable on disk without risking data loss:
    * Writing the SSTable fails: the table only ever exists in `sstables_tmp`, never in `sstables`. The memtable keeps its entries, and the WAL keeps them durable.
    * Opening the new SSTable fails: same as above. The table is opened (and thereby verified) before it is moved into `sstables`, and the move is followed by an `fsync` of `sstables`.
    * Opening the new log fails: the store is poisoned and must be reopened. No data is lost: the SSTable is readable and in `sstables`. On reopen, the old log is deleted because the new SSTable covers it, and the new log, if it was created, is empty.
    * Success: the SSTable is in `sstables` under the current log's `id`, and a new log with `id + 1` is open.
* Compaction also writes its merged table to `sstables_tmp`, verifies it, and then atomically moves it in place of the newest SSTable that took part in the merge. If deleting the other input tables fails, they are harmless: the merged table shadows them (and keeps tombstones), so lookups never reach them.
* Bit rot in the WAL or in SSTables is detected by checksums and returned as an error. It is detected, not repaired.
* A torn final record in the WAL is truncated. Any other corruption fails the open (fail-stop).

## Benchmarks/Performance
* In progress

## Storage
```
dir
├── lock              // flock per directory.
├── logs              // WALs for put/delete not yet persisted in an SSTable.
│   └── 4             // (see WAL Disk Format)
├── sstables          // SSTable persistence (lower = older).
│   ├── 1             // (see SSTable Disk Format)
│   ├── 2
│   └── 3
└── sstables_tmp      // SSTables being written (flushed or compacted) before the atomic move.
```

### Record Disk Format
`[Header | u32 | &[u8] | &[u8]]` where
1. Header `[u32 | u32 | u32 | u32 | u32]` where
    1. `u32` magic value
    2. `u32` the length of the key bytes
    3. `u32` the length of the value bytes
    4. `u32` the CRC32 of the key bytes
    5. `u32` the CRC32 of the value bytes
2. `u32` the CRC32 of the header bytes
3. `&[u8]` the serialized key bytes
4. `&[u8]` the serialized value bytes

### WAL Disk Format
`[Record; M]`, where `M` is unbounded in principle.

### SSTable Disk Format
`[[Record; N] | [u64; N] | u64 | u32 | u32]` where
1. `[Record; N]` the records sorted by key
2. `[u64; N]` the byte offset of each record in the file
3. `u64` the number of records `N`
4. `u32` the CRC32 of the offset bytes
5. `u32` the CRC32 of the record count
