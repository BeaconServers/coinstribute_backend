use std::{time::Duration, str::FromStr};

use hex_simd::AsciiCase;
use monero::{util::address::PaymentId, Address};
use rayon::prelude::*;
use reqwest::{Client, ClientBuilder, Method};
use simd_json::ValueAccess;
use serde::{Serialize, Deserialize};

pub struct WalletRPC {
	client: Client,
	wallet_daemon_url: String,
}

impl WalletRPC {
	pub(crate) fn new(wallet_daemon_url: String, connection_timeout: Option<Duration>, send_timeout: Option<Duration>) -> Self {
		Self {
			client: ClientBuilder::new()
				.connect_timeout(connection_timeout.unwrap_or(Duration::from_millis(3500)))
				.timeout(send_timeout.unwrap_or(Duration::from_millis(3500)))
				.build()
				.unwrap(),
			wallet_daemon_url,

		}
	}

	async fn request(&self, payload: &str) -> Result<simd_json::owned::Value, WalletRPCError> {
		let mut last_error = None;
		const MAX_CONN_ATTEMPTS: u8 = 5;

		// Attempt up to 5 times to send a request before returning an error
		for i in 0..MAX_CONN_ATTEMPTS {
			let req = self.client.request(Method::POST, self.wallet_daemon_url.clone() + "/json_rpc")
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
					false => return Err(WalletRPCError::ReqwestError(err)),
				},
			};
		}

		return Err(WalletRPCError::ReqwestError(last_error.unwrap()))

	}

	/// Returns the wallet balance in piconeros
	pub async fn get_balance(&self) -> Result<u64, WalletRPCError> {
		const REQUEST_BODY: &'static str = r#"{"jsonrpc": "2.0","id": "0","method": "get_balance","params":{"account_index":0}}"#;

        let response = self.request(REQUEST_BODY).await?;
        response["result"]["balance"].as_u64().ok_or(WalletRPCError::MissingData)

	}

	pub async fn get_address(&self) -> Result<Address, WalletRPCError> {
		const REQUEST_BODY: &'static str = r#"{"jsonrpc":"2.0","id":"0","method":"get_address","params":{"account_index":0}}}"#;

        let response = self.request(REQUEST_BODY).await?;
        let addr_str = response["result"]["address"].as_str().ok_or(WalletRPCError::MissingData)?;

        Ok(Address::from_str(addr_str)?)

	}

	/// Gets all the payments sent to a specific PaymentID (pray for me please)
	pub async fn get_payments(&self, payment_id: PaymentId) -> Result<Vec<Payment>, WalletRPCError> {
		let payment_id_hex = {
			let payment_id_bytes = payment_id.as_bytes();
			hex_simd::encode_to_boxed_str(payment_id_bytes, AsciiCase::Lower).into_string()

		};

		let mut req_body = String::from(r#"{"jsonrpc":"2.0","id":"0","method":"get_payments","params":{"payment_id":""#);

		req_body.push_str(&payment_id_hex);
		req_body.push_str(r#""}}"#);

		let response = self.request(&req_body).await?;

		if let Some(payments_json) = response["result"]["payments"].as_array() {
			let payments: Result<Vec<Payment>, WalletRPCError> = payments_json.par_iter().map(|payment_json| {
				Ok(Payment {
			        payment_id: {
			        	let payment_id_json = &payment_json["payment_id"];

			        	match payment_id_json.as_str() {
			        		Some(id_str) => {
			        			let hex_bytes = match hex_simd::decode_to_boxed_bytes(id_str.as_bytes()) {
			        				Ok(b) => b,
			        				Err(e) => return Err(WalletRPCError::HexError(e)),
			        			};

			        			if hex_bytes.len() != 8 {
			        				return Err(WalletRPCError::InvalidPaymentId)
			        			}

			        			let mut hex_bytes_array: [u8; 8] = [0; 8];
			        			hex_bytes_array.copy_from_slice(&hex_bytes);

			        			hex_bytes_array

			        		},
			        		None => return Err(WalletRPCError::MissingData),

			        	}

			        },
			        tx_hash: match payment_json["tx_hash"].as_str() {
			        	Some(tx_hash) => tx_hash.to_string(),
			        	None => return Err(WalletRPCError::MissingData),
			        },
			        amount: match payment_json["amount"].as_u64() {
			        	Some(amount) => amount,
			        	None => return Err(WalletRPCError::MissingData),
			        },
			        block_height: match payment_json["block_height"].as_u64() {
			        	Some(block_height) => block_height,
			        	None => return Err(WalletRPCError::MissingData),
			        },
			        unlock_time: match payment_json["unlock_time"].as_u64() {
			        	Some(unlock_height) => unlock_height,
			        	None => return Err(WalletRPCError::MissingData),
			        },
			        subaddr_index_major: match payment_json["subaddr_index"]["major"].as_u64() {
			        	Some(index_major) => index_major,
			        	None => return Err(WalletRPCError::MissingData),
			        },
			        subaddr_index_minor: match payment_json["subaddr_index"]["minor"].as_u64() {
			        	Some(index_minor) => index_minor,
			        	None => return Err(WalletRPCError::MissingData),
			        },
			        recv_addr: {
			        	match payment_json["address"].as_str() {
			        		Some(addr) => {
			        			match Address::from_str(addr) {
			        				Ok(addr) => addr,
			        				Err(e) =>return Err(WalletRPCError::AddrError(e)),
			        			}

			        		},
			        		None => return Err(WalletRPCError::MissingData),
			        	}
			        },
			    })
			}).collect();

			payments

		} else {
			Err(WalletRPCError::MissingData)

		}

	}

	pub async fn set_daemon(&self, daemon_url: &str, trusted: bool) -> Result<(), WalletRPCError> {
		let mut req_body = String::from(r#"{"jsonrpc":"2.0","id":"0","method":"set_daemon","params":{"address":""#);
		req_body.push_str(daemon_url);
		req_body.push_str(r#"","trusted":"#);
		req_body.push_str(&trusted.to_string());
		req_body.push_str(r#"}}"#);

        let response = self.request(&req_body).await?;
        
        match response.contains_key("result") {
        	true => Ok(()),
        	false => Err(WalletRPCError::InvalidSetDaemonReq),
        }
	}

	pub async fn set_refresh_time(&self, refresh_time: u64) -> Result<(), WalletRPCError> {
		let mut req_body = String::from(r#"{"jsonrpc":"2.0","id":"0","method":"auto_refresh","params":{"period":"#);
		req_body.push_str(&refresh_time.to_string());
		req_body.push_str("}}");

		let response = self.request(&req_body).await?;

		match response.contains_key("result") {
			true => Ok(()),
			false => Err(WalletRPCError::InvalidSetRefreshReq),
		}
	}

}

#[derive(Clone, Serialize, Deserialize)]
pub struct Payment {
	pub payment_id: [u8; 8],
	pub tx_hash: String,
	pub amount: u64,
	pub block_height: u64,
	/// Time (in block height) until this payment is safe to spend.
	pub unlock_time: u64,
	/// The account index for the subaddress
	pub subaddr_index_major: u64,
	/// Index of the subaddress of the account
	pub subaddr_index_minor: u64,
	/// Account receiving the payment
	pub recv_addr: Address,
}

#[derive(Debug)]
pub enum WalletRPCError {
	InvalidPaymentId,
	InvalidSetDaemonReq,
	InvalidSetRefreshReq,
	MissingData,
	AddrError(monero::util::address::Error),
	HexError(hex_simd::Error),
	ReqwestError(reqwest::Error),

}

impl From<reqwest::Error> for WalletRPCError {
    fn from(err: reqwest::Error) -> Self {
        Self::ReqwestError(err)
    }
}

impl From<monero::util::address::Error> for WalletRPCError {
    fn from(err: monero::util::address::Error) -> Self {
        Self::AddrError(err)
    }
}
