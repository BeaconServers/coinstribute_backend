use crate::{RequestDenial, DenialFault};
use crate::auth::verify_auth_cookie;
use crate::db::{AuthDB, CookieDB, MoneyDB, DB};
use crate::fundraiser::Fundraiser;

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Serialize, Deserialize};
use monero_wallet::{Address, Wallet, Transfers};
use parking_lot::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver};

use warp::{Rejection, Reply};
use warp::{http::Response, hyper::StatusCode};

#[derive(Deserialize)]
pub struct AuthorizedReq {
	username: String,
	auth_cookie: String,
}

#[derive(Deserialize)]
pub(crate) struct AttachXMRAddress {
	username: String,
	auth_cookie: String,
	monero_address: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct FinancialInfo {
	xmr_addr: Option<Address>,
	deposit_addr: Address,
	transfers: Transfers,
	balance: u64,
	pending_balance: u64,
    active_fundraisers: Vec<Fundraiser>,
	old_fundraisers: Vec<Fundraiser>,
	service_fee_total: u64,
}

impl FinancialInfo {
	pub fn new(addr_index: u64, deposit_addr: Address) -> Self {
		Self {
			xmr_addr: None,
			deposit_addr,
			active_fundraisers: Vec::new(),
			old_fundraisers: Vec::new(),
		    balance: 0,
		    pending_balance: 0,
		    service_fee_total: 0,
            transfers: Transfers {
				addr_index,
				transfers_in: Vec::new(),
				transfers_out: Vec::new(),
			},
		}
	}

	/// Adds all the transfers in and transfers out together
	fn get_balance(&self) -> u64 {
		let active_fund_total: u64 = self.active_fundraisers.iter().map(|f| f.amt_earned()).sum();
		let past_fund_total: u64 = self.old_fundraisers.iter().map(|f| f.amt_earned()).sum();
		// Using an algabraic proof that I calculated then simplified a ton, we know that service_fee is equal to:
		//  (amt_sent + network_fee) / 49
		// We know this because service_fee = (2 * orig_amt) / 100 and we know that t.amount = orig_amt - service_fee - net_fee
		// Using some algebra, we can solve for service_fee (this is for if Susorodni wants to check my math on this lol)
		// We will just use the max of this calculatin and the self.service_fee_total, since calculating the service fee will lag behind slightly while we wait for thee monero rpc to catch up
		let transfer_out_service_fee_total: u64 = self.transfers.transfers_out.iter().map(|t| (t.amount + t.fee) / 49).sum();
		
		active_fund_total + past_fund_total + self.balance - (self.service_fee_total.max(transfer_out_service_fee_total))

	}

	pub fn add_fundraiser(&mut self, fundraiser: Fundraiser) {
		self.active_fundraisers.push(fundraiser);

	}

	pub fn active_fundraisers(&self) -> &Vec<Fundraiser> {
		&self.active_fundraisers
	}
}

#[derive(Serialize)]
struct DepositReqResp {
	addr: String,
	min_amt: u64,
}

pub(crate) async fn deposit_req(invoice_req: AuthorizedReq, auth_db: AuthDB, auth_cookie_db: CookieDB, money_db: MoneyDB, wallet: &Wallet, cached_fee: Arc<CachedFee>) -> Result<impl Reply, Rejection> {
	if verify_auth_cookie(&invoice_req.username, &invoice_req.auth_cookie, &auth_cookie_db) && auth_db.contains_key(&invoice_req.username).unwrap() {
        let fee = cached_fee.get_fee(wallet).await;

        let financial_info = money_db.get(&invoice_req.username).unwrap().unwrap();

        let resp = DepositReqResp {
			addr: financial_info.deposit_addr.to_string(),
			min_amt: fee * 5,
		};

		let json = simd_json::to_string(&resp).unwrap();

		Ok(Response::builder()
			.status(StatusCode::OK)
			.body(json)
			.unwrap())

    } else {
		let denial = RequestDenial::new(
			DenialFault::User,
			"Invalid username or cookie".to_string(),
			String::new(),
		);

		Ok(denial.into_response(StatusCode::UNAUTHORIZED))

    }

}

#[derive(Serialize)]
struct BalanceReqResp {
	balance: u64,
    pending_deposits: u64,
}

pub async fn get_balance(auth_req: AuthorizedReq, auth_db: AuthDB, money_db: MoneyDB, auth_cookie_db: CookieDB) -> Result<impl Reply, Rejection> {
    if verify_auth_cookie(&auth_req.username, &auth_req.auth_cookie, &auth_cookie_db) && auth_db.contains_key(&auth_req.username).unwrap() {
        let financial_info = money_db.get(&auth_req.username).unwrap().unwrap();

        let resp = BalanceReqResp {
			balance: financial_info.get_balance(),
		    pending_deposits: financial_info.pending_balance,
        };

		let json = simd_json::to_string(&resp).unwrap();

		Ok(
			Response::builder()
				.status(StatusCode::OK)
				.body(json)
				.unwrap()
		)

    } else {
    	let denial = RequestDenial::new(
    		DenialFault::User,
    		"Invalid username or cookie".to_string(),
    		String::new(),
    	);

    	Ok(denial.into_response(StatusCode::UNAUTHORIZED))

    }

}

pub(crate) fn attach_xmr_address(attach_req: AttachXMRAddress, money_db: MoneyDB, auth_db: AuthDB, auth_cookie_db: CookieDB) -> Response<String> {
	if auth_db.contains_key(&attach_req.username).unwrap() && verify_auth_cookie(&attach_req.username, &attach_req.auth_cookie, &auth_cookie_db) {
		let mut financial_info = money_db.get(&attach_req.username).unwrap().unwrap();

		let xmr_addr = match Address::from_str(&attach_req.monero_address) {
			Ok(monero_addr) => monero_addr,
			Err(_) => {
				let resp = RequestDenial::new(
					DenialFault::User,
					"Invalid address".to_string(),
					String::new(),
				);

				return resp.into_response(StatusCode::BAD_REQUEST);
			},
		};

		financial_info.xmr_addr = Some(xmr_addr);

		money_db.insert(&attach_req.username, &financial_info).unwrap();

		Response::builder()
	        .status(StatusCode::OK)
	        .body(String::new())
	        .unwrap()

	} else {
		let resp = RequestDenial::new(
			DenialFault::User,
			"Invalid username or cookie".to_string(),
			String::new(),
		);

		resp.into_response(StatusCode::UNAUTHORIZED)

	}
}

// TODO: Actually secure this
pub async fn server_profits(auth_req: AuthorizedReq, money_db: MoneyDB) -> Result<impl Reply, Rejection> {
	if &auth_req.username == "billy" && &auth_req.auth_cookie == "bobby" {
		let wallet_balance = crate::wallet().get_balance(&[]).await.unwrap().balance;

		let user_total_balance: u64 = money_db.iter().map(|res| {
			let (_, financial_info) = res.unwrap();
			financial_info.get_balance()
		}).sum();

		println!("{}", wallet_balance - user_total_balance);

	}

	Ok(String::new())
}

#[derive(Deserialize)]
pub struct WithdrawMoneroReq {
	username: String,
	auth_cookie: String,
	amt: u64,
}

#[derive(Serialize)]
pub struct WithdrawMoneroResp {
	amt_sent_after_fee: u64,
	fee_taken: u64,
}

pub async fn withdraw_monero(withdraw_req: WithdrawMoneroReq, auth_db: AuthDB, cookie_db: CookieDB, money_db: MoneyDB, cached_fee: Arc<CachedFee>, monero_tx_send: UnboundedSender<TransferReq>) -> Result<impl Reply, Rejection> {
	if auth_db.contains_key(&withdraw_req.username).unwrap() && verify_auth_cookie(&withdraw_req.username, &withdraw_req.auth_cookie, &cookie_db) {
		let wallet = crate::wallet();

		let net_fee = cached_fee.get_uptodate_fee(wallet).await;
		if net_fee.is_none() {
            let denial = RequestDenial::new(DenialFault::Server, String::from("Could not get Monero fee"), String::new());
            return Ok(denial.into_response(StatusCode::INTERNAL_SERVER_ERROR));
        }
        let net_fee = net_fee.unwrap();

        let min_amt = net_fee * 5;

		if withdraw_req.amt >= min_amt {
			let financial_info = match money_db.get(&withdraw_req.username).unwrap() {
				Some(financial_info) => financial_info,
				None => {
					let denial = RequestDenial::new(DenialFault::User, String::from("Invalid username or cookie"), String::new());
					return Ok(denial.into_response(StatusCode::UNAUTHORIZED));
				},
			};

			let service_fee = 2 * (withdraw_req.amt / 100);
			let user_balance = financial_info.get_balance();

			if user_balance >= min_amt {
				let amt_to_send_after_fees = match (withdraw_req.amt).checked_sub(net_fee + service_fee) {
					Some(amt) => amt,
					None => {
						let denial = RequestDenial::new(DenialFault::User, format!("Withdrawal amount {} is lower than 0 after fees", withdraw_req.amt), String::new());
						return Ok(denial.into_response(StatusCode::BAD_REQUEST));
					}
				};

				let withdraw_addr = match financial_info.xmr_addr {
					Some(addr) => addr,
					None => {
						let denial = RequestDenial::new(DenialFault::User, String::from("Withdrawal address not found"), String::from("Please attach a withdrawl address to your account"));
						return Ok(denial.into_response(StatusCode::BAD_REQUEST));
					},
				};

				let transfer_req = TransferReq {
					username: withdraw_req.username.clone(),
			        dst: withdraw_addr,
			        priority: Some(0),
			        amt: amt_to_send_after_fees,
			        addr_index: financial_info.transfers.addr_index,
			    };

				// Because sending RPC calls to the wallet takes forever, we actually add the transaction to a queue of transactions to send
				match monero_tx_send.send(transfer_req) {
					Ok(_) =>  {
						let withdraw_resp = WithdrawMoneroResp {
					        amt_sent_after_fee: amt_to_send_after_fees,
					        fee_taken: service_fee + net_fee,
					    };

					    let json = simd_json::to_string(&withdraw_resp).unwrap();

					    Ok(Response::builder()
					    	.status(StatusCode::OK)
					    	.body(json)
					    	.unwrap())

					},
					Err(e) => {
						let denial = RequestDenial::new(DenialFault::Server, String::from("Internal server error, please try again"), String::new());
						Ok(denial.into_response(StatusCode::INTERNAL_SERVER_ERROR))
					},

				}
			} else {
				let denial = RequestDenial::new(DenialFault::User, format!("User balance {user_balance} is lower than the minimum: {min_amt}"), String::new());
				Ok(denial.into_response(StatusCode::PRECONDITION_FAILED))

			}

		} else {
			let denial = RequestDenial::new(DenialFault::User, format!("Withdrawal amount {} is lower than the minimum: {min_amt:?}", withdraw_req.amt), String::new());
			Ok(denial.into_response(StatusCode::PRECONDITION_FAILED))

		}

	} else {
		let denial = RequestDenial::new(DenialFault::User, String::from("Invalid username or cookie"), String::new());
		let json = simd_json::to_string(&denial).unwrap();

		Ok(Response::builder()
			.status(StatusCode::UNAUTHORIZED)
			.body(json)
			.unwrap())

	}

}

pub async fn update_cached_fee(cached_fee: Arc<CachedFee>, wallet: &Wallet) {
	loop {
		let fee = wallet.get_fee().await;

		if let Ok(fee) = fee {
			cached_fee.fee.store(fee, Ordering::Relaxed);
			//cached_fee.block_updated.store(height, Ordering::Relaxed);

		}

		tokio::time::sleep(Duration::from_secs(8)).await;
	}
}

pub async fn update_acc_balances(money_db: MoneyDB, wallet: &Wallet, all_transfers: Arc<RwLock<Vec<Transfers>>>) {
	const TIME_BETWEEN_UPDATES: Duration = Duration::from_secs(15);

	loop {
		let mut batch = sled::Batch::default();
        let indices: Vec<u64> = money_db.values().map(|f| f.unwrap().transfers.addr_index).collect();

        let all_balances = wallet.get_balance(&indices).await;
        let all_full_balances = wallet.get_all_balance(&indices).await;

		for res in money_db.sled_iter() {
			if let Ok((username_bytes, financial_info_bytes)) = res {
				let username: String = bincode::deserialize(&username_bytes).unwrap();
				let mut financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

				let all_transfers = all_transfers.read().await;
				
                let index_usize: usize = financial_info.transfers.addr_index.try_into().unwrap();

				financial_info.transfers = match all_transfers.get(index_usize.checked_sub(1).unwrap()) {
					Some(t) => t.clone(),
					None => financial_info.transfers,
				};


                if let Ok(all_balances) = &all_balances {
                    if let Some(balance) = all_balances.per_subaddress.iter().find(|b| b.addr_index == financial_info.transfers.addr_index) {
                        financial_info.balance = balance.balance;

                    } else {
                        eprintln!("Balance not found!");

                    }
               
                }

                if let Ok(all_full_balances) = &all_full_balances {
					if let Some(balance) = all_full_balances.per_subaddress.iter().find(|b| b.addr_index == financial_info.transfers.addr_index) {
                        financial_info.pending_balance = balance.balance - financial_info.balance;

                    } else {
                        eprintln!("All Balance not found!");

                    }     
                }

                batch.insert(bincode::serialize(&username).unwrap(), bincode::serialize(&financial_info).unwrap());

			} else {
				eprintln!("Error reading from DB");
				continue;

			}

		}


		money_db.apply_batch(batch).unwrap();
		tokio::time::sleep(TIME_BETWEEN_UPDATES).await;
	}
}

#[derive(Serialize, Deserialize)]
pub struct BlockchainTransfer {
	from: String,
	block_height: u64,
	amt: u64,
	tx_hash: String,
}


pub struct CachedFee {
	fee: AtomicU64,
	block_updated: AtomicU64,

}

impl CachedFee {
	pub const fn new() -> Self {
		Self {
			fee: AtomicU64::new(0),
			block_updated: AtomicU64::new(0),
		}
	}

	pub async fn get_fee(&self, wallet: &Wallet) -> u64 {
		let fee = self.fee.load(Ordering::Relaxed);

		// If the fee is 0, it hasn't yet been initialized
		match fee == 0 {
			true => {
				// If getting the fee fails, just fall back to an older known fee
                // Because monero transaction fees given by the daemon are per byte of the
                // transaction size, we just multiply by the average size of a monero transaction
                // 2KB
				wallet.get_fee().await.unwrap_or(4157) * 2_000

			},
			false => fee,

		}
	}

    pub async fn get_uptodate_fee(&self, wallet: &Wallet) -> Option<u64> {
        wallet.get_fee().await.ok().map(|fee_per_byte| fee_per_byte * 2_000)
    }
}

pub async fn update_transfers(wallet: &Wallet, all_transfers: Arc<RwLock<Vec<Transfers>>>, money_db: MoneyDB) {
	loop {
		let indices: Vec<u64> = money_db.values().map(|f| f.unwrap().transfers.addr_index).collect();

		// Done in its own block to drop the write lock more easily
		{
			let mut all_transfers = all_transfers.write().await;
			*all_transfers = match wallet.get_transfers(&indices, true, true).await {
				Ok(t) => t,
				Err(e) => {
					tokio::time::sleep(Duration::from_secs(30)).await;
					continue;
				},

			};
		}


		// Update the transfers of all users every 7 seconds
		tokio::time::sleep(Duration::from_secs(30)).await;
	}
}

#[derive(Debug)]
pub struct TransferReq {
	username: String,
	dst: Address,
	priority: Option<u8>, 
	amt: u64, 
	addr_index: u64,
}


pub async fn send_monero(wallet: &'static Wallet, mut monero_tx_req: UnboundedReceiver<TransferReq>, money_db: MoneyDB) {
	let pending_requests = Arc::new(Mutex::new(Vec::new()));
	let pending_requests_clone = pending_requests.clone();

	let money_db_clone = money_db.clone();

	let (a, b) = tokio::join!(
		tokio::task::spawn(async move {
			while let Some(req) = monero_tx_req.recv().await {
				let financial_info = money_db.get(&req.username).unwrap().unwrap();

				if financial_info.get_balance() >= req.amt {
					pending_requests_clone.clone().lock().push(req);

				}
			}

			panic!("This task shouldn't ever finish");
		}),
		tokio::task::spawn(async move {
			loop {
				// In order to not cause a deadlock, we first yield to the async executor
				tokio::task::yield_now().await;

				let money_db = money_db_clone.clone();

				// Since a lot of our mutations to the Vec depend on the indexes remaining the same, we keep a lock for the entirety of the loop iteration
				let pending_requests = &mut pending_requests.lock();
				let mut i = 0;

				loop {
					if i >= pending_requests.len() {
						break;
					}

					let req = pending_requests.get(i).unwrap();

					match wallet.transfer(req.priority, req.dst, req.amt, req.addr_index).await {
						Ok(out) => {
                            let mut financial_info = money_db.get(&req.username).unwrap().unwrap();

						    financial_info.service_fee_total += (req.amt + out.fee) / 49;
						    money_db.insert(&req.username, &financial_info).unwrap();
						    money_db.get_tree().flush_async().await.unwrap();

			    		    money_db.insert(&req.username, &financial_info).unwrap();
						    pending_requests.remove(i);
                        },
                        Err(_err) => {
                        	i += 1;
                        },

					}
				}

			}
		})
	);

	a.unwrap();
	b.unwrap();
}
