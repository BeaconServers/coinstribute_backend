mod auth;
mod money;

use std::convert::Infallible;
use std::error::Error;
use std::sync::{Arc, atomic::AtomicU64};

use monero_wallet::Wallet;
use once_cell::sync::{Lazy, OnceCell};
use serde_derive::Serialize;
use tokio::runtime::Runtime;
use warp::{Filter, Rejection, Reply};
use warp::http::StatusCode;

use auth::*;
use money::*;

static WALLET_RPC_ADDR: Lazy<String> = Lazy::new(|| String::from("http://127.0.0.1:19000"));
static DAEMON_ADDR: Lazy<String> = Lazy::new(|| String::from("http://node.moneroworld.com:18089"));

static WALLET: OnceCell<Wallet> = OnceCell::new();

fn main() {
    let tokio_rt = Arc::new(Runtime::new().unwrap());

    let db = sled::Config::default()
        .path("db")
        .mode(sled::Mode::HighThroughput)
        .open()
        .expect("Failed to open database");

    let current_payment_id: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    // Since stuff like the current Monero fee can take a long time to get from the wallet rpc, caching it is wise
    let cached_fee = Arc::new(CachedFee::new());

    let auth_db = db.open_tree(b"auth_db").expect("Failed to open authorization tree, time to panic!");
    let cookie_db = db.open_tree(b"cookie_db").expect("Failed to open cookie tree, time to panic!");
    let money_db = db.open_tree(b"money_db").expect("Failed to open money tree, oh no");

    let auth_db = warp::any().map(move || auth_db.clone());
    let cookie_db_filter = {
        let cookie_db = cookie_db.clone();
        warp::any().map(move || cookie_db.clone())
    }; 
    let money_db_filter = {
        let money_db_clone = money_db.clone();
        warp::any().map(move || money_db_clone.clone())
    };
    let wallet_filter = warp::any().map(wallet);
    let current_payment_id_filter = warp::any().map(move || current_payment_id.clone());
    let cached_fee_filter = {
        let cached_fee = cached_fee.clone();
        warp::any().map(move || cached_fee.clone())
    };

    let register = warp::path("signup")
        // 2 KB limit to username + password
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::json())
        .and(auth_db.clone())
        .and(money_db_filter.clone())
        .and(cookie_db_filter.clone())
        .and(current_payment_id_filter)
        .and_then(register);

    let login = warp::path("login")
        // 2 KB limit to username + password
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::json())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .and_then(login);

    let deposit_req = warp::path("deposit_req")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::json())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .and(money_db_filter.clone())
        .and(wallet_filter)
        .and(cached_fee_filter)
        .and_then(deposit_req);

    let get_balance = warp::path("get_balance")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::json())
        .and(auth_db.clone())
        .and(money_db_filter.clone())
        .and(cookie_db_filter.clone())
        .and_then(get_balance);

    let attach_xmr_address = warp::path("attach_xmr_address")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::json())
        .and(money_db_filter)
        .and(auth_db)
        .and(cookie_db_filter)
        .map(attach_xmr_address);

    let post_routes = warp::post()
        .and(
            register
            .or(login)
            .or(deposit_req)
            .or(attach_xmr_address)
            .or(get_balance)
        );


    tokio_rt.spawn(async move {
        let wallet = wallet();

        let mut set_settings = false;

        while !set_settings {
            set_settings = tokio::try_join!(
                wallet.set_daemon(&DAEMON_ADDR, DAEMON_ADDR.contains("127.0.0.1")),
                wallet.set_refresh_time(60),
            ).is_ok();
        }

        println!("Successfully configured wallet sync");

    });
    tokio_rt.spawn(update_acc_balances(money_db, wallet()));
    tokio_rt.spawn(update_cached_fee(cached_fee, wallet()));
    tokio_rt.spawn_blocking(move || destroy_expired_auth_cookies(cookie_db));

    tokio_rt.block_on(warp::serve(post_routes.recover(handle_rejection)).run(([127, 0, 0, 1], 3030)));

}

fn wallet() -> &'static Wallet {
    WALLET.get_or_init(|| {
        // Is it wasteful to spawn a whole tokio Runtime for a single function? Yes.
        // Should I TODO replace this with futures library? Also yes.
        let tokio_rt = Runtime::new().unwrap();

        println!("Initialized wallet");
        tokio_rt.block_on(Wallet::new(WALLET_RPC_ADDR.clone(), DAEMON_ADDR.clone(), None, None))
    })
}

#[derive(Serialize)]
struct ErrorMessage {
    code: u16,
    message: String,
}

async fn handle_rejection(err: Rejection) -> Result<impl Reply, Infallible> {
    let code;
    let message;

    if err.is_not_found() {
        code = StatusCode::NOT_FOUND;
        message = "NOT_FOUND";
    } else if let Some(e) = err.find::<warp::filters::body::BodyDeserializeError>() {
        // This error happens if the body could not be deserialized correctly
        // We can use the cause to analyze the error and customize the error message
        message = match e.source() {
            Some(cause) => {
                if cause.to_string().contains("denom") {
                    "FIELD_ERROR: denom"
                } else {
                    "BAD_REQUEST"
                }
            }
            None => "BAD_REQUEST",
        };
        code = StatusCode::BAD_REQUEST;
    } else if err.find::<warp::reject::MethodNotAllowed>().is_some() {
        // We can handle a specific error, here METHOD_NOT_ALLOWED,
        // and render it however we want
        code = StatusCode::METHOD_NOT_ALLOWED;
        message = "METHOD_NOT_ALLOWED";
    } else {
        // We should have expected this... Just log and say its a 500
        eprintln!("unhandled rejection: {:?}", err);
        code = StatusCode::INTERNAL_SERVER_ERROR;
        message = "UNHANDLED_REJECTION";
    }

    let json = warp::reply::json(&ErrorMessage {
        code: code.as_u16(),
        message: message.into(),
    });

    Ok(warp::reply::with_status(json, code))
}

enum DenialFault {
    Server,
    User,
    ServerAndUser,
    Unknown,
}

impl std::fmt::Display for DenialFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            DenialFault::Server => "Server",
            DenialFault::User => "User",
            DenialFault::ServerAndUser => "Server and User",
            DenialFault::Unknown => "Unknown",
        })
    }
}

#[derive(Serialize)]
struct RequestDenial {
    fault: String,
    reason: String,
    additional_info: String,
}

impl RequestDenial {
    fn new(fault: DenialFault, reason: String, additional_info: String) -> Self {
        Self {
            fault: fault.to_string(),
            reason,
            additional_info,
        }
    }
}