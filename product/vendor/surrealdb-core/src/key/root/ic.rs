//! Index Compaction Queue
//!
//! This module defines the key structure used for the index compaction queue.
//! The index compaction system periodically processes indexes that need
//! optimization, particularly full-text indexes that accumulate changes over
//! time.
//!
//! The `Ic` struct represents an entry in the compaction queue, identifying an
//! index that needs to be compacted. The compaction thread processes these
//! entries at regular intervals defined by the `index_compaction_interval`
//! configuration option.
use std::borrow::Cow;

use storekey::{BorrowDecode, Encode};
use uuid::Uuid;

use crate::catalog::{DatabaseId, IndexId, NamespaceId};
use crate::key::category::{Categorise, Category};
use crate::kvs::{KVKey, impl_kv_key_storekey};
use crate::val::TableName;

/// Represents an entry in the index compaction queue
///
/// When an index (particularly a full-text index) needs compaction, an `Ic` key
/// is created and stored in the database. The index compaction thread
/// periodically scans for these keys and processes the corresponding indexes.
///
/// Compaction helps optimize index performance by consolidating changes and
/// removing unnecessary data.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Encode, BorrowDecode)]
#[storekey(format = "()")]
pub(crate) struct IndexCompactionKey<'key> {
	__: u8,
	_a: u8,
	_b: u8,
	_c: u8,
	pub ns: NamespaceId,
	pub db: DatabaseId,
	pub tb: Cow<'key, TableName>,
	pub ix: IndexId,
	pub nid: Uuid,
	pub uid: Uuid,
}

impl_kv_key_storekey!(IndexCompactionKey<'_> => ());

impl Categorise for IndexCompactionKey<'_> {
	fn categorise(&self) -> Category {
		Category::IndexCompaction
	}
}

impl<'key> IndexCompactionKey<'key> {
	pub(crate) fn new(
		ns: NamespaceId,
		db: DatabaseId,
		tb: Cow<'key, TableName>,
		ix: IndexId,
		nid: Uuid,
		uid: Uuid,
	) -> Self {
		Self {
			__: b'/',
			_a: b'!',
			_b: b'i',
			_c: b'c',
			ns,
			db,
			tb,
			ix,
			nid,
			uid,
		}
	}

	/// The range spanning every compaction-queue entry: `/!id` is the
	/// successor of the `/!ic` tag, so the end bound covers the whole
	/// prefix regardless of the bytes that follow it.
	pub(crate) fn range() -> (Vec<u8>, Vec<u8>) {
		(b"/!ic\0".to_vec(), b"/!id".to_vec())
	}

	pub(crate) fn decode_key(k: &[u8]) -> anyhow::Result<IndexCompactionKey<'_>> {
		Ok(storekey::decode_borrow(k)?)
	}
}

/// Prefix of every compaction-queue entry belonging to one index.
///
/// Its encoded form is the shared prefix of exactly the index's queue
/// entries; [`Self::successor`] is the smallest key past them, which the
/// batched queue drain seeks to so the next batch continues with the next
/// index's entries.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Encode, BorrowDecode)]
#[storekey(format = "()")]
pub(crate) struct IndexCompactionIndexPrefix<'key> {
	__: u8,
	_a: u8,
	_b: u8,
	_c: u8,
	pub ns: NamespaceId,
	pub db: DatabaseId,
	pub tb: Cow<'key, TableName>,
	pub ix: IndexId,
}

impl_kv_key_storekey!(IndexCompactionIndexPrefix<'_> => ());

impl<'key> IndexCompactionIndexPrefix<'key> {
	pub(crate) fn new(
		ns: NamespaceId,
		db: DatabaseId,
		tb: Cow<'key, TableName>,
		ix: IndexId,
	) -> Self {
		Self {
			__: b'/',
			_a: b'!',
			_b: b'i',
			_c: b'c',
			ns,
			db,
			tb,
			ix,
		}
	}

	/// The smallest key strictly greater than every queue entry of this
	/// index: the encoded prefix with trailing `0xff` bytes stripped and
	/// the final byte incremented. Never empty — the encoding starts with
	/// `/`, so the carry cannot consume the whole key.
	pub(crate) fn successor(&self) -> anyhow::Result<Vec<u8>> {
		let mut bytes = self.encode_key()?;
		while bytes.last() == Some(&0xff) {
			bytes.pop();
		}
		match bytes.last_mut() {
			Some(b) => *b += 1,
			None => anyhow::bail!("cannot compute the successor of an empty key prefix"),
		}
		Ok(bytes)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::key::root::ic::IndexCompactionKey;
	use crate::kvs::KVKey;

	#[test]
	fn range() {
		assert_eq!(IndexCompactionKey::range(), (b"/!ic\0".to_vec(), b"/!id".to_vec()));
	}

	#[test]
	fn key() {
		let val = IndexCompactionKey::new(
			NamespaceId(1),
			DatabaseId(2),
			Cow::Owned(TableName::from("testtb")),
			IndexId(3),
			Uuid::from_u128(1),
			Uuid::from_u128(2),
		);
		let enc = IndexCompactionKey::encode_key(&val).unwrap();
		assert_eq!(enc, b"/!ic\x00\x00\x00\x01\x00\x00\x00\x02testtb\0\0\0\0\x03\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x02");
	}

	#[test]
	fn index_prefix_successor_brackets_exactly_one_index() {
		let entry = |tb: &str, ix: u32| {
			IndexCompactionKey::new(
				NamespaceId(1),
				DatabaseId(2),
				Cow::Owned(TableName::from(tb)),
				IndexId(ix),
				Uuid::from_u128(1),
				Uuid::from_u128(2),
			)
			.encode_key()
			.unwrap()
		};
		let prefix = IndexCompactionIndexPrefix::new(
			NamespaceId(1),
			DatabaseId(2),
			Cow::Owned(TableName::from("testtb")),
			IndexId(3),
		);
		let start = prefix.encode_key().unwrap();
		let succ = prefix.successor().unwrap();
		// Entries of this index fall between the prefix and its successor…
		assert!(start.as_slice() <= entry("testtb", 3).as_slice());
		assert!(entry("testtb", 3).as_slice() < succ.as_slice());
		// …while the next index and the next table start at or past it.
		assert!(entry("testtb", 4).as_slice() >= succ.as_slice());
		assert!(entry("testtc", 0).as_slice() >= succ.as_slice());
	}
}
