// Some code is taken from https://github.com/busyboredom/acceptxmr/blob/v0.10.1/src/rpc.rs for the daemon_rpc.rs module, so the below copyright is included.
/*
Copyright (c) 2021 AcceptXMR Contributors

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
 */

use std::time::Duration;

use hashbrown::{HashMap, HashSet};
use hex_simd::AsciiCase;
use rayon::prelude::*;
use reqwest::{Client, ClientBuilder, Method};
use simd_json::ValueAccess;

pub struct DaemonRPC {
	client: Client,
	daemon_url: String,
}

impl DaemonRPC {
	pub(crate) fn new(daemon_url: String, connection_timeout: Option<Duration>, send_timeout: Option<Duration>) -> Self {
		Self {
			client: ClientBuilder::new()
				.connect_timeout(connection_timeout.unwrap_or(Duration::from_millis(3500)))
				.timeout(send_timeout.unwrap_or(Duration::from_millis(3500)))
				.build()
				.unwrap(),
			daemon_url,

		}
	}

	async fn request(&self, payload: &str, endpoint: &str) -> Result<simd_json::owned::Value, DaemonRPCError> {
		let mut last_error = None;
		const MAX_CONN_ATTEMPTS: u8 = 6;

		// Attempt up to 6 times to send a request before returning an error
		for i in 0..MAX_CONN_ATTEMPTS {
			let req = self.client.request(Method::POST, self.daemon_url.clone() + endpoint)
				.body(payload.to_string())
				.build()?;

			match self.client.execute(req).await {
				Ok(response) => return Ok(response.json::<simd_json::owned::Value>().await?),
				Err(err) => match err.is_timeout() {
					true => {
						if i == MAX_CONN_ATTEMPTS - 1 {
							last_error = Some(err);
						}

						continue;
					},
					false => return Err(DaemonRPCError::ReqwestError(err)),
				},
			};
		}

		return Err(DaemonRPCError::ReqwestError(last_error.unwrap()))

	}

	pub async fn get_fee(&self) -> Result<u64, DaemonRPCError> {
		const REQUEST_BODY: &'static str = r#"{"jsonrpc":"2.0","id":"0","method":"get_fee_estimate"}}"#;
		let response = self.request(REQUEST_BODY, "/json_rpc").await?;

		response["result"]["fee"].as_u64().ok_or(DaemonRPCError::MissingData)

	}

	pub async fn block(&self, height: u64) -> Result<(monero::Hash, monero::Block), DaemonRPCError> {
		let request_body = r#"{"jsonrpc":"2.0","id":"0","method":"get_block","params":{"height":"#
			.to_owned()
			+ &height.to_string()
			+ "}}";

		let res: simd_json::owned::Value = self.request(&request_body, "/json_rpc").await?;

		let mut block_hash_hex = res["result"]["block_header"]["hash"]
			.as_str()
			.ok_or(DaemonRPCError::MissingData)?
			.to_string()
			.into_boxed_str()
			.into_boxed_bytes();

		hex_simd::decode_inplace(&mut block_hash_hex)?;

		let block_hash = monero::Hash::from_slice(&block_hash_hex);

		let block_str = res["result"]["blob"].as_str().ok_or(DaemonRPCError::MissingData)?;
		let block_hex = hex_simd::decode_to_boxed_bytes(block_str.as_bytes())?;
		let block: monero::Block = monero::consensus::deserialize(&block_hex)?;

		Ok((block_hash, block))
	}

	pub async fn block_transactions(
		&self,
		block: &monero::Block,
	) -> Result<Vec<monero::Transaction>, DaemonRPCError> {
		// Get block transactions in sets of 100 or less (the restricted RPC maximum).
		let transaction_hashes = &block.tx_hashes;
		self.transactions_by_hashes(transaction_hashes).await
	}

	pub async fn txpool(&self) -> Result<Vec<monero::Transaction>, DaemonRPCError> {
		let mut transactions = Vec::new();
		let request_body = "";

		let res = self.request(request_body, "/get_transaction_pool").await?;

		let blobs = if let Some(txs) = res["transactions"].as_array() {
			txs
		} else {
			// If there are no transactions in the txpool, just return an empty list.
			return Ok(transactions);
		};
		for blob in blobs {
			let mut tx_hex = blob["tx_blob"].as_str().ok_or(DaemonRPCError::MissingData)?.to_string().into_boxed_str().into_boxed_bytes();
			hex_simd::decode_inplace(&mut tx_hex)?;

			let tx: monero::Transaction = monero::consensus::deserialize(&tx_hex)?;
			transactions.push(tx);
		}
		Ok(transactions)
	}

	pub async fn txpool_hashes(&self) -> Result<HashSet<monero::Hash>, DaemonRPCError> {
		let mut transactions = HashSet::new();
		let request_body = "";

		let res = self.request(request_body, "/get_transaction_pool_hashes").await?;

		let blobs = if let Some(tx_hashes) = res["tx_hashes"].as_array() {
			tx_hashes
		} else {
			// If there are no tx hashes, just return an empty list.
			return Ok(transactions);
		};
		for blob in blobs {
			let mut tx_hash_hex = blob.as_str().ok_or(DaemonRPCError::MissingData)?.to_string().into_boxed_str().into_boxed_bytes();
			hex_simd::decode_inplace(&mut tx_hash_hex)?;
			let tx_hash = monero::consensus::deserialize(&tx_hash_hex)?;
			transactions.insert(tx_hash);
		}
		Ok(transactions)
	}

	pub async fn transactions_by_hashes(
		&self,
		hashes: &[monero::Hash],
	) -> Result<Vec<monero::Transaction>, DaemonRPCError> {
		/// Maximum number of transactions to request at once (daemon limits this).
		const MAX_REQUESTED_TRANSACTIONS: usize = 100;


		let mut transactions = Vec::new();
		for i in 0..=hashes.len() / MAX_REQUESTED_TRANSACTIONS {
			// We've gotta grab these in parts to avoid putting too much load on the RPC server, so
			// these are the start and end indexes of the hashes we're grabbing for now.
			// TODO: Get them concurrently.
			let starting_index: usize = i * MAX_REQUESTED_TRANSACTIONS;
			let ending_index: usize =
				std::cmp::min(MAX_REQUESTED_TRANSACTIONS * (i + 1), hashes.len());

			// If requesting an empty list, return what we have now.
			if ending_index == starting_index {
				return Ok(transactions);
			}

			// Build a json containing the hashes of the transactions we want.
			let request_body = r#"{"txs_hashes":"#.to_owned()
				+
					&simd_json::json!(&hashes[starting_index..ending_index]
                    .par_iter()
                    .map(|x| hex_simd::encode_to_boxed_str(x.as_bytes(), AsciiCase::Lower).to_string()) // Convert from monero::Hash to hex.
					.collect::<Vec<String>>())
					.to_string()
				+ "}";

			let res = self.request(&request_body, "/get_transactions").await?;

			let hexes = res["txs_as_hex"]
				.as_array()
				.ok_or_else(|| DaemonRPCError::MissingData)?;
			if ending_index - starting_index == hexes.len() {
				//trace!("Received {} transactions", hexes.len());
			} else {
				/*warn!(
                    "Received {} transactions, requested {}",
                    hexes.len(),
                    ending_index - starting_index
                );*/
			}

			transactions.reserve(hexes.len());

			// Add these transactions to the total list.
			for tx_json in hexes {
				let mut tx_hex = tx_json
					.as_str()
					.expect("failed to read transaction hex from json")
					.to_string()
					.into_boxed_str()
					.into_boxed_bytes();

				hex_simd::decode_inplace(&mut tx_hex)?;
				let tx: monero::Transaction = monero::consensus::deserialize(&tx_hex)?;
				transactions.push(tx);
			}
		}
		Ok(transactions)
	}

	pub async fn daemon_height(&self) -> Result<u64, DaemonRPCError> {
		let request_body = r#"{"jsonrpc":"2.0","id":"0","method":"get_block_count"}"#;

		let res = self.request(request_body, "/json_rpc").await?;

		let count = res["result"]["count"]
			.as_u64()
			.ok_or_else(|| DaemonRPCError::MissingData)?;

		Ok(count)
	}

}

#[derive(Debug)]
pub enum DaemonRPCError {
	MissingData,
	MoneroEncodingError(monero::consensus::encode::Error),
	ReqwestError(reqwest::Error),
	HexError(hex_simd::Error)

}

impl From<reqwest::Error> for DaemonRPCError {
    fn from(e: reqwest::Error) -> Self {
        Self::ReqwestError(e)

    }
}

impl From<monero::consensus::encode::Error> for DaemonRPCError {
	fn from(e: monero::consensus::encode::Error) -> Self {
		Self::MoneroEncodingError(e)

	}
}

impl From<hex_simd::Error> for DaemonRPCError {
	fn from(e: hex_simd::Error) -> Self {
		Self::HexError(e)

	}
}
