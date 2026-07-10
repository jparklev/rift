use rusqlite::{
    Error as SqlError,
    types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef},
};
use std::fmt;
use ulid::Ulid;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RiftId {
    text: String,
    // Markers, hooks, locks, and public responses continue to use canonical
    // text. The registry stores this compact binary representation instead.
    bytes: Option<[u8; 16]>,
}

impl RiftId {
    pub(crate) fn new() -> Self {
        Self::from_ulid(Ulid::new())
    }

    pub(crate) fn from_stored(value: String) -> Self {
        let bytes = Ulid::from_string(&value)
            .ok()
            // A marker with a different spelling (for example, lowercase)
            // remains an unknown marker, as it was when the registry used
            // exact TEXT equality.
            .filter(|id| id.to_string() == value)
            .map(|id| id.to_bytes());
        Self { text: value, bytes }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    fn from_ulid(id: Ulid) -> Self {
        Self {
            text: id.to_string(),
            bytes: Some(id.to_bytes()),
        }
    }

    fn from_database_bytes(bytes: [u8; 16]) -> Self {
        Self::from_ulid(Ulid::from_bytes(bytes))
    }

    /// Returns the fixed-width representation used by the SQLite registry.
    /// User-controlled marker text is deliberately not accepted here: an
    /// invalid marker must not be coerced into a different registry entry.
    pub(crate) fn database_bytes(&self) -> rusqlite::Result<[u8; 16]> {
        self.bytes.ok_or_else(|| {
            SqlError::ToSqlConversionFailure(Box::new(InvalidDatabaseId(self.text.clone())))
        })
    }
}

impl fmt::Display for RiftId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.text.fmt(formatter)
    }
}

impl ToSql for RiftId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(Value::Blob(
            self.database_bytes()?.to_vec(),
        )))
    }
}

impl FromSql for RiftId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let ValueRef::Blob(bytes) = value else {
            return Err(FromSqlError::InvalidType);
        };
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| FromSqlError::InvalidBlobSize {
                expected_size: 16,
                blob_size: bytes.len(),
            })?;
        Ok(Self::from_database_bytes(bytes))
    }
}

#[derive(Debug)]
struct InvalidDatabaseId(String);

impl fmt::Display for InvalidDatabaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rift registry ID is not a canonical ULID: {}",
            self.0
        )
    }
}

impl std::error::Error for InvalidDatabaseId {}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    #[test]
    fn stores_canonical_ulids_as_fixed_width_blobs() {
        let id = RiftId::from_stored("01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
        let database = Connection::open_in_memory().unwrap();
        database
            .execute("CREATE TABLE ids (id BLOB NOT NULL)", [])
            .unwrap();
        database
            .execute("INSERT INTO ids (id) VALUES (?1)", params![id])
            .unwrap();

        let (kind, length, decoded): (String, i64, RiftId) = database
            .query_row("SELECT typeof(id), length(id), id FROM ids", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(kind, "blob");
        assert_eq!(length, 16);
        assert_eq!(decoded, id);
    }

    #[test]
    fn rejects_noncanonical_marker_text_for_database_lookup() {
        let id = RiftId::from_stored("not-a-ulid".into());
        assert!(id.database_bytes().is_err());
    }
}
