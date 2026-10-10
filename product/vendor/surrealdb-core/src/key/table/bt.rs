//! Stores the writer-admission ticket counter for one index build generation.
//!
//! Allocating a ticket is the one piece of writer admission that every indexed
//! write performs, so the counter lives on its own generation-scoped key rather
//! than on `!bs`. The builder's initial-scan batch compare-and-swaps `!bs`
//! twice — the ownership heartbeat before the batch and the progress checkpoint
//! after it — and holds that transaction open for the whole batch, so a counter
//! sharing `!bs` would let every admitted write invalidate the batch in flight.
//!
//! The key exists for exactly as long as its generation is the active one: the
//! transaction that installs a generation creates it, and the transaction that
//! installs the next generation removes it. Writers therefore always
//! compare-and-swap a key that is present, and both sides of a generation flip
//! read before they write. That is what fences an in-flight admission against a
//! concurrent flip on last-writer-wins backends (TiKV, IndexedDB, SurrealDS),
//! where a blind write never conflicts.
use std::borrow::Cow;
use std::ops::Range;

use storekey::{BorrowDecode, Encode};

use crate::catalog::{DatabaseId, IndexId, NamespaceId};
use crate::key::category::{Categorise, Category};
use crate::kvs::index::{BuildGeneration, BuildTicket};
use crate::kvs::{KVKey, Key, impl_kv_key_storekey};
use crate::val::TableName;

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Encode, BorrowDecode)]
#[storekey(format = "()")]
pub(crate) struct Bt<'a> {
	__: u8,
	_a: u8,
	pub ns: NamespaceId,
	_b: u8,
	pub db: DatabaseId,
	_c: u8,
	pub tb: Cow<'a, TableName>,
	_d: u8,
	_e: u8,
	_f: u8,
	pub ix: IndexId,
	pub generation: BuildGeneration,
}

impl_kv_key_storekey!(Bt<'_> => BuildTicket);

impl Categorise for Bt<'_> {
	fn categorise(&self) -> Category {
		Category::IndexBuildTicketCounter
	}
}

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Encode)]
#[storekey(format = "()")]
pub(crate) struct BtPrefix<'a> {
	__: u8,
	_a: u8,
	pub ns: NamespaceId,
	_b: u8,
	pub db: DatabaseId,
	_c: u8,
	pub tb: Cow<'a, TableName>,
	_d: u8,
	_e: u8,
	_f: u8,
	pub ix: IndexId,
}

impl_kv_key_storekey!(BtPrefix<'_> => ());

impl<'a> Bt<'a> {
	/// Create a key for the ticket counter of one build generation.
	pub(crate) fn new(
		ns: NamespaceId,
		db: DatabaseId,
		tb: &'a TableName,
		ix: IndexId,
		generation: BuildGeneration,
	) -> Self {
		Self {
			__: b'/',
			_a: b'*',
			ns,
			_b: b'*',
			db,
			_c: b'*',
			tb: Cow::Borrowed(tb),
			_d: b'!',
			_e: b'b',
			_f: b't',
			ix,
			generation,
		}
	}

	/// Return the range covering the counter of one build generation.
	pub(crate) fn range(
		ns: NamespaceId,
		db: DatabaseId,
		tb: &'a TableName,
		ix: IndexId,
		generation: BuildGeneration,
	) -> anyhow::Result<Range<Key>> {
		let beg = Self::new(ns, db, tb, ix, generation).encode_key()?;
		let mut end = beg.clone();
		end.push(0);
		Ok(beg..end)
	}

	/// Return the range covering the counters of every generation.
	pub(crate) fn all_generations_range(
		ns: NamespaceId,
		db: DatabaseId,
		tb: &'a TableName,
		ix: IndexId,
	) -> anyhow::Result<Range<Key>> {
		let mut beg = BtPrefix {
			__: b'/',
			_a: b'*',
			ns,
			_b: b'*',
			db,
			_c: b'*',
			tb: Cow::Borrowed(tb),
			_d: b'!',
			_e: b'b',
			_f: b't',
			ix,
		}
		.encode_key()?;
		let mut end = beg.clone();
		beg.push(0);
		end.push(0xff);
		Ok(beg..end)
	}
}
