### Record Disk Format
`[Header | &[u8] | &[u8]]` where
1. Header `[u32 | u32 | u32 | u32 | u32]` where
  1. `u32` magic value
  2. `u32` the length of the key bytes
  3. `u32` the length of the value bytes
  4. `u32` the CRC32 of the key bytes
  5. `u32` the CRC32 of the value bytes
2. `&[u8]` the serialized key bytes
3. `&[u8]` the serialized value bytes

### WAL Disk Format
`[Record; M]` where M is unbounded in principle

### SSTable Disk Format
`[[Record; N] | [u64; N] | u32 | u32]` where
1. `[Record; N]` where `N = MAX_TABLE_ENTRIES + 1`
2. `[u64; N]` the file byte offsets of each of the tuples
3. `u32` the CRC32 of the offset bytes
4. `u32` the CRC32 of the length bytes
