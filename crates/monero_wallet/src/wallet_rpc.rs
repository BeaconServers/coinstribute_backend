use std::{time::Duration, str::FromStr};

use monero::Address;
use rayon::prelude::*;
use hyper::client::HttpConnector;
use hyper::{Client, Request, Body};
use simd_json::{ValueAccess, Mutable};
use serde::{Serialize, Deserialize};

pub struct WalletRPC {
	client: Client<HttpConnector, Body>,
	/// For requests that can be expected to take up to 45 seconds
	long_client: Client<HttpConnector, Body>,
	wallet_daemon_url: String,
}

impl WalletRPC {
	pub(crate) fn new(mut wallet_daemon_url: String, connection_timeout: Option<Duration>) -> Self {
		wallet_daemon_url.push_str("/json_rpc");

		let mut connector = HttpConnector::new();
		connector.set_connect_timeout(connection_timeout);

		let mut long_connector = HttpConnector::new();
		long_connector.set_connect_timeout(Some(Duration::from_secs(30)));

		let client = Client::builder()
			.build(connector);

		let long_client = Client::builder()
			.build(long_connector);

		Self {
			client,
			long_client,
			wallet_daemon_url,

		}
	}

	async fn request(&self, payload: &str) -> Result<simd_json::owned::Value, WalletRPCError> {
		let mut last_error = None;
		const MAX_CONN_ATTEMPTS: u8 = 5;

		// Attempt up to 5 times to send a request before returning an error
		for i in 0..MAX_CONN_ATTEMPTS {
			let req = Request::post(&self.wallet_daemon_url)
				.body(Body::from(payload.to_string()))
				.unwrap();

			match self.client.request(req).await {
				Ok(response) => {
					let response = hyper::body::to_bytes(response.into_body()).await?;
					let mut response = response.to_vec();

					let val = simd_json::to_owned_value(&mut response);
					return Ok(val?);

				},
				Err(err) => match err.is_timeout() {
					true => {
						if i == MAX_CONN_ATTEMPTS - 1 {
							last_error = Some(err);
						}

						continue;
					},
					false => Err(WalletRPCError::HyperError(err))?,
				},
			};
		}

		Err(WalletRPCError::HyperError(last_error.unwrap()))?

	}

	/// Just like request, except it uses the long client and only attemptps to send a message twice
	async fn long_request(&self, payload: &str) -> Result<simd_json::owned::Value, WalletRPCError> {
		let mut last_error = None;
		const MAX_CONN_ATTEMPTS: u8 = 2;

		// Attempt up to 5 times to send a request before returning an error
		for i in 0..MAX_CONN_ATTEMPTS {
			let req = Request::post(&self.wallet_daemon_url)
				.body(Body::from(payload.to_string()))
				.unwrap();

			match self.long_client.request(req).await {
				Ok(response) => {
					let response = hyper::body::to_bytes(response.into_body()).await?;
					let mut response = response.to_vec();

					let val = simd_json::to_owned_value(&mut response);
					return Ok(val?);

				},
				Err(err) => match err.is_timeout() {
					true => {
						if i == MAX_CONN_ATTEMPTS - 1 {
							last_error = Some(err);
						}

						continue;
					},
					false => Err(WalletRPCError::HyperError(err))?,
				},
			};
		}

		Err(WalletRPCError::HyperError(last_error.unwrap()))?

	}

	pub async fn create_address(&self, username: &str) -> Result<(Address, u64), WalletRPCError> {
		let mut req_body = String::from(r#"{"jsonrpc":"2.0","id":"0","method":"create_address","params":{"account_index":0,"label":""#);
		req_body.push_str(username);
		req_body.push_str(r#""}}"#);

		let response = self.request(&req_body).await?;
		let result = response.get("result").ok_or(WalletRPCError::MissingData)?;

		let addr = Address::from_str(result.get("address").ok_or(WalletRPCError::MissingData)?.as_str().unwrap()).unwrap();
		let acc_index = result.get("address_index").ok_or(WalletRPCError::MissingData)?.as_u64().unwrap();

		Ok((addr, acc_index))
	}

	/// Returns the wallet balance in piconeros
	pub async fn get_balance(&self, addr_indices: &[u64], unlocked: bool) -> Result<WalletBalance, WalletRPCError> {
		let mut req_body = String::from(r#"{"jsonrpc": "2.0","id": "0","method": "get_balance","params":{"account_index":0,"address_indices":"#);
		let balance_type = match unlocked {
			true => "unlocked_balance",
			false => "balance",

		};

		let addr_indices_string = addr_indices_to_string(addr_indices);

		req_body.push_str(&addr_indices_string);
		req_body.push_str("}}");

        let mut response = self.request(&req_body).await?;
        let result = response.get_mut("result").ok_or(WalletRPCError::MissingData)?;

        let per_subaddress = result.get_mut("per_subaddress").ok_or(WalletRPCError::MissingData)?.as_array_mut().unwrap();

        let balances: Result<Vec<SubaddressBalance>, WalletRPCError> = per_subaddress.par_drain(..).map(|val| {
        	Ok(SubaddressBalance {
	            addr: Address::from_str(val.get("address").ok_or(WalletRPCError::MissingData)?.as_str().unwrap()).unwrap(),
	            addr_index: val.get("address_index").ok_or(WalletRPCError::MissingData)?.as_u64().unwrap(),
	            balance: val.get(balance_type).ok_or(WalletRPCError::MissingData)?.as_u64().unwrap(),
        	})
        }).collect();

       Ok(WalletBalance {
            balance: result.get(balance_type).ok_or(WalletRPCError::MissingData)?.as_u64().unwrap(),
            per_subaddress: balances?,
        })

	}

	pub async fn get_transfers(&self, addr_indices: &[u64], transfers_in: bool, transfers_out: bool) -> Result<Vec<Transfers>, WalletRPCError> {
		let mut req = String::from(r#"{"jsonrpc":"2.0","id":"0","method":"get_transfers","params":{"account_index": 0,"#);

		req.push_str(r#""in":"#);
		req.push_str(&transfers_in.to_string());
		req.push_str(r#","out":"#);
		req.push_str(&transfers_out.to_string());
		req.push_str(r#","subaddr_indices":"#);

		let addr_indices_as_string = addr_indices_to_string(addr_indices);
		req.push_str(&addr_indices_as_string);
		req.push_str("}}");

		let response = self.request(&req).await?;
		let result = response.get("result").ok_or(WalletRPCError::MissingData)?;

		let val_to_transfer = |val: &simd_json::owned::Value| -> Option<Transfer> {
            Some(Transfer {
			    address: Address::from_str(match val.get("address") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_str().unwrap()).unwrap(),
			    amount: match val.get("amount") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_u64().unwrap(),
			    confirmations: match val.get("confirmations") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_u64().unwrap(),
			    double_spend_seen: match val.get("double_spend_seen") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_bool().unwrap(),
			    height: match val.get("height") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_u64().unwrap(),
			    addr_index: match val.get("subaddr_index") {
			    	Some(v) => match v.get("minor") {
                       Some(v) => v.as_u64().unwrap(),
                       None => return None,
                    },
			    	None => return None,
			    },
			    unlock_time: match val.get("unlock_time") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_u64().unwrap(),
			    fee: match val.get("fee") {
			    	Some(v) => v,
			    	None => return None,
			    }.as_u64().unwrap(),
			})
		};

		let mut all_transfers_in = {
			match result.get("in") {
				Some(arr) => {
					let res = arr.as_array().unwrap();
					res.par_iter().filter_map(val_to_transfer).collect()

				},
				None => Vec::new(),
			}
		};


		let mut all_transfers_out = {
			match result.get("out") {
				Some(arr) => {
					let res = arr.as_array().unwrap();
					res.par_iter().filter_map(val_to_transfer).collect()

				},
				None => Vec::new(),
			}
		};

		let mut final_transfers_vec: Vec<Transfers> = addr_indices.iter().map(|indice| {
			Transfers {
			    addr_index: *indice,
			    transfers_in: Vec::new(),
			    transfers_out: Vec::new(),
			}
		}).collect();

		all_transfers_in.drain(..).for_each(|transfer| {
			let index_usize: usize = transfer.addr_index.try_into().unwrap();
			match final_transfers_vec.get_mut(index_usize.checked_sub(1).unwrap()) {
                Some(transfers) => transfers.transfers_in.push(transfer),
                None => (),
            };
		});

		all_transfers_out.drain(..).for_each(|transfer| {
			let index_usize: usize = transfer.addr_index.try_into().unwrap();

            if index_usize > 0 {
                match final_transfers_vec.get_mut(index_usize - 1) {
                    Some(transfers) => transfers.transfers_out.push(transfer),
                    None => (),
                };
            }

		});

		Ok(final_transfers_vec)
	}

	pub async fn get_address(&self) -> Result<Address, WalletRPCError> {
		const REQUEST_BODY: &'static str = r#"{"jsonrpc":"2.0","id":"0","method":"get_address","params":{"account_index":0}}}"#;

        let response = self.request(REQUEST_BODY).await?;
        let addr_str = response["result"]["address"].as_str().ok_or(WalletRPCError::MissingData)?;

        Ok(Address::from_str(addr_str)?)

	}

	pub async fn transfer(&self, priority: Option<u8>, dst: Address, amt: u64, addr_index: u64) -> Result<TransferOut, WalletRPCError> {
		assert!(priority.unwrap_or(0) <= 3);

		let mut req_body = String::from(r#"{"jsonrpc":"2.0","id":"0","method":"transfer","params":{"destinations":[{"amount":"#);
		
		req_body.push_str(&amt.to_string());
		req_body.push_str(r#","address":""#);
		req_body.push_str(&dst.to_string());
		req_body.push_str(r#""}],"priority":"#);
		req_body.push_str(&priority.unwrap_or(0).to_string());
		req_body.push_str(r#","ring_size":11"#);
		req_body.push_str(r#","account_index":0"#);
        req_body.push_str(r#","get_tx_key": true,"#);
        req_body.push_str(r#""subaddr_indices":["#);
        req_body.push_str(&addr_index.to_string());
		req_body.push_str("]}}");

		let response = self.long_request(&req_body).await?;

		let result = response.get("result").ok_or(WalletRPCError::MissingData)?;
		
		Ok(TransferOut {
			amount: result.get("amount").ok_or(WalletRPCError::MissingData)?.as_u64().unwrap(),
			fee: result.get("fee").ok_or(WalletRPCError::MissingData)?.as_u64().unwrap(),
			tx_key: result.get("tx_key").ok_or(WalletRPCError::MissingData)?.as_str().unwrap().to_string(),
		})

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

pub struct TransferOut {
    pub amount: u64,
    pub fee: u64,
    pub tx_key: String,
}

impl TransferOut {
	pub fn amount(&self) -> u64 {
		self.amount
	}

	pub fn fee(&self) -> u64 {
		self.fee
	}

	pub fn total(&self) -> u64 {
		self.amount + self.fee
	}
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transfers {
	pub addr_index: u64,
	pub transfers_in: Vec<Transfer>,
	pub transfers_out: Vec<Transfer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transfer {
	address: Address,
	pub amount: u64,
	pub fee: u64,
	confirmations: u64,
	double_spend_seen: bool,
	height: u64,
	addr_index: u64,
	unlock_time: u64,

}

pub struct WalletBalance {
	pub balance: u64,
	pub per_subaddress: Vec<SubaddressBalance>,
}

#[derive(Debug)]
pub struct SubaddressBalance {
	pub addr: Address,
	pub addr_index: u64,
	pub balance: u64,
}

#[derive(Debug)]
pub enum WalletRPCError {
	InvalidPaymentId,
	InvalidSetDaemonReq,
	InvalidSetRefreshReq,
	MissingData,
	AddrError(monero::util::address::Error),
	HyperError(hyper::Error),
	JSONError(simd_json::Error),

}

impl From<hyper::Error> for WalletRPCError {
    fn from(err: hyper::Error) -> Self {
        Self::HyperError(err)
    }
}

impl From<simd_json::Error> for WalletRPCError {
    fn from(err: simd_json::Error) -> Self {
        Self::JSONError(err)
    }
}

impl From<monero::util::address::Error> for WalletRPCError {
    fn from(err: monero::util::address::Error) -> Self {
        Self::AddrError(err)
    }
}


fn addr_indices_to_string(addr_indices: &[u64]) -> String {
	let mut addr_indices_string = String::from("[");

	addr_indices.iter().for_each(|index| {
		addr_indices_string.push_str(&index.to_string());
	});
	addr_indices_string.push(']');

	addr_indices_string
}
