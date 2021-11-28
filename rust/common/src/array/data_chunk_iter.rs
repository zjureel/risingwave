use serde::{Deserialize, Serialize};
use std::hash::Hash;
use std::ops;

use crate::array::DataChunk;
use crate::types::{DataTypeKind, Datum, DatumRef, ScalarImpl, ToOwnedDatum};

impl DataChunk {
    pub fn rows(&self) -> DataChunkRefIter<'_> {
        DataChunkRefIter {
            chunk: self,
            idx: 0,
        }
    }
}

pub struct DataChunkRefIter<'a> {
    chunk: &'a DataChunk,
    idx: usize,
}

/// Data Chunk iter only iterate visible tuples.
impl<'a> Iterator for DataChunkRefIter<'a> {
    type Item = RowRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.idx >= self.chunk.capacity() {
                return None;
            }
            let (cur_val, vis) = self.chunk.row_at(self.idx).ok()?;
            self.idx += 1;
            if vis {
                return Some(cur_val);
            }
        }
    }
}

impl<'a> DataChunkRefIter<'a> {
    pub fn new(chunk: &'a DataChunk) -> Self {
        Self { chunk, idx: 0 }
    }
}

/// TODO: Consider merge with Row in storage. It is end with Ref because it do not own data
/// and avoid conflict with [`Row`].
#[derive(Debug, PartialEq)]
pub struct RowRef<'a>(pub Vec<DatumRef<'a>>);

impl<'a> RowRef<'a> {
    pub fn new(values: Vec<DatumRef<'a>>) -> Self {
        Self(values)
    }

    pub fn value_at(&self, pos: usize) -> DatumRef<'a> {
        self.0[pos]
    }

    pub fn size(&self) -> usize {
        self.0.len()
    }
}

impl<'a> ops::Index<usize> for RowRef<'a> {
    type Output = DatumRef<'a>;
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Row(pub Vec<Datum>);

impl Row {
    pub fn size(&self) -> usize {
        self.0.len()
    }
}

impl ops::Index<usize> for Row {
    type Output = Datum;
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl From<RowRef<'_>> for Row {
    fn from(row_ref: RowRef<'_>) -> Self {
        Row(row_ref
            .0
            .into_iter()
            .map(ToOwnedDatum::to_owned_datum)
            .collect::<Vec<_>>())
    }
}

impl PartialOrd for Row {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if self.0.len() != other.0.len() {
            return None;
        }
        self.0.partial_cmp(&other.0)
    }
}

impl Ord for Row {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl Row {
    /// Serialize the row into a memcomparable bytes.
    ///
    /// All values are nullable. Each value will have 1 extra byte to indicate whether it is null.
    pub fn serialize(&self) -> Result<Vec<u8>, memcomparable::Error> {
        let mut serializer = memcomparable::Serializer::default();
        for v in self.0.iter() {
            if let Some(v) = v {
                1u8.serialize(&mut serializer)?;
                v.serialize(&mut serializer)?;
            } else {
                0u8.serialize(&mut serializer)?;
            }
        }
        Ok(serializer.into_inner())
    }

    /// Serialize the row into a memcomparable bytes. All values must not be null.
    pub fn serialize_not_null(&self) -> Result<Vec<u8>, memcomparable::Error> {
        let mut serializer = memcomparable::Serializer::default();
        for v in self.0.iter() {
            v.as_ref()
                .expect("value can not be null")
                .serialize(&mut serializer)?;
        }
        Ok(serializer.into_inner())
    }
}

/// Deserializer of the `Row`.
pub struct RowDeserializer {
    schema: Vec<DataTypeKind>,
}

impl RowDeserializer {
    /// Creates a new `RowDeserializer` with row schema.
    pub fn new(schema: Vec<DataTypeKind>) -> Self {
        RowDeserializer { schema }
    }

    /// Deserialize the row from a memcomparable bytes.
    pub fn deserialize(&self, data: &[u8]) -> Result<Row, memcomparable::Error> {
        let mut values = vec![];
        values.reserve(self.schema.len());
        let mut deserializer = memcomparable::Deserializer::from_slice(data);
        for &ty in self.schema.iter() {
            match u8::deserialize(&mut deserializer)? {
                0 => values.push(None),
                1 => {
                    let scalar = ScalarImpl::deserialize(ty, &mut deserializer)?;
                    values.push(Some(scalar));
                }
                t => return Err(memcomparable::Error::InvalidTagEncoding(t as _)),
            }
        }
        Ok(Row(values))
    }

    /// Deserialize the row from a memcomparable bytes. All values are not null.
    pub fn deserialize_not_null(&self, data: &[u8]) -> Result<Row, memcomparable::Error> {
        let mut values = vec![];
        values.reserve(self.schema.len());
        let mut deserializer = memcomparable::Deserializer::from_slice(data);
        for &ty in self.schema.iter() {
            let scalar = ScalarImpl::deserialize(ty, &mut deserializer)?;
            values.push(Some(scalar));
        }
        Ok(Row(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataTypeKind as Ty, IntervalUnit};

    #[test]
    fn row_memcomparable_encode_decode_not_null() {
        let row = Row(vec![
            Some(ScalarImpl::Utf8("string".into())),
            Some(ScalarImpl::Bool(true)),
            Some(ScalarImpl::Int16(1)),
            Some(ScalarImpl::Int32(2)),
            Some(ScalarImpl::Int64(3)),
            Some(ScalarImpl::Float32(4.)),
            Some(ScalarImpl::Float64(5.)),
            Some(ScalarImpl::Decimal("-233.3".parse().unwrap())),
            Some(ScalarImpl::Interval(IntervalUnit::new(7, 8, 9))),
        ]);
        let bytes = row.serialize_not_null().unwrap();
        assert_eq!(bytes.len(), 9 + 1 + 2 + 4 + 8 + 4 + 8 + 13 + 16);

        let de = RowDeserializer::new(vec![
            Ty::Varchar,
            Ty::Boolean,
            Ty::Int16,
            Ty::Int32,
            Ty::Int64,
            Ty::Float32,
            Ty::Float64,
            Ty::Decimal,
            Ty::Interval,
        ]);
        let row1 = de.deserialize_not_null(&bytes).unwrap();
        assert_eq!(row, row1);
    }

    #[test]
    fn row_memcomparable_encode_decode() {
        let row = Row(vec![
            Some(ScalarImpl::Utf8("string".into())),
            Some(ScalarImpl::Bool(true)),
            Some(ScalarImpl::Int16(1)),
            Some(ScalarImpl::Int32(2)),
            Some(ScalarImpl::Int64(3)),
            Some(ScalarImpl::Float32(4.)),
            Some(ScalarImpl::Float64(5.)),
            Some(ScalarImpl::Decimal("-233.3".parse().unwrap())),
            Some(ScalarImpl::Interval(IntervalUnit::new(7, 8, 9))),
        ]);
        let bytes = row.serialize().unwrap();
        assert_eq!(bytes.len(), 9 + 1 + 2 + 4 + 8 + 4 + 8 + 13 + 16 + 9);

        let de = RowDeserializer::new(vec![
            Ty::Varchar,
            Ty::Boolean,
            Ty::Int16,
            Ty::Int32,
            Ty::Int64,
            Ty::Float32,
            Ty::Float64,
            Ty::Decimal,
            Ty::Interval,
        ]);
        let row1 = de.deserialize(&bytes).unwrap();
        assert_eq!(row, row1);
    }
}
