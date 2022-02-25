mod auth;
mod money;

use std::sync::{Arc, atomic::AtomicU64};

use monero_wallet::Wallet;
use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;
use warp::Filter;

use auth::*;
use money::*;

static WALLET: OnceCell<Wallet> = OnceCell::new();

fn main() {
    let tokio_rt = Arc::new(Runtime::new().unwrap());

    let db = sled::Config::default()
        .path("db")
        .mode(sled::Mode::HighThroughput)
        .open()
        .expect("Failed to open database");

    let current_payment_id: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

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
    let wallet_filter = warp::any().map(move || wallet());
    let current_payment_id_filter = warp::any().map(move || current_payment_id.clone());

    let register = warp::path("signup")
        // 2 KB limit to username + password
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(auth_db.clone())
        .and(money_db_filter.clone())
        .and(current_payment_id_filter.clone())
        .map(register);

    let login = warp::path("login")
        // 2 KB limit to username + password
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .map(login);

    let deposit_req = warp::path("deposit_req")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .and(money_db_filter.clone())
        .and(wallet_filter.clone())
        .and_then(deposit_req);

    let get_balance = warp::path("get_balance")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(auth_db.clone())
        .and(money_db_filter.clone())
        .and(cookie_db_filter.clone())
        .and_then(get_balance);

    let attach_xmr_address = warp::path("attach_xmr_address")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(money_db_filter.clone())
        .and(auth_db.clone())
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


    let cookie_db = cookie_db.clone();

    tokio_rt.spawn(update_acc_balances(money_db.clone(), wallet()));
    tokio_rt.spawn_blocking(move || destroy_expired_auth_cookies(cookie_db.clone()));

    tokio_rt.block_on(warp::serve(post_routes).run(([127, 0, 0, 1], 3030)));

}

fn wallet() -> &'static Wallet {
    WALLET.get_or_init(|| {
        let tokio_rt = Runtime::new().unwrap();

        let wallet_rpc_addr = String::from("http://127.0.0.1:19000");
        let daemon_rpc_addr = String::from("http://node.moneroworld.com:18089");

        tokio_rt.block_on(Wallet::new(wallet_rpc_addr, daemon_rpc_addr, None, None))

    })
}
