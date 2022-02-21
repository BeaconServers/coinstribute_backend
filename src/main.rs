mod auth;
mod money;

use std::sync::Arc;
use std::time::Duration;

use acceptxmr::PaymentGateway;
use tokio::runtime::Runtime;
use warp::Filter;

use auth::*;
use money::*;

const PRIVATE_VIEW_KEY: &'static str = "5cbf005b48eb14e0fe6cbe9337cd29570c60651200336cc6f3982cfba2dbe40c";
const PUBLIC_SPEND_KEY: &'static str = "c1cc28cd4703b969478438332f890f559331bfedaf2ccc53258df2ca7c9ed269";


fn main() {
    let tokio_rt = Arc::new(Runtime::new().unwrap());

    let db = sled::Config::default()
        .path("db")
        .mode(sled::Mode::HighThroughput)
        .open()
        .expect("Failed to open database");

    let payment_gateway = Arc::new(PaymentGateway::builder(PRIVATE_VIEW_KEY, PUBLIC_SPEND_KEY)
        .scan_interval(Duration::from_millis(100))
        .build());

    let auth_db = db.open_tree(b"auth_db").expect("Failed to open authorization tree, time to panic!");
    let invoice_db = db.open_tree(b"invoice_db").expect("Failed to open invoice tree, time to panic!");
    let cookie_db = db.open_tree(b"cookie_db").expect("Failed to open cookie tree, time to panic!");

    let auth_db = warp::any().map(move || auth_db.clone());
    let invoice_db = warp::any().map(move || invoice_db.clone());
    let cookie_db_filter = {
        let cookie_db = cookie_db.clone();
        warp::any().map(move || cookie_db.clone())
    };

    let payment_gateway_clone = payment_gateway.clone();
    let payment_gateway_filter = warp::any().map(move || payment_gateway_clone.clone());

    let register = warp::path("signup")
        // 2 KB limit to username + password
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(auth_db.clone())
        .map(register);

    let login = warp::path("login")
        // 2 KB limit to username + password
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .map(login);

    let new_invoice = warp::path("new_invoice")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(invoice_db.clone())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .and(payment_gateway_filter.clone())
        .and_then(new_invoice);

    let invoice_status = warp::path("invoice_status")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(invoice_db.clone())
        .and(auth_db.clone())
        .and(cookie_db_filter.clone())
        .and(payment_gateway_filter.clone())
        .map(invoice_status);

    let post_routes = warp::post()
        .and(
            register
            .or(login)
            .or(new_invoice)
            .or(invoice_status)
        );

    // All get requests should be compressed


    let payment_gateway = payment_gateway.clone();
    let cookie_db = cookie_db.clone();

    tokio_rt.spawn(async move { payment_gateway.run().await });
    tokio_rt.spawn_blocking(move || destroy_expired_auth_cookies(cookie_db.clone()));
    tokio_rt.block_on(warp::serve(post_routes).run(([127, 0, 0, 1], 3030)));

}

