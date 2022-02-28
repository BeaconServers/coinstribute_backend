use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{DenialFault, RequestDenial};
use crate::money::FinancialInfo;
use crate::auth::verify_auth_cookie;

use serde_derive::{Serialize, Deserialize};
use sled::Tree;

use warp::http::Response;
use warp::hyper::StatusCode;

#[derive(Serialize, Deserialize)]
struct InternalTransfer {
    origin: String,
    receiver_fundraiser: u64,
    transfer_time: u64,
    amt: u64,
}

#[derive(Deserialize)]
pub struct CreateFundraiserReq {
    username: String,
    auth_cookie: String,
    fundraiser_name: String,
    goal: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Fundraiser {
    name: String,
    owner: String,
    goal: u64,
    fundraiser_id: u64,
    payments: Vec<InternalTransfer>,
}

impl Fundraiser {
    pub fn amt_earned(&self) -> u64 {
        self.payments.iter().map(|p| p.amt).sum()
    }
}

pub fn create_fundraiser(create_fund_req: CreateFundraiserReq, auth_db: Tree, auth_cookie_db: Tree, fundraiser_db: Tree, money_db: Tree, current_fundraiser_id_db: Tree, current_fundraiser_id: Arc<AtomicU64>) -> Response<String> {
    let username_bytes = bincode::serialize(&create_fund_req.username).unwrap();

    if auth_db.contains_key(&username_bytes).unwrap() && verify_auth_cookie(&create_fund_req.username, &create_fund_req.auth_cookie, &auth_cookie_db) {
        let fundraiser = Fundraiser {
            name: create_fund_req.fundraiser_name,
            owner: create_fund_req.username,
            fundraiser_id: current_fundraiser_id.fetch_add(1, Ordering::SeqCst),
            goal: create_fund_req.goal,
            payments: Vec::new(),
        };

        let fundraiser_json = simd_json::to_string(&fundraiser).unwrap();
        let fundraiser_bytes = bincode::serialize(&fundraiser).unwrap();

        let financial_info_bytes = money_db.get(&username_bytes).unwrap().unwrap();
        let mut financial_info: FinancialInfo = bincode::deserialize(&financial_info_bytes).unwrap();

        fundraiser_db.insert(&fundraiser_bytes, &[]).unwrap();

        current_fundraiser_id_db.insert(b"current_id", &(fundraiser.fundraiser_id + 1).to_be_bytes()).unwrap();
        current_fundraiser_id_db.flush().unwrap();

        financial_info.add_fundraiser(fundraiser);

        money_db.insert(&username_bytes, bincode::serialize(&financial_info).unwrap()).unwrap();

        Response::builder()
            .status(StatusCode::OK)
            .body(fundraiser_json)
            .unwrap()

    } else {
        let denial = RequestDenial::new(DenialFault::User, "Invalid username or cookie".to_string(), String::new());

        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(simd_json::to_string(&denial).unwrap())
            .unwrap()

    }

}