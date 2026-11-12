use std::{
    fs::{self, File},
    iter::Peekable,
    path::Path,
};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    Error,
    record::Record,
    sstable::{self, SSTable, SSTableIter},
};

struct MergeIter<'a, 'b, K: DeserializeOwned, V: DeserializeOwned> {
    newer: Peekable<SSTableIter<'a, K, V>>,
    older: Peekable<SSTableIter<'b, K, V>>,
}

impl<K: DeserializeOwned + Ord, V: DeserializeOwned> Iterator for MergeIter<'_, '_, K, V> {
    type Item = Result<Record<K, V>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.newer.peek(), self.older.peek()) {
            (None, None) => None,
            (Some(_), None) => self.newer.next(),
            (None, Some(_)) => self.older.next(),
            (Some(new), Some(old)) => match (new.as_ref(), old.as_ref()) {
                (Err(_), _) => self.newer.next(),
                (_, Err(_)) => self.older.next(),
                (Ok(left), Ok(right)) => match left.key.cmp(&right.key) {
                    std::cmp::Ordering::Less => self.newer.next(),
                    std::cmp::Ordering::Equal => {
                        let _ = self.older.next();
                        self.newer.next()
                    }
                    std::cmp::Ordering::Greater => self.older.next(),
                },
            },
        }
    }
}

pub(crate) fn merge<K, V>(
    newer: &SSTable<K, V>,
    older: &SSTable<K, V>,
    tmp_path: &Path,
) -> Result<(), Error>
where
    K: DeserializeOwned + Serialize + Ord,
    V: DeserializeOwned + Serialize,
{
    let iter = MergeIter {
        newer: newer.into_iter().peekable(),
        older: older.into_iter().peekable(),
    };
    let tuples = iter.map(|res| res.map(|res| (res.key, res.value)));

    let file = File::create(&tmp_path).map_err(Error::io(&tmp_path))?;
    sstable::write(file, &tmp_path, tuples).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        e
    })?;

    fs::rename(&tmp_path, &newer.path).map_err(Error::io(tmp_path))
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use map_macro::btree_map;

    use crate::{
        Value,
        tests::{make_sstable, unique_tmp_path},
    };

    use super::*;

    #[test]
    // TODO: good candidate for prop test
    fn merge_iter_happy() {
        let new = btree_map! {
            // New only
             1 => Value::Some(1),
             // New replaced value
             2 => Value::Some(22),
             // New removed
             3 => Value::Tombstone,
             // New put value
             5 => Value::Some(5),
             // New only
             99 => Value::Some(99),
             // Ensure new longer
             100 => Value::Tombstone,
        };
        let old = btree_map! {
            // Old only
             -1 => Value::Some(-1),
             // Value replaced by new
             2 => Value::Some(2),
             // Value removed by new
             3 => Value::Some(3),
             // Old only
             4 => Value::Some(4),
             // Value put by new
             5 => Value::Tombstone,
        };
        let expected = [
            Record {
                key: -1,
                value: Value::Some(-1),
            },
            Record {
                key: 1,
                value: Value::Some(1),
            },
            Record {
                key: 2,
                value: Value::Some(22),
            },
            Record {
                key: 3,
                value: Value::Tombstone,
            },
            Record {
                key: 4,
                value: Value::Some(4),
            },
            Record {
                key: 5,
                value: Value::Some(5),
            },
            Record {
                key: 99,
                value: Value::Some(99),
            },
            Record {
                key: 100,
                value: Value::Tombstone,
            },
        ];
        let (new, _) = make_sstable(&new);
        let (old, _) = make_sstable(&old);

        let iter = MergeIter {
            newer: new.into_iter().peekable(),
            older: old.into_iter().peekable(),
        };
        assert!(iter.map(Result::unwrap).eq(expected));
    }

    #[test]
    fn merge_happy() {
        let new = btree_map! {
            // New only
             1 => Value::Some(1),
        };
        let old = btree_map! {
            // Old only
             -1 => Value::Some(-1),
        };
        let (new, new_path) = make_sstable(&new);
        let (old, _) = make_sstable(&old);
        merge(&new, &old, &unique_tmp_path()).unwrap();

        let table: SSTable<i32, i32> = SSTable::open(new_path, new.idx).unwrap();
        assert_eq!(table.len(), 2);
        assert_matches!(table.get_value(0), Ok(Some(Value::Some(-1))));
        assert_matches!(table.get_value(1), Ok(Some(Value::Some(1))));
    }
}
