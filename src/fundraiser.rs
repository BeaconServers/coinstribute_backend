use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{DenialFault, RequestDenial};
use crate::db::{AuthDB, CookieDB, FundraiserDB, MoneyDB, DB, CurrentFundraiserIdDB};
use crate::auth::verify_auth_cookie;

use rayon::prelude::*;
use serde_derive::{Serialize, Deserialize};

use warp::http::Response;
use warp::hyper::StatusCode;

#[derive(Clone, Serialize, Deserialize)]
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
    purpose: String,
    goal: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Fundraiser {
    name: String,
    owner: String,
    goal: u64,
    fundraiser_id: u64,
    purpose: String,
    payments: Vec<InternalTransfer>,
}

impl Fundraiser {
    pub fn amt_earned(&self) -> u64 {
        self.payments.iter().map(|p| p.amt).sum()
    }

    fn from_req(req: &CreateFundraiserReq, current_fundraiser_id: Arc<AtomicU64>) -> Self {
        Self {
            name: req.fundraiser_name.clone(),
            owner: req.username.clone(),
            fundraiser_id: current_fundraiser_id.fetch_add(1, Ordering::SeqCst),
            goal: req.goal,
            purpose: req.purpose.clone(),
            payments: Vec::new(),
        }
    }
}

pub fn create_fundraiser(create_fund_req: CreateFundraiserReq, auth_db: AuthDB, auth_cookie_db: CookieDB, fundraiser_db: FundraiserDB, money_db: MoneyDB, current_fundraiser_id_db: CurrentFundraiserIdDB, current_fundraiser_id: Arc<AtomicU64>) -> Response<String> {
    if auth_db.contains_key(&create_fund_req.username).unwrap() && verify_auth_cookie(&create_fund_req.username, &create_fund_req.auth_cookie, &auth_cookie_db) {
        let fundraiser = Fundraiser::from_req(&create_fund_req, current_fundraiser_id);

        if fundraiser_db.contains_key(&fundraiser.name).unwrap() {
            let denial = RequestDenial::new(DenialFault::User, String::from("The given fundraiser name already exists"), String::from("Please pick a different name for your fundraiser"));
            let json = simd_json::to_string(&denial).unwrap();

            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(json)
                .unwrap()

        } else {
            let mut financial_info = money_db.get(&create_fund_req.username).unwrap().unwrap();

            if financial_info.active_fundraisers().len() >= 5 {
                let denial = RequestDenial::new(
                DenialFault::User, 
                String::from("You can only have a maximum of 5 fundraisers"), 
        String::from("Please delete or archive an existing fundraiser or wait for it to reach its goal")
                );

                let json = simd_json::to_string(&denial).unwrap();

                Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(json)
                    .unwrap()

            } else {
                let fundraiser_json = simd_json::to_string(&fundraiser).unwrap();

                fundraiser_db.insert(&fundraiser.name, &fundraiser).unwrap();

                current_fundraiser_id_db.insert(&"current_id".to_string(), &(fundraiser.fundraiser_id + 1)).unwrap();
                current_fundraiser_id_db.flush().unwrap();

                financial_info.add_fundraiser(fundraiser);

                money_db.insert(&create_fund_req.username, &financial_info).unwrap();

                Response::builder()
                    .status(StatusCode::OK)
                    .body(fundraiser_json)
                    .unwrap()
            }
        }

    } else {
        let denial = RequestDenial::new(DenialFault::User, "Invalid username or cookie".to_string(), String::new());

        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(simd_json::to_string(&denial).unwrap())
            .unwrap()

    }

}

#[derive(Deserialize)]
pub struct SearchFundraisersReq {
    query: String,
    max_num_results: u64,
    order: SearchFundraiserOrder,
}

#[derive(Deserialize)]
enum SearchFundraiserOrder {
    Alphabetical,
    MostFunded,
    LeastFunded,
    LowestGoal,
    HighestGoal,
}

#[derive(Serialize)]
struct SearchFundraiserRes {
    name: String,
    purpose: String,
    owner: String,
    goal: u64,
    amt_earned: u64,
}

impl From<Fundraiser> for SearchFundraiserRes {
    fn from(fundraiser: Fundraiser) -> Self {
        Self {
            amt_earned: fundraiser.amt_earned(),
            name: fundraiser.name,
            purpose: fundraiser.purpose,
            owner: fundraiser.owner,
            goal: fundraiser.goal,
        }
    }
}

pub fn search_fundraisers(search_req: SearchFundraisersReq, fundraiser_db: FundraiserDB) -> Response<String> {    
    let query_lowercase = search_req.query.to_lowercase();

    if search_req.max_num_results > 100 {
        let denial = RequestDenial::new(
            DenialFault::User, 
            String::from("We can only return a maximum of 100 results at a time"),
            String::from("Please request a lower number"),
        );

        let json = simd_json::to_string(&denial).unwrap();

        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(json)
            .unwrap();
    }

    let max_num_results: usize = search_req.max_num_results.try_into().unwrap();

    let mut results: Vec<SearchFundraiserRes> = Vec::with_capacity(max_num_results.min(fundraiser_db.len()));

    for res in fundraiser_db.iter() {
        if results.len() >= max_num_results {
            break;

        }

        let (fundraiser_name_lowercase, fundraiser) = res.unwrap();

        if fundraiser_name_lowercase.contains(&query_lowercase) || fundraiser.purpose.to_lowercase().contains(&query_lowercase) || fundraiser.owner.to_lowercase().contains(&query_lowercase) {
            results.push(fundraiser.into());

        }
    }

    let alphabetical_sort = |first_res: &SearchFundraiserRes, second_res: &SearchFundraiserRes| -> std::cmp::Ordering {
        first_res.name.partial_cmp(&second_res.name).unwrap_or(std::cmp::Ordering::Equal)

    };

    let least_funded_sort = |first_res: &SearchFundraiserRes, second_res: &SearchFundraiserRes| -> std::cmp::Ordering {
        first_res.amt_earned.cmp(&second_res.amt_earned)

    };

    let most_funded_sort = |first_res: &SearchFundraiserRes, second_res: &SearchFundraiserRes| -> std::cmp::Ordering {
        match first_res.amt_earned.cmp(&second_res.amt_earned) {
            std::cmp::Ordering::Less => std::cmp::Ordering::Greater,
            std::cmp::Ordering::Greater => std::cmp::Ordering::Less,
            std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
        }

    };

    let lowest_goal_sort = |first_res: &SearchFundraiserRes, second_res: &SearchFundraiserRes| -> std::cmp::Ordering {
        first_res.goal.cmp(&second_res.goal)

    };

    let highest_goal_sort = |first_res: &SearchFundraiserRes, second_res: &SearchFundraiserRes| -> std::cmp::Ordering {
        match first_res.goal.cmp(&second_res.goal) {
            std::cmp::Ordering::Less => std::cmp::Ordering::Greater,
            std::cmp::Ordering::Greater => std::cmp::Ordering::Less,
            std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
        }

    };
    
    results.as_parallel_slice_mut().par_sort_unstable_by(match search_req.order {
        SearchFundraiserOrder::Alphabetical => alphabetical_sort,
        SearchFundraiserOrder::LeastFunded => least_funded_sort,
        SearchFundraiserOrder::MostFunded => most_funded_sort,
        SearchFundraiserOrder::LowestGoal => lowest_goal_sort,
        SearchFundraiserOrder::HighestGoal => highest_goal_sort,
    });

    let json = simd_json::to_string(&results).unwrap();

    Response::builder()
        .status(StatusCode::OK)
        .body(json)
        .unwrap()

}

