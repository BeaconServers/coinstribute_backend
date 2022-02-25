use crate::auth::verify_auth_cookie;

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use serde::{Serialize, Deserialize};
use sled::Tree;

use rayon::prelude::*;

use monero_wallet::{Address, Wallet, Payment, PaymentId};

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
	transfers_in: Vec<Payment>,
	transfers_out: Vec<Transfer>,
}

impl FinancialInfo {
	pub fn new(current_payment_id: Arc<AtomicU64>) -> Self {
		Self {
			xmr_addr: None,
			payment_id: current_payment_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst).to_be_bytes(),
			transfers_in: Vec::new(),
			transfers_out: Vec::new(),
		}
	}

	/// Adds all the transfers in and transfers out together
	fn get_balance(&self) -> u64 {
		let (transfers_in_total, transfers_out_total): (u64, u64) = rayon::join(
			|| self.transfers_in.par_iter().map(|p| p.amount).sum(),
			|| self.transfers_out.par_iter().map(|t| t.amt).sum(),

		);

		transfers_in_total.checked_sub(transfers_out_total).unwrap()

	}
}

pub(crate) async fn deposit_req(invoice_req: AuthorizedReq, auth_db: Tree, auth_cookie_db: Tree, money_db: Tree, wallet: &Wallet) -> Result<impl Reply, Rejection> {
	let invoice_req_username_bytes = bincode::serialize(&invoice_req.username).unwrap();

	if verify_auth_cookie(&invoice_req.username, &invoice_req.auth_cookie, &auth_cookie_db) && auth_db.contains_key(invoice_req_username_bytes).unwrap() {
		let username = &invoice_req.username;

		let payment_id = {
			let financial_info_bytes = money_db.get(bincode::serialize(username).unwrap()).unwrap().unwrap();
			let financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

			PaymentId::from_slice(&financial_info.payment_id)
		};

		let addr = wallet.new_integrated_addr(payment_id);
        let fee = wallet.get_fee().await.unwrap();

		Ok(Response::builder()
	        .status(StatusCode::OK)
	        .body(format!("Deposit to {addr} with at least {} Monero", (fee as f64 * 10.0) / 10.0_f64.powi(12) ))
	        .unwrap())
			
    } else {
    	Ok(Response::builder()
	        .status(StatusCode::UNAUTHORIZED)
	        .body("Invalid username or cookie".to_string())
	        .unwrap())

    }
}

pub async fn get_balance(auth_req: AuthorizedReq, auth_db: Tree, money_db: Tree, auth_cookie_db: Tree) -> Result<impl Reply, Rejection> {
    let username_bytes = bincode::serialize(&auth_req.username).unwrap();

    if verify_auth_cookie(&auth_req.username, &auth_req.auth_cookie, &auth_cookie_db) && auth_db.contains_key(&username_bytes).unwrap() {
        let financial_info_bytes = money_db.get(&username_bytes).unwrap().unwrap();
        let financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();
        let balance = financial_info.get_balance() as f64 / 10_u64.pow(12) as f64;

        Ok(Response::builder() 
            .status(StatusCode::UNAUTHORIZED)
            .body(balance.to_string())
            .unwrap())

    } else {
        Ok(Response::builder() 
              .status(StatusCode::UNAUTHORIZED)
              .body("Invalid username or cookie".to_string())
              .unwrap())
    }
}

pub(crate) fn attach_xmr_address(attach_req: AttachXMRAddress, money_db: Tree, auth_db: Tree, auth_cookie_db: Tree) -> Response<String> {
	let username_bytes = bincode::serialize(&attach_req.username).unwrap();

	if auth_db.contains_key(&username_bytes).unwrap() && verify_auth_cookie(&attach_req.username, &attach_req.auth_cookie, &auth_cookie_db) {
		let financial_info_bytes = money_db.get(&username_bytes).unwrap().unwrap();
		let mut financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

		let xmr_addr = match Address::from_str(&attach_req.monero_address) {
			Ok(monero_addr) => monero_addr,
			Err(_) => return Response::builder()
		        .status(StatusCode::BAD_REQUEST)
		        .body("Invalid Monero address".to_string())
		        .unwrap(),
		};

		financial_info.xmr_addr = Some(xmr_addr);

		Response::builder()
	        .status(StatusCode::OK)
	        .body("Set new address".to_string())
	        .unwrap()

	} else {
		Response::builder()
	        .status(StatusCode::UNAUTHORIZED)
	        .body("Invalid username or cookie".to_string())
	        .unwrap()

	}
}

pub async fn update_acc_balances(money_db: Tree, wallet: &Wallet) {
	const TIME_BETWEEN_UPDATES: Duration = Duration::from_secs(5);

	loop {
		let mut batch = sled::Batch::default();

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
					let payments = wallet.get_payments(payment_id).await.unwrap();


					if payments.len() != financial_info.transfers_in.len() {
						 financial_info.transfers_in = payments.par_iter().filter_map(|payment| {
							match payment.unlock_time == 0 {
								true => Some(payment),
								false => None,
							}
						}).cloned().collect();

						batch.insert(bincode::serialize(&username).unwrap(), bincode::serialize(&financial_info).unwrap());

					}

				},
				Err(e) => {
					eprintln!("Error when trying to access money_db: {e:?}");
				},
			}
		}

		// Apply all the transactions at once in one large batch
		money_db.apply_batch(batch).unwrap();

		tokio::time::sleep(TIME_BETWEEN_UPDATES).await;
	}
}

#[derive(Serialize, Deserialize)]
pub struct Transfer {
	from: String,
	block_height: u64,
	amt: u64,
	tx_hash: String,
}
