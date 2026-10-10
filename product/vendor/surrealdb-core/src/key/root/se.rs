//! Stores durable RPC session state
use std::ops::Range;

use anyhow::Result;
use storekey::{BorrowDecode, Encode};
use uuid::Uuid;

use crate::dbs::DurableSession;
use crate::key::category::{Categorise, Category};
use crate::kvs::impl_kv_key_storekey;

// Represents the durable copy of a client-attached RPC session, so the
// session survives the process that attached it and is reachable from any
// cluster node sharing the datastore.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Encode, BorrowDecode)]
pub(crate) struct Se {
	__: u8,
	_a: u8,
	_b: u8,
	_c: u8,
	pub id: Uuid,
}

impl_kv_key_storekey!(Se => DurableSession);

impl Categorise for Se {
	fn categorise(&self) -> Category {
		Category::RpcSession
	}
}

impl Se {
	pub(crate) fn new(id: Uuid) -> Self {
		Self {
			__: b'/',
			_a: b'!',
			_b: b's',
			_c: b'e',
			id,
		}
	}

	pub(crate) fn decode_key(k: &[u8]) -> Result<Se> {
		Ok(storekey::decode_borrow(k)?)
	}
}

/// The range spanning every durable RPC session entry (`/!se{id}`), used to
/// scan all sessions for the periodic expiry purge.
pub(crate) struct SePrefix {}

impl SePrefix {
	pub(crate) fn encode_range(&self) -> Result<Range<Vec<u8>>> {
		Ok(b"/!se\x00".to_vec()..b"/!sf".to_vec())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::kvs::KVKey;

	#[test]
	fn key() {
		let val = Se::new(Uuid::default());
		let enc = Se::encode_key(&val).unwrap();
		assert_eq!(&enc, b"/!se\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00");
	}

	#[test]
	fn test_prefix() {
		let range = SePrefix {}.encode_range().unwrap();
		assert_eq!(range.start.as_slice(), b"/!se\0");
		assert_eq!(range.end.as_slice(), b"/!sf");
	}
}
