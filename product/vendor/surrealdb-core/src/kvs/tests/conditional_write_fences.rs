//! Conflict semantics the durable index-build fences depend on.
//!
//! Writer admission compare-and-swaps a generation's ticket counter. Two
//! transitions have to invalidate an allocation that is already in flight: a
//! generation flip, which removes the counter under a conditional delete, and
//! the `Building`→`Closing` transition, which advances it. Both rely on the
//! backend rejecting one of two conflicting conditional operations on the same
//! key.
//!
//! These belong at this level rather than in the index tests because the
//! guarantee is per-backend: two blind writes to one key do *not* conflict on
//! the last-writer-wins stores (see `multiwriter_same_keys_allow`), so a fence
//! built from blind writes would be a silent no-op there while passing every
//! test on RocksDB.
#![cfg(any(
	feature = "kv-mem",
	feature = "kv-rocksdb",
	feature = "kv-surrealkv",
	feature = "kv-tikv",
))]

use uuid::Uuid;

use super::CreateDs;
use crate::kvs::LockType::*;
use crate::kvs::TransactionType::*;

/// A conditional delete and a concurrent compare-and-swap of the same key must
/// not both commit.
pub async fn conditional_delete_conflicts_with_conditional_write(new_ds: impl CreateDs) {
	let node_id = Uuid::parse_str("2f7c9a1e-3b4d-4c5e-8a6f-0d1e2f3a4b5c").unwrap();
	let (ds, _) = new_ds.create_ds(node_id).await;
	let tx = ds.transaction(Write, Optimistic).await.unwrap();
	tx.set(&"test", &b"0".to_vec()).await.unwrap();
	tx.commit().await.unwrap();
	// A writer allocating from the current value
	let tx1 = ds.transaction(Write, Optimistic).await.unwrap();
	tx1.putc(&"test", &b"1".to_vec(), Some(&b"0".to_vec())).await.unwrap();
	// A generation flip removing the counter
	let tx2 = ds.transaction(Write, Optimistic).await.unwrap();
	tx2.delc(&"test", Some(&b"0".to_vec())).await.unwrap();
	// First committer wins, the other is rejected
	tx1.commit().await.unwrap();
	tx2.commit().await.unwrap_err();
	let tx = ds.transaction(Read, Optimistic).await.unwrap();
	assert_eq!(tx.get(&"test", None).await.unwrap().unwrap(), b"1");
	tx.cancel().await.unwrap();
}

/// The reverse order, which is the one the protocol actually depends on: the
/// conditional delete commits first and the concurrent allocation must then be
/// rejected.
///
/// A reservation committed *before* a flip is visible to the drain that
/// follows it and is harmless, so asserting only the other order would leave
/// the load-bearing case untested.
pub async fn conditional_write_is_rejected_after_a_conditional_delete(new_ds: impl CreateDs) {
	let node_id = Uuid::parse_str("3a8d0b2f-4c5e-4d6f-9b7a-1e2f3a4b5c6d").unwrap();
	let (ds, _) = new_ds.create_ds(node_id).await;
	let tx = ds.transaction(Write, Optimistic).await.unwrap();
	tx.set(&"test", &b"0".to_vec()).await.unwrap();
	tx.commit().await.unwrap();
	let tx1 = ds.transaction(Write, Optimistic).await.unwrap();
	tx1.putc(&"test", &b"1".to_vec(), Some(&b"0".to_vec())).await.unwrap();
	let tx2 = ds.transaction(Write, Optimistic).await.unwrap();
	tx2.delc(&"test", Some(&b"0".to_vec())).await.unwrap();
	// The flip commits first; the allocation must not survive it
	tx2.commit().await.unwrap();
	tx1.commit().await.unwrap_err();
	let tx = ds.transaction(Read, Optimistic).await.unwrap();
	assert!(tx.get(&"test", None).await.unwrap().is_none());
	tx.cancel().await.unwrap();
}

/// Two compare-and-swaps of the same key, from the same observed value, must
/// not both commit.
///
/// The `Closing` fence advances the counter rather than storing the value it
/// read: a same-value write is invisible to a backend that validates by value
/// rather than by version, which would make that fence a silent no-op there.
pub async fn concurrent_conditional_writes_conflict(new_ds: impl CreateDs) {
	let node_id = Uuid::parse_str("4b9e1c30-5d6f-4e70-ac8b-2f3a4b5c6d7e").unwrap();
	let (ds, _) = new_ds.create_ds(node_id).await;
	let tx = ds.transaction(Write, Optimistic).await.unwrap();
	tx.set(&"test", &b"0".to_vec()).await.unwrap();
	tx.commit().await.unwrap();
	// The fence, advancing the counter
	let tx1 = ds.transaction(Write, Optimistic).await.unwrap();
	tx1.putc(&"test", &b"1".to_vec(), Some(&b"0".to_vec())).await.unwrap();
	// A writer allocating from the same observed value
	let tx2 = ds.transaction(Write, Optimistic).await.unwrap();
	tx2.putc(&"test", &b"1".to_vec(), Some(&b"0".to_vec())).await.unwrap();
	tx1.commit().await.unwrap();
	tx2.commit().await.unwrap_err();
	let tx = ds.transaction(Read, Optimistic).await.unwrap();
	assert_eq!(tx.get(&"test", None).await.unwrap().unwrap(), b"1");
	tx.cancel().await.unwrap();
}

macro_rules! define_tests {
	($new_ds:ident) => {
		#[tokio::test]
		#[serial_test::serial]
		async fn conditional_delete_conflicts_with_conditional_write() {
			use super::conditional_write_fences as fences;
			fences::conditional_delete_conflicts_with_conditional_write($new_ds).await;
		}

		#[tokio::test]
		#[serial_test::serial]
		async fn conditional_write_is_rejected_after_a_conditional_delete() {
			use super::conditional_write_fences as fences;
			fences::conditional_write_is_rejected_after_a_conditional_delete($new_ds).await;
		}

		#[tokio::test]
		#[serial_test::serial]
		async fn concurrent_conditional_writes_conflict() {
			use super::conditional_write_fences as fences;
			fences::concurrent_conditional_writes_conflict($new_ds).await;
		}
	};
}
pub(crate) use define_tests;
