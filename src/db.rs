// Generics babyyyyyyyyy
use serde::de::DeserializeOwned;
use serde::Serialize;
use sled::{Batch, Result, Tree};

use crate::auth::{AuthCookie, GeneratedCaptcha, User};
use crate::money::FinancialInfo;
use crate::software::Item;

#[derive(Clone)]
pub struct AuthDB(pub Tree);
#[derive(Clone)]
pub struct CookieDB(pub Tree);
#[derive(Clone)]
pub struct MoneyDB(Tree);
#[derive(Clone)]
pub struct CurrentPaymentIdDB(pub Tree);
#[derive(Clone)]
pub struct CaptchaDB(pub Tree);
#[derive(Clone)]
pub struct SoftwareDB(pub Tree);
#[derive(Clone)]
pub struct UploadIdDB(pub Tree);

pub trait DB<K: serde::Serialize + DeserializeOwned, V: Serialize + DeserializeOwned> {
	fn new(tree: Tree) -> Self;
	fn get_tree(&self) -> &Tree;
	fn contains_key(&self, key: &K) -> Result<bool> {
		let key_as_bytes = bincode::serialize(key).unwrap();
		self.get_tree().contains_key(&key_as_bytes)
	}
	fn flush(&self) -> Result<usize> {
		self.get_tree().flush()
	}
	fn len(&self) -> usize {
		self.get_tree().len()
	}
	fn remove(&self, key: &K) -> Result<Option<V>> {
		let key_bytes = bincode::serialize(&key).unwrap();

		match self.get_tree().remove(&key_bytes)? {
			Some(old_val_bytes) => {
				let old_val: V = bincode::deserialize(&old_val_bytes).unwrap();
				Ok(Some(old_val))
			},
			None => Ok(None),
		}
	}

	fn insert(&self, key: &K, val: &V) -> Result<Option<V>> {
		let key_as_bytes = bincode::serialize(key).unwrap();
		let val_as_bytes = bincode::serialize(val).unwrap();

		match self.get_tree().insert(key_as_bytes, val_as_bytes)? {
			Some(old_val_bytes) => {
				let old_val: V = bincode::deserialize(&old_val_bytes).unwrap();
				Ok(Some(old_val))
			},
			None => Ok(None),
		}
	}

	fn get(&self, key: &K) -> sled::Result<Option<V>> {
		let key_as_bytes = bincode::serialize(key).unwrap();

		match self.get_tree().get(key_as_bytes)? {
			Some(val_bytes) => {
				let val: V = bincode::deserialize(&val_bytes).unwrap();
				Ok(Some(val))
			},
			None => Ok(None),
		}
	}

	fn apply_batch(&self, batch: Batch) -> Result<()> {
		self.get_tree().apply_batch(batch)
	}

	fn iter(&self) -> Box<dyn Iterator<Item = sled::Result<(K, V)>>> {
		Box::new(self.get_tree().iter().map(|res| match res {
			Ok((key_bytes, val_bytes)) => {
				let key: K = bincode::deserialize(&key_bytes).unwrap();
				let val: V = bincode::deserialize(&val_bytes).unwrap();

				Ok((key, val))
			},
			Err(e) => Err(e),
		}))
	}

	fn values(&self) -> Box<dyn Iterator<Item = Result<V>>> {
		Box::new(self.sled_iter().values().map(|v| match v {
			Ok(val_bytes) => {
				let val: V = bincode::deserialize(&val_bytes).unwrap();
				Ok(val)
			},
			Err(e) => Err(e),
		}))
	}

	fn sled_iter(&self) -> sled::Iter {
		self.get_tree().iter()
	}
}

impl DB<String, User> for AuthDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}

impl DB<String, AuthCookie> for CookieDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}

impl DB<String, FinancialInfo> for MoneyDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}

impl DB<String, u64> for CurrentPaymentIdDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}

impl DB<[u8; blake3::OUT_LEN], GeneratedCaptcha> for CaptchaDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}

impl DB<u64, Item> for SoftwareDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}

impl DB<[u8; blake3::OUT_LEN], u64> for UploadIdDB {
	fn new(tree: Tree) -> Self {
		Self(tree)
	}

	fn get_tree(&self) -> &Tree {
		&self.0
	}
}
