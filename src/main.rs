mod auth;
mod money;

use std::sync::Arc;
use std::time::Duration;

use argon2::{ThreadMode, Variant, Version};
use acceptxmr::PaymentGateway;

use serde_derive::{Serialize, Deserialize};
use sled::Tree;

use rand::prelude::*;

use tokio::runtime::Runtime;

use warp::http::{Response, StatusCode};
use warp::Filter;

use money::{new_invoice, invoice_status};

const ARGON2_CONFIG: argon2::Config = argon2::Config {
    ad: &[],
    hash_length: 32,
    lanes: 4,
    mem_cost: 4096,
    secret: &[],
    thread_mode: ThreadMode::Parallel,
    time_cost: 3,
    variant: Variant::Argon2i,
    version: Version::Version13,
};

const PRIVATE_VIEW_KEY: &'static str = "ad2093a5705b9f33e6f0f0c1bc1f5f639c756cdfc168c8f2ac6127ccbdab3a03";
const PUBLIC_SPEND_KEY: &'static str = "7388a06bd5455b793a82b90ae801efb9cc0da7156df8af1d5800e4315cc627b4";

#[derive(Deserialize)]
pub struct AuthRequest {
    username: String,
    password: String,

}

#[derive(Serialize, Deserialize)]
pub struct User {
    username: String,
    salt: [u8; 32],
    hashed_password: Vec<u8>,

}

fn main() {
    let tokio_rt = Arc::new(Runtime::new().unwrap());

    let db = sled::Config::default()
        .path("db")
        .mode(sled::Mode::HighThroughput)
        .use_compression(true)
        .open()
        .expect("Failed to open database");

    let payment_gateway = Arc::new(PaymentGateway::builder(PRIVATE_VIEW_KEY, PUBLIC_SPEND_KEY)
        .scan_interval(Duration::from_millis(100))
        .build());

    let auth_db = db.open_tree(b"auth_db").expect("Failed to open authorization tree, time to panic!");

    let invoice_db = db.open_tree(b"invoice_db").expect("Failed to open invoice tree, time to panic!");

    let auth_db = warp::any().map(move || auth_db.clone());
    let invoice_db = warp::any().map(move || invoice_db.clone());

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
        .map(login);

    let new_invoice = warp::path("new_invoice")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(invoice_db.clone())
        .and(auth_db.clone())
        .and(payment_gateway_filter.clone())
        .and_then(new_invoice);

    let invoice_status = warp::path("invoice_status")
        .and(warp::body::content_length_limit(2048))
        .and(warp::body::form())
        .and(invoice_db.clone())
        .and(auth_db.clone())
        .and(payment_gateway_filter.clone())
        .and_then(invoice_status);

    let post_routes = warp::post()
        .and(
            register
            .or(login)
            .or(new_invoice)
            .or(invoice_status)
        );

    // All get requests should be compressed


    let payment_gateway = payment_gateway.clone();

    tokio_rt.spawn(async move { payment_gateway.run().await });
    tokio_rt.block_on(warp::serve(post_routes).run(([127, 0, 0, 1], 3030)));

}

fn register(user_info: AuthRequest, auth_db: Tree) -> Response<String> {
    let username = user_info.username;
    let password = user_info.password;

    let user_exists = auth_db.contains_key(username.as_bytes()).unwrap();

    match user_exists {
        true => {
        Response::builder()
            .status(StatusCode::CONFLICT)
            .body("Username already registered".to_string())
            .unwrap()

        },
        false => {
            let mut salt: [u8; 32] = [0; 32];
            rand::thread_rng().fill_bytes(&mut salt);

            let hashed_password = match argon2::hash_raw(password.as_bytes(), &salt, &ARGON2_CONFIG).ok() {
                Some(hash) => hash,
                None => return Response::builder()
                  .status(StatusCode::INTERNAL_SERVER_ERROR)
                  .body("".to_string())
                  .unwrap()

            };

            let user = User {
                username,
                salt,
                hashed_password,
            };

            let user_bytes = bincode::serialize(&user).unwrap();

            auth_db.insert(user.username.as_bytes(), user_bytes).unwrap();

            Response::builder()
                .status(StatusCode::OK)
                .body("Added new user!".to_string())
                .unwrap()
        }

    }

}

fn login(user_info: AuthRequest, auth_db: Tree) -> Response<String> {
    let username = user_info.username;
    let password = user_info.password;

    match auth_db.get(username.as_bytes()).unwrap() {
        Some(user_info_bytes) => {
            let user_info: User = bincode::deserialize(&user_info_bytes).unwrap();

            match argon2::verify_raw(password.as_bytes(), &user_info.salt, &user_info.hashed_password, &ARGON2_CONFIG).unwrap() {
                // Login successful
                true => Response::builder()
                    .status(StatusCode::OK)
                    .body(String::from("Login sucessful"))
                    .unwrap(),

                false => Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body("Wrong username or password".to_string())
                    .unwrap()

            }

        },
        None => Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body("Wrong username or password".to_string())
                .unwrap()
    }

}

