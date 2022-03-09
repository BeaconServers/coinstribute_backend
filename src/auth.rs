use crate::logging::{Log, LogType, Event, Priority};
use crate::db::{DB, AuthDB, CookieDB, MoneyDB};

use std::fmt::Display;
use std::net::SocketAddr;
use std::time::{SystemTime, Duration};

use rand::prelude::*;
use rand::distributions::Alphanumeric;

use serde::{Serialize, Deserialize};
use tokio::sync::mpsc::Sender;
use warp::reply::Reply;
use warp::{http::Response, hyper::StatusCode};
use warp::Rejection;

use crate::{DenialFault, RequestDenial, wallet};
use crate::money::FinancialInfo;

const RESERVED_USERNAMES: [&str; 8] = ["admin", "root", "sudo", "bootlegbilly", "susorodni", "billyb2", "billyb", "billy"];

const MIN_USER_LEN: usize = 4;
const MAX_USER_LEN: usize = 50;

#[derive(Deserialize)]
pub struct AuthRequest {
    username: String,
    password: String,

}

#[derive(Serialize, Deserialize)]
pub struct User {
    username: String,
    salt: [u8; 32],
    hashed_password: [u8; blake3::OUT_LEN],

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

pub async fn register(user_info: AuthRequest, auth_db: AuthDB, money_db: MoneyDB, auth_cookie_db: CookieDB, socket_addr: Option<SocketAddr>, logger: Sender<Log>) -> Result<impl Reply, Rejection> {
    let username = user_info.username.to_lowercase();
    let password = user_info.password;

    let get_res = async move { match auth_db.contains_key(&username).unwrap() {
        true => {
            let login_req_denial = RequestDenial::new(
                DenialFault::User,
                "Username already registered".to_string(),
                String::new(),
            );

            Log::new(
                Some(username.clone()), 
                socket_addr.unwrap().ip(), 
                Event::Registration { success: false }, 
                LogType::Auth, 
                login_req_denial.reason.clone(),
                Priority::Low,
            ).send(&logger).await.unwrap();

            Ok(login_req_denial.into_response(StatusCode::CONFLICT))
        },
        false => {
            if let Some(reason) = invalid_user_reason(&username) {
                let login_req_denial = RequestDenial::new(
                    DenialFault::User,
                    reason.to_string(),
                    String::new(),
                );

                Log::new(
                    Some(username.clone()), 
                    socket_addr.unwrap().ip(), 
                    Event::Registration { success: false }, 
                    LogType::Auth, 
                    login_req_denial.reason.clone(),
                    Priority::Low,
                ).send(&logger).await.unwrap();

                return Ok(login_req_denial.into_response(StatusCode::BAD_REQUEST));

            }

            let mut salt: [u8; 32] = [0; 32];
            rand::thread_rng().fill_bytes(&mut salt);

            // Since both of these are relatively long tasks, there's no reason not to run them in parallel.
            // Especially since create_account is async
            let (create_acc_res, hashed_password) = tokio::join!({
                let start_time = std::time::Instant::now();
                let addr = wallet().create_address(&username);
                println!("Time to create addr: {}", std::time::Instant::now().duration_since(start_time).as_secs_f32());
                addr
            },
                tokio::task::spawn_blocking(move || hash_password(password.as_bytes(), &salt))
            );

            let (addr, addr_index) = match create_acc_res {
                Ok(res) => res,
                Err(err) => {
                    let denial = RequestDenial::new(
                        DenialFault::Server,
                        "Internal Server Error".to_string(),
                        format!("Error creating financial account due to {err:?}"),
                    );

                    Log::new(
                        Some(username.clone()), 
                        socket_addr.unwrap().ip(), 
                        Event::Registration { success: false }, 
                        LogType::Auth, 
                        denial.reason.clone(),
                        Priority::High,
                    ).send(&logger).await.unwrap();

                    return Ok(denial.into_response(StatusCode::INTERNAL_SERVER_ERROR));
                }

            };

            let user = User {
                username,
                salt,
                hashed_password: hashed_password.unwrap(),
            };

            auth_db.insert(&user.username, &user).unwrap();
            money_db.insert(&user.username, &FinancialInfo::new(addr_index, addr)).unwrap();

            let cookie = generate_auth_cookie(&user.username, &auth_db, auth_cookie_db.clone());

            Ok(auth_cookie_result_to_response(user.username, cookie, &logger, socket_addr, false).await)

        }
    }};

    let (res, _) = tokio::join!(
        get_res,
        tokio::time::sleep(Duration::from_millis(100))

    );

    res
}

pub(crate) async fn login(user_info: AuthRequest, auth_db: AuthDB, auth_cookie_db: CookieDB, socket_addr: Option<SocketAddr>, logger: Sender<Log>) -> Result<impl Reply, Rejection> {
    let username = user_info.username.to_lowercase();
    let password = user_info.password;


    let denial = RequestDenial::new(
        DenialFault::User,
        "Wrong username or password".to_string(),
        String::new(),
    );

    let denial_log = Log::new(
        Some(username.clone()), 
        socket_addr.unwrap().ip(), 
        Event::Login { success: false }, 
        LogType::Auth, 
        denial.reason.clone(),
        Priority::Low,
    );

    let wrong_user_or_pass_resp = denial.into_response(StatusCode::UNAUTHORIZED);

    let give_response = async move { match auth_db.get(&username).unwrap() {
        Some(user_info) => {
            tokio::task::yield_now().await;
            match verify_hash(password.as_bytes(), &user_info.hashed_password) {
                // Login successful
                true => {
                	let auth_cookie = generate_auth_cookie(&username, &auth_db, auth_cookie_db.clone());
                	Ok(auth_cookie_result_to_response(username, auth_cookie, &logger, socket_addr, true).await)

                },
                false => {
                    denial_log.send(&logger).await.unwrap();
                    Ok(wrong_user_or_pass_resp)

                },

            }
        },
        None => {
            denial_log.send(&logger).await.unwrap();
            Ok(wrong_user_or_pass_resp)
        },
    }};

    let (res, _) = tokio::join!(
        tokio::task::spawn(give_response),
        tokio::time::sleep(Duration::from_millis(100)),
    );

    res.unwrap()

}

fn generate_auth_cookie(username: &String, auth_db: &AuthDB, auth_cookie_db: CookieDB) -> Result<AuthCookie, GenAuthCookieErr> {
	if !auth_db.contains_key(username)? {
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

	    auth_cookie_db.insert(&auth_cookie.username, &auth_cookie)?;

	    Ok(auth_cookie)

	}

}

async fn auth_cookie_result_to_response(username: String, auth_cookie: Result<AuthCookie, GenAuthCookieErr>, logger: &Sender<Log>, socket_addr: Option<SocketAddr>, login: bool) -> Response<String> {
    let status_code = match auth_cookie.as_ref().err() {
        Some(err) => {
            Log::new(
                Some(username.clone()), 
                socket_addr.unwrap().ip(),
                match err {
                    GenAuthCookieErr::UserDoesntExist => match login {
                        true => Event::Login { success: false },
                        false => Event::Registration { success: false }, 
                    },
                    GenAuthCookieErr::DbErr(_) => Event::ServerError,
                    GenAuthCookieErr::BincodeErr(_) => Event::ServerError,
                },
                LogType::Auth, 
                format!("{:?}", err),
                Priority::Low,
            ).send(logger).await.unwrap();

            match err {
                GenAuthCookieErr::UserDoesntExist => StatusCode::UNAUTHORIZED,
                GenAuthCookieErr::DbErr(_) => StatusCode::INTERNAL_SERVER_ERROR,
                GenAuthCookieErr::BincodeErr(_) => StatusCode::INTERNAL_SERVER_ERROR,
            }
        },
        None => {
            Log::new(
                Some(username.clone()), 
                socket_addr.unwrap().ip(), 
                match login {
                    true => Event::Login { success: true },
                    false => Event::Registration { success: true }, 
                }, 
                LogType::Auth, 
                String::new(),
                Priority::Low,
            ).send(logger).await.unwrap();

            StatusCode::OK
        },
    };

    match auth_cookie {
        Ok(auth_cookie) => {
            let resp = AuthReqResponse {
                username,
                cookie: auth_cookie.cookie.clone(),
                exp_time: auth_cookie.expiration_time,
            };

            let json = simd_json::to_string(&resp).unwrap();

            Response::builder()
                .status(status_code)
                .body(json)
                .unwrap()

        },
        Err(_err) => {
            let denial = RequestDenial::new(
                DenialFault::Server,
                "Internal Server Error".to_string(),
                String::new(),
            );

            denial.into_response(StatusCode::INTERNAL_SERVER_ERROR)
        },
    }
}

pub(crate) fn destroy_expired_auth_cookies(auth_cookie_db: CookieDB) {
	loop {
		use std::thread::sleep;

		let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

		// Loops through the auth cookie database and deletes any expired auth cookies, then waits a second
		auth_cookie_db.iter().for_each(|auth_cookie_key_val| {
			if let Ok((username, auth_cookie)) = auth_cookie_key_val {
				if current_time > auth_cookie.expiration_time {
					// The cookie has expired, so it must be removed
					auth_cookie_db.remove(&username).unwrap_or_else(|error| {
                        eprintln!("Error reading auth_cookie db: {error:?}");
                        None
                    });
				}

			} else {
				eprintln!("Error reading auth_cookie db");

			}
		});

		sleep(Duration::from_secs(3));

	}

}

#[derive(Serialize)]
struct AuthReqResponse {
    username: String,
    cookie: String,
    exp_time: u64,
}

pub(crate) fn verify_auth_cookie(username: &String, cookie: &str, auth_cookie_db: &CookieDB) -> bool {
	match auth_cookie_db.get(username).unwrap() {
		Some(auth_cookie) => {
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


fn hash_password(password: &[u8], salt: &[u8; 32]) -> [u8; blake3::OUT_LEN] {
    let mut bytes_to_hash = Vec::with_capacity(password.len() + salt.len());
    
    bytes_to_hash.extend_from_slice(password);
    bytes_to_hash.extend_from_slice(salt);

    let hash = blake3::hash(&bytes_to_hash);
    hash.try_into().unwrap()

}

fn verify_hash(password: &[u8], hash: &[u8; blake3::OUT_LEN]) -> bool {
    let salt = &hash[0..32];
    let auth_hash = hash_password(password, salt.try_into().unwrap());


    auth_hash == *hash

}
