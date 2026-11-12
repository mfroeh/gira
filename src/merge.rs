use std::iter::Peekable;

use serde::de::DeserializeOwned;

use crate::{
    Error,
    record::Record,
    sstable::{SSTable, SSTableIter},
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
) -> impl Iterator<Item = Result<Record<K, V>, Error>>
where
    K: DeserializeOwned + Ord,
    V: DeserializeOwned,
{
    MergeIter {
        newer: newer.into_iter().peekable(),
        older: older.into_iter().peekable(),
    }
}

#[cfg(test)]
mod tests {
    use map_macro::btree_map;

    use crate::{Value, tests::make_sstable};

    use super::*;

    #[test]
    // TODO: good candidate for prop test
    fn merge_happy() {
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
        let new = make_sstable(&new);
        let old = make_sstable(&old);

        assert!(merge(&new, &old).map(Result::unwrap).eq(expected));
    }
}
