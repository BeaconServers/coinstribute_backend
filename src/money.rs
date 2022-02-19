use crate::auth::verify_auth_cookie;

use std::sync::Arc;

use acceptxmr::{InvoiceId, PaymentGateway};

use serde::Deserialize;
use sled::Tree;

use warp::{Rejection, Reply};
use warp::{http::Response, hyper::StatusCode};

#[derive(Deserialize)]
pub(crate) struct InvoiceReq {
	username: String,
	/// The invoice amount in piconeros (10^-12 Monero)
	deposit_amt: u64,
	auth_cookie: String,
}

#[derive(Deserialize)]
pub(crate) struct ViewInvoiceReq {
	username: String,
	auth_cookie: String,
}

pub(crate) async fn new_invoice(invoice_req: InvoiceReq, invoice_db: Tree, auth_db: Tree, gateway: Arc<PaymentGateway>) -> Result<impl Reply, Rejection> {
	let invoice_req_username_bytes = invoice_req.username.as_bytes();

	if verify_auth_cookie(invoice_req.auth_cookie, &auth_db) && auth_db.contains_key(invoice_req_username_bytes).unwrap() {
		if invoice_db.contains_key(invoice_req_username_bytes).unwrap() {
			Ok(Response::builder()
		        .status(StatusCode::PRECONDITION_FAILED)
		        .body("You already have an invoice".to_string())
		        .unwrap())

		} else {
			// Invoice amount must be > 0.001 Monero
			if invoice_req.deposit_amt > 1000000000 {
				// Require 10 confirmations on the Monero network before accepting deposits
				const REQ_NUM_OF_CONFIRMATIONS: u64 = 10;
				// Expires in 35 blocks (about an hour)
				const BLOCK_EXPIRE_TIME: u64 = 35;

				let username = &invoice_req.username;

				let invoice_id = 
					gateway.new_invoice(
						invoice_req.deposit_amt, 
						REQ_NUM_OF_CONFIRMATIONS, 
						BLOCK_EXPIRE_TIME, 
						&format!("For deposit on {username} account")
					).await.unwrap();

				let invoice_id_bin = bincode::serialize(&invoice_id).unwrap();

				invoice_db.insert(invoice_req_username_bytes, invoice_id_bin).unwrap();

				Ok(Response::builder()
			        .status(StatusCode::OK)
			        .body("Created invoice".to_string())
			        .unwrap())


			} else {
				Ok(Response::builder()
			        .status(StatusCode::PRECONDITION_FAILED)
			        .body("Invalid invoice! 0.001 Monero or greater required".to_string())
			        .unwrap())
			

			}
			
		}

    } else {
    	Ok(Response::builder()
	        .status(StatusCode::UNAUTHORIZED)
	        .body("Bad cookie".to_string())
	        .unwrap())

    }
}

pub(crate) async fn invoice_status(status_req: ViewInvoiceReq, invoice_db: Tree, auth_db: Tree, gateway: Arc<PaymentGateway>) -> Result<impl Reply, Rejection> {
	let username_bytes = status_req.username.as_bytes();

	if !auth_db.contains_key(username_bytes).unwrap() || !verify_auth_cookie(status_req.auth_cookie, &auth_db) {
		return 
			Ok(Response::builder()
		        .status(StatusCode::UNAUTHORIZED)
		        .body("Invalid username or cookie".to_string())
		        .unwrap());
	}

	match invoice_db.get(username_bytes).unwrap() {
		Some(invoice_id) => {
			let invoice_id: InvoiceId = bincode::deserialize(&invoice_id).unwrap();
			let invoice = gateway.get_invoice(invoice_id).unwrap().unwrap();

			Ok(Response::builder()
		        .status(StatusCode::OK)
		        .body(format!("{invoice:?}"))
		        .unwrap())

		},
		None => Ok(Response::builder()
		        .status(StatusCode::OK)
		        .body("No invoice found for user".to_string())
		        .unwrap())
	}

}