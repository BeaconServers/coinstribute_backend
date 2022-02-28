use std::fmt::Display;
use std::time::{SystemTime, Duration};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use argon2::{ThreadMode, Variant, Version};

use rand::prelude::*;
use rand::distributions::Alphanumeric;

use once_cell::sync::Lazy;

use sled::Tree;
use serde::{Serialize, Deserialize};
use warp::reply::Reply;
use warp::{http::Response, hyper::StatusCode};
use warp::Rejection;

use crate::{DenialFault, RequestDenial};
use crate::money::FinancialInfo;

pub(crate) static ARGON2_CONFIG: Lazy<argon2::Config> = Lazy::new(|| argon2::Config {
    ad: &[],
    hash_length: 32,
    lanes: {
       let num_threads: usize = std::thread::available_parallelism().unwrap().into();
       let num_threads: u32 = num_threads.try_into().unwrap();

       num_threads
    },
    mem_cost: 4096,
    secret: &[],
    thread_mode: ThreadMode::Parallel,
    time_cost: 3,
    variant: Variant::Argon2i,
    version: Version::Version13,
});


const RESERVED_USERNAMES: [&str; 8] = ["admin", "root", "sudo", "bootlegbilly", "susorodni", "billyb2", "billyb", "billy"];

const MIN_USER_LEN: usize = 4;
const MAX_USER_LEN: usize = 30;

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

enum InvalidUserReason {
    UsernameLenIncorrect,
    NotAlphanumeric,
    ReservedUsername,
}

impl Display for InvalidUserReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::auth::InvalidUserReason::*;

        match self {
            UsernameLenIncorrect => write!(f, "The username given is too short! (Must be between {} and {} long", MIN_USER_LEN, MAX_USER_LEN),
            NotAlphanumeric => write!(f, "The username given is not alphanumeric"),
            ReservedUsername => write!(f, "The username given is one of the following reserved usernames: {:?}", RESERVED_USERNAMES),
        }
    }
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

pub(crate) async fn register(user_info: AuthRequest, auth_db: Tree, money_db: Tree, auth_cookie_db: Tree, current_payment_id_db: Tree, current_payment_id: Arc<AtomicU64>) -> Result<impl Reply, Rejection> {
    let username = user_info.username;
    let password = user_info.password;

    let username_bytes = bincode::serialize(&username).unwrap();

   let get_response = move || -> (StatusCode, Box<dyn erased_serde::Serialize + Send>) { match auth_db.contains_key(&username_bytes).unwrap() {
        true => {
            let login_req_denial = RequestDenial::new(
                DenialFault::User,
                "Username already registered".to_string(),
        String::new(),
                );

                (StatusCode::CONFLICT, Box::new(login_req_denial))

        },
        false => {
            if let Some(reason) = invalid_user_reason(&username) {
                let login_req_denial = RequestDenial::new(
                DenialFault::User,
                reason.to_string(),
        String::new(),
                );

                return (StatusCode::BAD_GATEWAY, Box::new(login_req_denial));
            }

            let mut salt: [u8; 32] = [0; 32];
            rand::thread_rng().fill_bytes(&mut salt);

            let hashed_password = match argon2::hash_raw(password.as_bytes(), &salt, &ARGON2_CONFIG).ok() {
                Some(hash) => hash,
                None => {
                    let denial = RequestDenial::new(
                DenialFault::Server,
                "Internal Server Error".to_string(),
        String::new(),
                    );

                    return (StatusCode::INTERNAL_SERVER_ERROR, Box::new(denial));

                }

            };

            let user = User {
                username,
                salt,
                hashed_password,
            };

            let user_bytes = bincode::serialize(&user).unwrap();

            auth_db.insert(&username_bytes, user_bytes).unwrap();
            money_db.insert(&username_bytes, bincode::serialize(&FinancialInfo::new(current_payment_id, current_payment_id_db)).unwrap()).unwrap();

            let cookie = generate_auth_cookie(&user.username, &auth_db, auth_cookie_db);

            auth_cookie_result_to_response(user.username, cookie)

        }

    }};

    let (res, _) = tokio::join!(
        tokio::task::spawn_blocking(get_response),
        tokio::time::sleep(Duration::from_millis(800)),
    );

    let (code, json) = res.unwrap();

    Ok(Response::builder()
        .status(code)
        .body(simd_json::to_string(&json).unwrap())
        .unwrap())

}

pub(crate) async fn login(user_info: AuthRequest, auth_db: Tree, auth_cookie_db: Tree) -> Result<impl Reply, Rejection> {
    let username = user_info.username;
    let password = user_info.password;

    let wrong_user_or_pass: (StatusCode, Box<dyn erased_serde::Serialize + Send>) = (StatusCode::UNAUTHORIZED, Box::new(RequestDenial::new(
        DenialFault::User,
        "Wrong username or password".to_string(),
String::new(),
    )));

    let give_response = move || match auth_db.get(bincode::serialize(&username).unwrap()).unwrap() {
        Some(user_info_bytes) => {
            let user_info: User = bincode::deserialize(&user_info_bytes).unwrap();

            match argon2::verify_raw(password.as_bytes(), &user_info.salt, &user_info.hashed_password, &ARGON2_CONFIG).unwrap() {
                // Login successful
                true => {
                	let auth_cookie = generate_auth_cookie(&username, &auth_db, auth_cookie_db);
                	auth_cookie_result_to_response(username, auth_cookie)

                },
                false => wrong_user_or_pass,

            }

        },
        None =>wrong_user_or_pass,
    };

    let (res, _) = tokio::join!(
        tokio::task::spawn_blocking(give_response),
        tokio::time::sleep(Duration::from_millis(800)),
    );

    let (code, json) = res.unwrap();

    Ok(Response::builder()
        .status(code)
        .body(simd_json::to_string(&json).unwrap())
        .unwrap())

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

fn auth_cookie_result_to_response(username: String, auth_cookie: Result<AuthCookie, GenAuthCookieErr>) -> (StatusCode, Box<dyn erased_serde::Serialize + Send>) {
    (match auth_cookie.as_ref().err() {
        Some(err) => match err {
            GenAuthCookieErr::UserDoesntExist => StatusCode::UNAUTHORIZED,
            GenAuthCookieErr::DbErr(_) => StatusCode::INTERNAL_SERVER_ERROR,
            GenAuthCookieErr::BincodeErr(_) => StatusCode::INTERNAL_SERVER_ERROR,
        },
        None => StatusCode::OK,
    },
    match auth_cookie {
			Ok(auth_cookie) => {
                Box::new(AuthReqResponse {
                    username,
                    cookie: auth_cookie.cookie.clone(),
                    exp_time: auth_cookie.expiration_time,
                })

            },
			Err(_err) => {
                Box::new(RequestDenial::new(
            DenialFault::Server,
            "Internal Server Error".to_string(),
    String::new(),
                ))
            },
		}
    )
}

pub(crate) fn destroy_expired_auth_cookies(auth_cookie_db: Tree) {
	loop {
		use std::thread::sleep;

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
struct AuthReqResponse {
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

/// Returns Some(InvalidUserReason) if the username is invalid
fn invalid_user_reason(username: &str) -> Option<InvalidUserReason> {
    if username.len() < MIN_USER_LEN || username.len() > MAX_USER_LEN {
        Some(InvalidUserReason::UsernameLenIncorrect)

    } else if !username.chars().all(char::is_alphanumeric) {
        Some(InvalidUserReason::NotAlphanumeric)

    } else if RESERVED_USERNAMES.contains(&username.to_lowercase().as_str()) {
        Some(InvalidUserReason::ReservedUsername)
        
    } else {
        None

    }
   

}
