//! Transactional string/binary key-value facade implemented on SurrealDB.
//!
//! This deliberately mirrors the narrow table operations used by Facial's
//! existing metadata/inventory code while consolidating persistence on SurrealDB.
//! Values are persisted in a single SurrealDB table; binary cache values use
//! deterministic hexadecimal encoding and therefore cannot contain raw media
//! blobs by accident.

use crate::surreal_store;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLockReadGuard, RwLockWriteGuard};
use surrealdb::types::SurrealValue;

const TABLE: &str = "facial_kv";
const SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone)]
pub struct KvError(String);

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KvError {}

#[derive(Clone)]
pub struct Database {
    store: Arc<surreal_store::Store>,
}

#[derive(Clone)]
pub struct ReadOnlyDatabase(Database);

pub trait ReadableDatabase {
    fn begin_read(&self) -> Result<ReadTransaction<'_>, KvError>;
}

impl Database {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, KvError> {
        let store = surreal_store::open(path.as_ref()).map_err(KvError)?;
        let database = Self { store };
        database.ensure_schema()?;
        Ok(database)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, KvError> {
        Self::create(path)
    }

    fn ensure_schema(&self) -> Result<(), KvError> {
        let _schema_guard = self
            .store
            .transaction_lock()
            .write()
            .map_err(|_| KvError("SurrealDB schema lock is poisoned".to_string()))?;
        let db = self.store.db();
        surreal_store::run(async move {
            db.query("DEFINE TABLE IF NOT EXISTS facial_schema SCHEMALESS;")
                .await
                .map_err(|error| format!("bootstrap Facial key-value schema registry: {error}"))?
                .check()
                .map_err(|error| format!("bootstrap Facial key-value schema registry: {error}"))?;
            let mut existing = db
                .query("SELECT component, schema_version, engine_version FROM facial_schema:kv;")
                .await
                .map_err(|error| format!("read Facial key-value schema registry: {error}"))?;
            let meta: Option<serde_json::Value> = existing
                .take(0)
                .map_err(|error| format!("decode Facial key-value schema registry: {error}"))?;
            if let Some(meta) = meta {
                if meta.get("component").and_then(serde_json::Value::as_str) != Some("kv")
                    || meta
                        .get("schema_version")
                        .and_then(serde_json::Value::as_u64)
                        != Some(SCHEMA_VERSION)
                    || meta
                        .get("engine_version")
                        .and_then(serde_json::Value::as_str)
                        != Some(surreal_store::ENGINE_VERSION)
                {
                    return Err("Facial key-value schema registry is incompatible".to_string());
                }
            }
            db.query(
                "DEFINE TABLE OVERWRITE facial_schema SCHEMAFULL;\n\
                 DEFINE FIELD OVERWRITE component ON TABLE facial_schema TYPE string;\n\
                 DEFINE FIELD OVERWRITE schema_version ON TABLE facial_schema TYPE int;\n\
                 DEFINE FIELD OVERWRITE engine_version ON TABLE facial_schema TYPE string;\n\
                 DEFINE TABLE OVERWRITE facial_kv SCHEMAFULL;\n\
                 DEFINE FIELD OVERWRITE bucket ON TABLE facial_kv TYPE string;\n\
                 DEFINE FIELD OVERWRITE key ON TABLE facial_kv TYPE string;\n\
                 DEFINE FIELD OVERWRITE value ON TABLE facial_kv TYPE string;\n\
                 DEFINE INDEX OVERWRITE facial_kv_bucket_key ON TABLE facial_kv COLUMNS bucket, key UNIQUE;\n\
                 UPSERT facial_schema:kv SET component = 'kv', schema_version = $schema_version, engine_version = $engine_version;",
            )
                .bind(("schema_version", SCHEMA_VERSION))
                .bind(("engine_version", surreal_store::ENGINE_VERSION))
                .await
                .map_err(|error| format!("define Facial key-value schema: {error}"))?
                .check()
                .map_err(|error| format!("define Facial key-value schema: {error}"))?;
            Ok(())
        })
        .map_err(KvError)
    }

    pub fn begin_write(&self) -> Result<WriteTransaction<'_>, KvError> {
        let write_guard = self
            .store
            .transaction_lock()
            .write()
            .map_err(|_| KvError("SurrealDB writer lock is poisoned".to_string()))?;
        Ok(WriteTransaction {
            store: Arc::clone(&self.store),
            operations: Arc::new(Mutex::new(Vec::new())),
            _write_guard: write_guard,
        })
    }

    pub fn session_id(&self) -> &str {
        self.store.session_id()
    }
}

impl ReadableDatabase for Database {
    fn begin_read(&self) -> Result<ReadTransaction<'_>, KvError> {
        let read_guard = self
            .store
            .transaction_lock()
            .read()
            .map_err(|_| KvError("SurrealDB reader lock is poisoned".to_string()))?;
        Ok(ReadTransaction {
            store: Arc::clone(&self.store),
            _read_guard: read_guard,
        })
    }
}

impl ReadOnlyDatabase {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KvError> {
        Database::open(path).map(Self)
    }
}

impl ReadableDatabase for ReadOnlyDatabase {
    fn begin_read(&self) -> Result<ReadTransaction<'_>, KvError> {
        self.0.begin_read()
    }
}

pub struct TableDefinition<K, V> {
    name: &'static str,
    marker: PhantomData<fn() -> (K, V)>,
}

impl<K, V> Copy for TableDefinition<K, V> {}

impl<K, V> Clone for TableDefinition<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> TableDefinition<K, V> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }
}

pub trait PersistedType {
    type Input: ?Sized;
    type Owned;
    fn encode(value: &Self::Input) -> String;
    fn encode_owned(value: &Self::Owned) -> String;
    fn decode(value: &str) -> Result<Self::Owned, KvError>;
}

impl<'a> PersistedType for &'a str {
    type Input = str;
    type Owned = String;

    fn encode(value: &str) -> String {
        value.to_string()
    }

    fn encode_owned(value: &Self::Owned) -> String {
        value.clone()
    }

    fn decode(value: &str) -> Result<Self::Owned, KvError> {
        Ok(value.to_string())
    }
}

impl<'a> PersistedType for &'a [u8] {
    type Input = [u8];
    type Owned = Vec<u8>;

    fn encode(value: &[u8]) -> String {
        let mut encoded = String::with_capacity(value.len() * 2);
        for byte in value {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02x}");
        }
        encoded
    }

    fn encode_owned(value: &Self::Owned) -> String {
        Self::encode(value)
    }

    fn decode(value: &str) -> Result<Self::Owned, KvError> {
        if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(KvError("invalid hexadecimal SurrealDB value".to_string()));
        }
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair)
                    .map_err(|error| KvError(format!("invalid hex pair: {error}")))?;
                u8::from_str_radix(pair, 16)
                    .map_err(|error| KvError(format!("invalid hex pair: {error}")))
            })
            .collect()
    }
}

pub struct AccessGuard<V: PersistedType> {
    value: V::Owned,
    marker: PhantomData<V>,
}

impl<'a> AccessGuard<&'a str> {
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl<'a> AccessGuard<&'a [u8]> {
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Deserialize, SurrealValue)]
struct StoredRow {
    key: String,
    value: String,
}

#[derive(Clone)]
enum Operation {
    Upsert {
        bucket: String,
        key: String,
        value: String,
    },
    Delete {
        bucket: String,
        key: String,
    },
}

pub struct ReadTransaction<'a> {
    store: Arc<surreal_store::Store>,
    _read_guard: RwLockReadGuard<'a, ()>,
}

impl ReadTransaction<'_> {
    pub fn open_table<K, V: PersistedType>(
        &self,
        definition: TableDefinition<K, V>,
    ) -> Result<ReadTable<'_, V>, KvError> {
        Ok(ReadTable {
            store: &self.store,
            bucket: definition.name,
            marker: PhantomData,
        })
    }
}

pub struct WriteTransaction<'a> {
    store: Arc<surreal_store::Store>,
    operations: Arc<Mutex<Vec<Operation>>>,
    _write_guard: RwLockWriteGuard<'a, ()>,
}

impl WriteTransaction<'_> {
    pub fn open_table<K, V: PersistedType>(
        &self,
        definition: TableDefinition<K, V>,
    ) -> Result<WriteTable<'_, V>, KvError> {
        Ok(WriteTable {
            store: &self.store,
            operations: &self.operations,
            bucket: definition.name,
            marker: PhantomData,
        })
    }

    pub fn commit(self) -> Result<(), KvError> {
        let operations = self
            .operations
            .lock()
            .map_err(|_| KvError("SurrealDB transaction buffer is poisoned".to_string()))?
            .clone();
        if operations.is_empty() {
            return Ok(());
        }
        let db = self.store.db();
        surreal_store::run(async move {
            let mut sql = String::from("BEGIN TRANSACTION;\n");
            for (index, operation) in operations.iter().enumerate() {
                match operation {
                    Operation::Upsert { .. } => sql.push_str(&format!(
                        "UPSERT type::record('{TABLE}', $id{index}) SET bucket = $bucket{index}, key = $key{index}, value = $value{index};\n"
                    )),
                    Operation::Delete { .. } => sql.push_str(&format!(
                        "DELETE type::record('{TABLE}', $id{index});\n"
                    )),
                }
            }
            sql.push_str("COMMIT TRANSACTION;");
            let mut query = db.query(sql);
            for (index, operation) in operations.into_iter().enumerate() {
                let (bucket, key) = match &operation {
                    Operation::Upsert { bucket, key, .. } | Operation::Delete { bucket, key } => {
                        (bucket.clone(), key.clone())
                    }
                };
                query = query.bind((format!("id{index}"), record_id(&bucket, &key)));
                if let Operation::Upsert { value, .. } = operation {
                    query = query
                        .bind((format!("bucket{index}"), bucket))
                        .bind((format!("key{index}"), key))
                        .bind((format!("value{index}"), value));
                }
            }
            query
                .await
                .map_err(|error| format!("commit embedded SurrealDB transaction: {error}"))?
                .check()
                .map_err(|error| format!("commit embedded SurrealDB transaction: {error}"))?;
            Ok(())
        })
        .map_err(KvError)
    }
}

pub trait ReadableTable<V: PersistedType> {
    fn get(&self, key: &str) -> Result<Option<AccessGuard<V>>, KvError>;
    fn iter(&self) -> Result<TableIter<V>, KvError>;
}

pub struct ReadTable<'a, V: PersistedType> {
    store: &'a Arc<surreal_store::Store>,
    bucket: &'static str,
    marker: PhantomData<V>,
}

impl<V: PersistedType> ReadTable<'_, V> {
    pub fn iter(&self) -> Result<TableIter<V>, KvError> {
        rows_for_bucket::<V>(&self.store, self.bucket, None)
    }

    pub fn range<R>(&self, range: R) -> Result<TableIter<V>, KvError>
    where
        R: RangeBounds<String>,
    {
        rows_for_bucket::<V>(&self.store, self.bucket, Some(bounds_to_owned(range)))
    }
}

impl<V: PersistedType> ReadableTable<V> for ReadTable<'_, V> {
    fn get(&self, key: &str) -> Result<Option<AccessGuard<V>>, KvError> {
        get_value::<V>(&self.store, self.bucket, key)
    }

    fn iter(&self) -> Result<TableIter<V>, KvError> {
        ReadTable::iter(self)
    }
}

pub struct WriteTable<'a, V: PersistedType> {
    store: &'a Arc<surreal_store::Store>,
    operations: &'a Arc<Mutex<Vec<Operation>>>,
    bucket: &'static str,
    marker: PhantomData<V>,
}

impl<V: PersistedType> WriteTable<'_, V> {
    pub fn insert(&mut self, key: &str, value: &V::Input) -> Result<(), KvError> {
        self.operations
            .lock()
            .map_err(|_| KvError("SurrealDB transaction buffer is poisoned".to_string()))?
            .push(Operation::Upsert {
                bucket: self.bucket.to_string(),
                key: key.to_string(),
                value: V::encode(value),
            });
        Ok(())
    }

    pub fn remove(&mut self, key: &str) -> Result<Option<AccessGuard<V>>, KvError> {
        let previous = ReadableTable::<V>::get(self, key)?;
        self.operations
            .lock()
            .map_err(|_| KvError("SurrealDB transaction buffer is poisoned".to_string()))?
            .push(Operation::Delete {
                bucket: self.bucket.to_string(),
                key: key.to_string(),
            });
        Ok(previous)
    }

    pub fn iter(&self) -> Result<TableIter<V>, KvError> {
        rows_for_bucket_with_operations::<V>(&self.store, self.bucket, None, &self.operations)
    }

    pub fn range<R>(&self, range: R) -> Result<TableIter<V>, KvError>
    where
        R: RangeBounds<String>,
    {
        rows_for_bucket_with_operations::<V>(
            &self.store,
            self.bucket,
            Some(bounds_to_owned(range)),
            &self.operations,
        )
    }
}

impl<V: PersistedType> ReadableTable<V> for WriteTable<'_, V> {
    fn get(&self, key: &str) -> Result<Option<AccessGuard<V>>, KvError> {
        if let Some(operation) = self
            .operations
            .lock()
            .map_err(|_| KvError("SurrealDB transaction buffer is poisoned".to_string()))?
            .iter()
            .rev()
            .find(|operation| match operation {
                Operation::Upsert {
                    bucket, key: row, ..
                }
                | Operation::Delete { bucket, key: row } => bucket == self.bucket && row == key,
            })
            .cloned()
        {
            return match operation {
                Operation::Upsert { value, .. } => Ok(Some(AccessGuard {
                    value: V::decode(&value)?,
                    marker: PhantomData,
                })),
                Operation::Delete { .. } => Ok(None),
            };
        }
        get_value::<V>(&self.store, self.bucket, key)
    }

    fn iter(&self) -> Result<TableIter<V>, KvError> {
        WriteTable::iter(self)
    }
}

pub type TableIter<V> =
    std::vec::IntoIter<Result<(AccessGuard<&'static str>, AccessGuard<V>), KvError>>;

fn get_value<V: PersistedType>(
    store: &Arc<surreal_store::Store>,
    bucket: &str,
    key: &str,
) -> Result<Option<AccessGuard<V>>, KvError> {
    let db = store.db();
    let id = record_id(bucket, key);
    let row: Option<StoredRow> = surreal_store::run(async move {
        let mut response = db
            .query(format!(
                "SELECT key, value FROM ONLY type::record('{TABLE}', $id);"
            ))
            .bind(("id", id))
            .await
            .map_err(|error| format!("read embedded SurrealDB value: {error}"))?;
        response
            .take(0)
            .map_err(|error| format!("decode embedded SurrealDB value: {error}"))
    })
    .map_err(KvError)?;
    row.map(|row| {
        Ok(AccessGuard {
            value: V::decode(&row.value)?,
            marker: PhantomData,
        })
    })
    .transpose()
}

type OwnedBounds = (Bound<String>, Bound<String>);

fn bounds_to_owned<R: RangeBounds<String>>(range: R) -> OwnedBounds {
    let start = match range.start_bound() {
        Bound::Included(value) => Bound::Included(value.to_string()),
        Bound::Excluded(value) => Bound::Excluded(value.to_string()),
        Bound::Unbounded => Bound::Unbounded,
    };
    let end = match range.end_bound() {
        Bound::Included(value) => Bound::Included(value.to_string()),
        Bound::Excluded(value) => Bound::Excluded(value.to_string()),
        Bound::Unbounded => Bound::Unbounded,
    };
    (start, end)
}

fn rows_for_bucket<V: PersistedType>(
    store: &Arc<surreal_store::Store>,
    bucket: &str,
    bounds: Option<OwnedBounds>,
) -> Result<TableIter<V>, KvError> {
    let db = store.db();
    let bucket = bucket.to_string();
    let rows: Vec<StoredRow> = surreal_store::run(async move {
        let mut response = db
            .query(format!(
                "SELECT key, value FROM {TABLE} WHERE bucket = $bucket ORDER BY key;"
            ))
            .bind(("bucket", bucket))
            .await
            .map_err(|error| format!("scan embedded SurrealDB table: {error}"))?;
        response
            .take(0)
            .map_err(|error| format!("decode embedded SurrealDB table: {error}"))
    })
    .map_err(KvError)?;
    let rows = rows
        .into_iter()
        .filter(|row| {
            bounds
                .as_ref()
                .is_none_or(|bounds| in_bounds(&row.key, bounds))
        })
        .map(|row| {
            Ok((
                AccessGuard {
                    value: row.key,
                    marker: PhantomData,
                },
                AccessGuard {
                    value: V::decode(&row.value)?,
                    marker: PhantomData,
                },
            ))
        })
        .collect::<Vec<_>>();
    Ok(rows.into_iter())
}

fn rows_for_bucket_with_operations<V: PersistedType>(
    store: &Arc<surreal_store::Store>,
    bucket: &str,
    bounds: Option<OwnedBounds>,
    operations: &Arc<Mutex<Vec<Operation>>>,
) -> Result<TableIter<V>, KvError> {
    let mut rows = rows_for_bucket::<V>(store, bucket, None)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(key, value)| (key.value, V::encode_owned(&value.value)))
        .collect::<BTreeMap<_, _>>();
    for operation in operations
        .lock()
        .map_err(|_| KvError("SurrealDB transaction buffer is poisoned".to_string()))?
        .iter()
    {
        match operation {
            Operation::Upsert {
                bucket: row_bucket,
                key,
                value,
            } if row_bucket == bucket => {
                rows.insert(key.clone(), value.clone());
            }
            Operation::Delete {
                bucket: row_bucket,
                key,
            } if row_bucket == bucket => {
                rows.remove(key);
            }
            _ => {}
        }
    }
    let rows = rows
        .into_iter()
        .filter(|(key, _)| bounds.as_ref().is_none_or(|bounds| in_bounds(key, bounds)))
        .map(|(key, value)| {
            Ok((
                AccessGuard {
                    value: key,
                    marker: PhantomData,
                },
                AccessGuard {
                    value: V::decode(&value)?,
                    marker: PhantomData,
                },
            ))
        })
        .collect::<Vec<_>>();
    Ok(rows.into_iter())
}

fn in_bounds(value: &str, bounds: &OwnedBounds) -> bool {
    let lower = match &bounds.0 {
        Bound::Included(bound) => value >= bound.as_str(),
        Bound::Excluded(bound) => value > bound.as_str(),
        Bound::Unbounded => true,
    };
    let upper = match &bounds.1 {
        Bound::Included(bound) => value <= bound.as_str(),
        Bound::Excluded(bound) => value < bound.as_str(),
        Bound::Unbounded => true,
    };
    lower && upper
}

fn record_id(bucket: &str, key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(bucket.as_bytes());
    digest.update([0]);
    digest.update(key.as_bytes());
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TABLE: TableDefinition<&str, &str> = TableDefinition::new("transaction_test");

    fn temp_database(label: &str) -> (std::path::PathBuf, Database) {
        let root = std::env::temp_dir().join(format!(
            "facial-surreal-kv-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        let db = Database::create(&root).expect("create test database");
        (root, db)
    }

    #[test]
    fn write_table_iteration_overlays_pending_changes() {
        let (root, db) = temp_database("overlay");
        let seed = db.begin_write().unwrap();
        {
            let mut table = seed.open_table(TEST_TABLE).unwrap();
            table.insert("a", "old").unwrap();
            table.insert("b", "remove").unwrap();
        }
        seed.commit().unwrap();

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(TEST_TABLE).unwrap();
            table.insert("a", "new").unwrap();
            table.remove("b").unwrap();
            table.insert("c", "added").unwrap();
            let rows = table
                .iter()
                .unwrap()
                .map(|row| {
                    let (key, value) = row.unwrap();
                    (key.value().to_string(), value.value().to_string())
                })
                .collect::<BTreeMap<_, _>>();
            assert_eq!(rows.get("a").map(String::as_str), Some("new"));
            assert!(!rows.contains_key("b"));
            assert_eq!(rows.get("c").map(String::as_str), Some("added"));
        }
        txn.commit().unwrap();
        drop(db);
        surreal_store::wait_until_closed(&root).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_read_modify_write_transactions_are_serialized() {
        let (root, db) = temp_database("serialized");
        let db = Arc::new(db);
        let seed = db.begin_write().unwrap();
        {
            let mut table = seed.open_table(TEST_TABLE).unwrap();
            table.insert("counter", "0").unwrap();
        }
        seed.commit().unwrap();

        let threads = (0..8)
            .map(|_| {
                let db = Arc::clone(&db);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let txn = db.begin_write().unwrap();
                        {
                            let mut table = txn.open_table(TEST_TABLE).unwrap();
                            let current = table
                                .get("counter")
                                .unwrap()
                                .unwrap()
                                .value()
                                .parse::<u64>()
                                .unwrap();
                            table.insert("counter", &(current + 1).to_string()).unwrap();
                        }
                        txn.commit().unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(TEST_TABLE).unwrap();
        assert_eq!(table.get("counter").unwrap().unwrap().value(), "160");
        drop(txn);
        drop(db);
        surreal_store::wait_until_closed(&root).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
