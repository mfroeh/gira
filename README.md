
# SSTable Disk Format
* `[[u32 | u32 | &[u8] | &[u8]]; N]` where
  1. `u32` the length of the key bytes
  2. `u32` the length of the value bytes
  3. `&[u8]` the serialized key bytes
  4. `&[u8]` the serialized value bytes
* `[u32; N]` the file byte offsets of each of the tuples
* `u32` the number of tuples (`N`) in the file
