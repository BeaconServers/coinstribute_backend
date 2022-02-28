use crate::{AuthDB, CookieDB, MoneyDB, RequestDenial, DenialFault};
use crate::auth::verify_auth_cookie;
use crate::fundraiser::Fundraiser;

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Serialize, Deserialize};
use sled::Tree;

use rayon::prelude::*;

use monero_wallet::{Address, Wallet, PaymentId, Payment};

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

#[derive(Serialize, Deserialize)]
pub(crate) struct FinancialInfo {
	xmr_addr: Option<Address>,
	// The amount of monero in piconeros
	payment_id: [u8; 8],
	deposits: Vec<Payment>,
	active_fundraisers: Vec<Fundraiser>,
	old_fundraisers: Vec<Fundraiser>,
	transfers_out: Vec<BlockchainTransfer>,
}

impl FinancialInfo {
	pub fn new(current_payment_id: Arc<AtomicU64>, current_payment_id_db: Tree) -> Self {
		let new = Self {
			xmr_addr: None,
			payment_id: current_payment_id.fetch_add(1, Ordering::SeqCst).to_be_bytes(),
			active_fundraisers: Vec::new(),
			old_fundraisers: Vec::new(),
			transfers_out: Vec::new(),
			deposits: Vec::new(),
		};

		current_payment_id_db.insert(b"current_id", &(u64::from_be_bytes(new.payment_id) + 1).to_be_bytes()).unwrap();
		current_payment_id_db.flush().unwrap();

		new

	}

	/// Adds all the transfers in and transfers out together
	fn get_balance(&self) -> u64 {
		let active_fund_total: u64 = self.active_fundraisers.iter().map(|f| f.amt_earned()).sum();
		let past_fund_total: u64 = self.old_fundraisers.iter().map(|f| f.amt_earned()).sum();
		let deposits_total: u64 = self.deposits.iter().map(|p| p.amount).sum(); 

		let transfers_out_total: u64 = self.transfers_out.iter().map(|p| p.amt).sum();

		(active_fund_total + deposits_total + past_fund_total).checked_sub(transfers_out_total).unwrap()

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
	let invoice_req_username_bytes = bincode::serialize(&invoice_req.username).unwrap();

	let (code, json_resp): (StatusCode, Box<dyn erased_serde::Serialize + Send>) = if verify_auth_cookie(&invoice_req.username, &invoice_req.auth_cookie, &auth_cookie_db) && auth_db.contains_key(invoice_req_username_bytes).unwrap() {
		let username = &invoice_req.username;

		let payment_id = {
			let financial_info_bytes = money_db.get(bincode::serialize(username).unwrap()).unwrap().unwrap();
			let financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

			PaymentId::from_slice(&financial_info.payment_id)
		};

		let addr = wallet.new_integrated_addr(payment_id);
        let fee = cached_fee.get_fee(wallet).await;


		(StatusCode::OK, Box::new(DepositReqResp {
			addr: addr.to_string(),
			min_amt: fee * 10,
		}))

    } else {
		(StatusCode::UNAUTHORIZED, Box::new(RequestDenial::new(
		DenialFault::User,
		"Invalid username or cookie".to_string(),
String::new(),
		)))

    };

	Ok(Response::builder()
		.status(code)
		.body(simd_json::to_string(&json_resp).unwrap())
		.unwrap())

}

#[derive(Serialize)]
struct BalanceReqResp {
	balance: u64,
}

pub async fn get_balance(auth_req: AuthorizedReq, auth_db: AuthDB, money_db: MoneyDB, auth_cookie_db: CookieDB) -> Result<impl Reply, Rejection> {
    let username_bytes = bincode::serialize(&auth_req.username).unwrap();

    let (code, json_resp): (StatusCode, Box<dyn erased_serde::Serialize>) = if verify_auth_cookie(&auth_req.username, &auth_req.auth_cookie, &auth_cookie_db) && auth_db.contains_key(&username_bytes).unwrap() {
        let financial_info_bytes = money_db.get(&username_bytes).unwrap().unwrap();
        let financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

        (StatusCode::OK, Box::new(BalanceReqResp {
			balance: financial_info.get_balance()

		}))

    } else {
		(StatusCode::UNAUTHORIZED, Box::new(RequestDenial::new(
		DenialFault::User,
		"Invalid username or cookie".to_string(),
String::new(),
		)))
    };

	Ok(Response::builder()
		.status(code)
		.body(simd_json::to_string(json_resp.as_ref()).unwrap())
		.unwrap())
}

pub(crate) fn attach_xmr_address(attach_req: AttachXMRAddress, money_db: MoneyDB, auth_db: AuthDB, auth_cookie_db: CookieDB) -> Response<String> {
	let username_bytes = bincode::serialize(&attach_req.username).unwrap();

	if auth_db.contains_key(&username_bytes).unwrap() && verify_auth_cookie(&attach_req.username, &attach_req.auth_cookie, &auth_cookie_db) {
		let financial_info_bytes = money_db.get(&username_bytes).unwrap().unwrap();
		let mut financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

		let xmr_addr = match Address::from_str(&attach_req.monero_address) {
			Ok(monero_addr) => monero_addr,
			Err(_) => {
				let resp = RequestDenial::new(
				DenialFault::User,
				"Invalid address".to_string(),
		String::new(),
				);

				return Response::builder()
		        	.status(StatusCode::BAD_REQUEST)
		        	.body(simd_json::to_string(&resp).unwrap())
		        	.unwrap();
			},
		};

		financial_info.xmr_addr = Some(xmr_addr);

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

		Response::builder()
			.status(StatusCode::UNAUTHORIZED)
			.body(simd_json::to_string(&resp).unwrap())
			.unwrap()

	}
}

pub async fn update_cached_fee(cached_fee: Arc<CachedFee>, wallet: &Wallet) {
	loop {
		let fee = wallet.get_fee().await;

		if let Ok(fee) = fee {
			cached_fee.fee.store(fee, Ordering::Relaxed);
			//cached_fee.block_updated.store(height, Ordering::Relaxed);

		}

		tokio::time::sleep(Duration::from_secs(5)).await;
	}
}

pub async fn update_acc_balances(money_db: Tree, wallet: &Wallet) {
	const TIME_BETWEEN_UPDATES: Duration = Duration::from_secs(5);

	loop {
		let mut batch = sled::Batch::default();

		let mut num_of_errors: u8 = 0;

		// Updates the amt of money for all users 
		for db_entry in money_db.iter() {
			match &db_entry {
				// TODO: Just use an Arc to get rid of all the cloning
				Ok(entry) => {
					let (username, mut financial_info): (String, FinancialInfo) = (
						bincode::deserialize(&entry.0).unwrap(), 
						bincode::deserialize(&entry.1).unwrap(),
					);

					let payment_id = PaymentId::from_slice(&financial_info.payment_id);

					let payments = match wallet.get_payments(payment_id).await {
						Ok(payments) => payments,
						Err(_err) => {
							num_of_errors += 1;

							if num_of_errors >= 5 {
								eprintln!("Over 5 get_payments errors!")

							}

							continue;
						},

					};

					if payments.len() != financial_info.deposits.len() {
						 financial_info.deposits = payments.par_iter().filter_map(|payment| {
							match payment.unlock_time == 0 {
								true => Some(payment),
								false => None,
							}
						}).cloned().collect();

						let username_bin = match bincode::serialize(&username) {
							Ok(bin) => bin,
							Err(e) => {
								eprintln!("Could not serialize username due to error: {e:?}, this is very bad news!!!");
								continue;
							}
						};

						let financial_info_bin = match bincode::serialize(&financial_info) {
							Ok(bin) => bin,
							Err(e) => {
								eprintln!("Could not serialize financial_info due to error: {e:?}, this is very bad news!!!");
								continue;
							}
						};

						batch.insert(username_bin, financial_info_bin);

					}

				},
				Err(e) => {
					eprintln!("Error when trying to access money_db: {e:?}");
				},
			}

			// Yields to the tokio Runtime between every update so that the server isn't blocking other async tasks
			tokio::task::yield_now().await;
		}

		// Apply all the transactions at once in one large batch
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
				wallet.get_fee().await.unwrap_or(3681250)

			},
			false => fee,

		}
	}
}