use std::time::SystemTime;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use argon2::{ThreadMode, Variant, Version};

use rand::prelude::*;
use rand::distributions::Alphanumeric;

use sled::Tree;
use serde::{Serialize, Deserialize};
use warp::{http::Response, hyper::StatusCode};

use crate::money::FinancialInfo;

pub(crate) const ARGON2_CONFIG: argon2::Config = argon2::Config {
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

#[derive(Serialize, Deserialize)]
pub struct AuthCookie {
    username: String,
    cookie: String,
    // Creation time in Unix time
    creation_time: u64,
    expiration_time: u64,

}

#[derive(Debug)]
enum GenAuthCookieErr {
	UserDoesntExist,
	DbErr(sled::Error),
	BincodeErr(bincode::Error)

}

impl From<sled::Error> for GenAuthCookieErr {
    fn from(err: sled::Error) -> Self {
        Self::DbErr(err)

    }
}

impl From<bincode::Error> for GenAuthCookieErr {
    fn from(err: bincode::Error) -> Self {
        Self::BincodeErr(err)

    }
}

pub(crate) fn register(user_info: AuthRequest, auth_db: Tree, money_db: Tree, current_payment_id: Arc<AtomicU64>) -> Response<String> {
    let username = user_info.username;
    let password = user_info.password;

    let username_bytes = bincode::serialize(&username).unwrap();

    let user_exists = auth_db.contains_key(&username_bytes).unwrap();

    match user_exists {
        true => {
        Response::builder()
            .status(StatusCode::CONFLICT)
            .body("Username already registered".to_string())
            .unwrap()

        },
        false => {
            if !username_is_valid(&username) {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body("Invalid username".to_string())
                    .unwrap();
            }

            let mut salt: [u8; 32] = [0; 32];
            rand::thread_rng().fill_bytes(&mut salt);

            let hashed_password = match argon2::hash_raw(password.as_bytes(), &salt, &ARGON2_CONFIG).ok() {
                Some(hash) => hash,
                None => return Response::builder()
                  .status(StatusCode::INTERNAL_SERVER_ERROR)
                  .body("Internal Server Error".to_string())
                  .unwrap()

            };

            let user = User {
                username,
                salt,
                hashed_password,
            };

            let user_bytes = bincode::serialize(&user).unwrap();

            auth_db.insert(&username_bytes, user_bytes).unwrap();
            money_db.insert(&username_bytes, bincode::serialize(&FinancialInfo::new(current_payment_id)).unwrap()).unwrap();

            Response::builder()
                .status(StatusCode::OK)
                .body("Added new user!".to_string())
                .unwrap()
        }

    }

}

pub(crate) fn login(user_info: AuthRequest, auth_db: Tree, auth_cookie_db: Tree) -> Response<String> {
    let username = user_info.username;
    let password = user_info.password;

    match auth_db.get(bincode::serialize(&username).unwrap()).unwrap() {
        Some(user_info_bytes) => {
            let user_info: User = bincode::deserialize(&user_info_bytes).unwrap();

            match argon2::verify_raw(password.as_bytes(), &user_info.salt, &user_info.hashed_password, &ARGON2_CONFIG).unwrap() {
                // Login successful
                true => {
                	let auth_cookie = generate_auth_cookie(&username, &auth_db, auth_cookie_db);
                	auth_cookie_result_to_response(username, &auth_cookie)

                },
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

fn generate_auth_cookie(username: &str, auth_db: &Tree, auth_cookie_db: Tree) -> Result<AuthCookie, GenAuthCookieErr> {
	let username_bytes = bincode::serialize(username).unwrap();

	if !auth_db.contains_key(&username_bytes)? {
		// This is a very bad error
		Err(GenAuthCookieErr::UserDoesntExist)

	} else {
		let cookie: String = thread_rng()
	        .sample_iter(&Alphanumeric)
	        .take(50)
	        .map(char::from)
	        .collect();


	    const NINETY_DAYS_AS_SECS: u64 = 7776000;
	    // Calling unwrap here since if SystemTime > UNIX_EPOCH, we have wayyyy bigger problems
	    let creation_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

	    let auth_cookie = AuthCookie {
	        username: username.to_string(),
	        cookie,
	        // The time in Unix time
	        creation_time,
	        // In case it isn't obvious, the auth cookie expires in 90 days
	        expiration_time: creation_time + NINETY_DAYS_AS_SECS,
	    };

	    let auth_cookie_bytes = bincode::serialize(&auth_cookie)?;
	    auth_cookie_db.insert(&username_bytes, auth_cookie_bytes)?;

	    Ok(auth_cookie)

	}

}

fn auth_cookie_result_to_response(username: String, auth_cookie: &Result<AuthCookie, GenAuthCookieErr>) -> Response<String> {
	Response::builder()
		.status(match auth_cookie.as_ref().err() {
			Some(err) => match err {
				GenAuthCookieErr::UserDoesntExist => StatusCode::UNAUTHORIZED,
				GenAuthCookieErr::DbErr(_) => StatusCode::INTERNAL_SERVER_ERROR,
				GenAuthCookieErr::BincodeErr(_) => StatusCode::INTERNAL_SERVER_ERROR,
			},
			None => StatusCode::OK,
		})
		.body(match auth_cookie {
			Ok(auth_cookie) => {
                let resp = LoginRequestResponse {
                    username,
                    cookie: auth_cookie.cookie.clone(),
                    exp_time: auth_cookie.expiration_time,
                };

                simd_json::to_string(&resp).unwrap()
            },
			Err(err) => format!("{err:?}"),
		})
		.unwrap()
}

pub(crate) fn destroy_expired_auth_cookies(auth_cookie_db: Tree) {
	loop {
		use std::thread::sleep;
		use std::time::Duration;

		let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

		// Loops through the auth cookie database and deletes any expired auth cookies, then waits a second
		auth_cookie_db.iter().for_each(|auth_cookie_key_val| {
			if let Ok((username, auth_cookie)) = auth_cookie_key_val {
				//let username: String = bincode::deserialize(&username).unwrap();
				let auth_cookie: AuthCookie = bincode::deserialize(&auth_cookie).unwrap();

				if current_time > auth_cookie.expiration_time {
					// The cookie has expired, so it must be removed
					auth_cookie_db.remove(username).unwrap_or_else(|error| {
                        eprintln!("Error reading auth_cookie db: {error:?}");
                        None
                    });
				}

			} else {
				eprintln!("Error reading auth_cookie db");

			}
		});

		sleep(Duration::from_secs(1));

	}

}

#[derive(Serialize)]
struct LoginRequestResponse {
    username: String,
    cookie: String,
    exp_time: u64,
}

pub(crate) fn verify_auth_cookie(username: &str, cookie: &str, auth_cookie_db: &Tree) -> bool {
	let username_bytes = bincode::serialize(username).unwrap();

	match auth_cookie_db.get(username_bytes).unwrap() {
		Some(auth_cookie_bytes) => {
			let auth_cookie: AuthCookie = bincode::deserialize(&auth_cookie_bytes).unwrap();
			auth_cookie.cookie.as_str() == cookie

		},
		None => false,
	}
	
}

fn username_is_valid(username: &str) -> bool {
    const INVALID_USERNAMES: [&str; 8] = ["admin", "root", "sudo", "bootlegbilly", "susorodni", "billyb2", "billyb", "billy"];

    username.chars().all(char::is_alphanumeric) && !INVALID_USERNAMES.contains(&username.to_lowercase().as_str())

}
