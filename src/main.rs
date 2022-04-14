mod auth;
mod db;
mod logging;
mod money;
mod software;

use std::convert::Infallible;
use std::error::Error;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use auth::*;
use db::*;
use logging::*;
use monero_wallet::{Transfers, Wallet};
use money::*;
use once_cell::sync::{Lazy, OnceCell};
use serde_derive::Serialize;
use software::*;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, RwLock};
use warp::http::StatusCode;
use warp::hyper::Response;
use warp::{Filter, Rejection, Reply};

static WALLET_RPC_ADDR: Lazy<String> = Lazy::new(|| String::from("http://127.0.0.1:19000"));
static DAEMON_ADDR: Lazy<String> = Lazy::new(|| String::from("http://127.0.0.1:18081"));

static WALLET: OnceCell<Wallet> = OnceCell::new();

fn main() {
	wallet();
	let tokio_rt = Arc::new(Runtime::new().unwrap());

	let db = sled::Config::default()
		.path("db")
		.mode(sled::Mode::HighThroughput)
		.open()
		.expect("Failed to open database");

	// Since stuff like the current Monero fee can take a long time to get from the wallet rpc, caching it is wise
	let cached_fee = Arc::new(CachedFee::new());

	let auth_db = AuthDB::new(
		db.open_tree(b"auth_db")
			.expect("Failed to open authorization tree, time to panic!"),
	);
	let cookie_db = CookieDB::new(
		db.open_tree(b"cookie_db")
			.expect("Failed to open cookie tree, time to panic!"),
	);
	let money_db = MoneyDB::new(
		db.open_tree(b"money_db")
			.expect("Failed to open money tree, oh no"),
	);
	let captcha_db = CaptchaDB::new(
		db.open_tree(b"captcha_db")
			.expect("Failed to open captcha_db, time to panic!"),
	);
	let software_db = SoftwareDB::new(
		db.open_tree(b"software_db")
			.expect("Failed to open software_db, time to panic!"),
	);
	let upload_id_db = UploadIdDB::new(
		db.open_tree(b"upload_id_db")
			.expect("Failed to open upload_id_db, time to panic!"),
	);
	let download_count_db = DownloadCountDB::new(
		db.open_tree(b"download_count_db")
			.expect("Failed to open download_count_db, time to panic!"),
	);
	let software_ownership_db = SoftwareOwnershipDB::new(
		db.open_tree(b"software_ownership_db")
			.expect("Failed to open software_ownership_db, time to panic!"),
	);

	let all_transfers: Arc<RwLock<Vec<Transfers>>> = Arc::new(RwLock::new(Vec::new()));
	let current_software_id: Arc<AtomicU64> = Arc::new(AtomicU64::new(
		match software_db.iter().map(|item| item.unwrap().0).max() {
			Some(id) => id + 1,
			None => {
				println!("No software uploads found");
				0
			},
		},
	));

	let (monero_sender, monero_tx_rcv) = mpsc::unbounded_channel::<TransferReq>();
	let (log_send, log_rcv) = mpsc::channel::<Log>(200);

	let pub_addr = wallet().address();

	let addr = monero::util::address::Address::subaddress(
		monero::network::Network::Mainnet,
		pub_addr.public_spend,
		pub_addr.public_view,
	);

	let auth_db = warp::any().map(move || auth_db.clone());
	let cookie_db_filter = {
		let cookie_db = cookie_db.clone();
		warp::any().map(move || cookie_db.clone())
	};
	let money_db_filter = {
		let money_db_clone = money_db.clone();
		warp::any().map(move || money_db_clone.clone())
	};
	let software_db_filter = {
		let software_db_clone = software_db.clone();
		warp::any().map(move || software_db_clone.clone())
	};
	let upload_id_db_filter = {
		let db = upload_id_db.clone();
		warp::any().map(move || db.clone())
	};
	let download_count_db_filter = {
		let db = download_count_db.clone();
		warp::any().map(move || db.clone())
	};
	let software_ownership_db_filter = {
		let db = software_ownership_db.clone();
		warp::any().map(move || db.clone())
	};

	let captcha_db_filter = {
		let captcha_db = captcha_db.clone();
		warp::any().map(move || captcha_db.clone())
	};
	let wallet_filter = warp::any().map(wallet);
	let cached_fee_filter = {
		let cached_fee = cached_fee.clone();
		warp::any().map(move || cached_fee.clone())
	};
	let monero_send_filter = warp::any().map(move || monero_sender.clone());
	let log_filter = {
		let log_send = log_send.clone();
		warp::any().map(move || log_send.clone())
	};
	let current_software_id_filter = {
		let id = current_software_id.clone();
		warp::any().map(move || id.clone())
	};

	let cors = warp::cors()
		.allow_any_origin()
		.allow_methods([
			"POST", "GET", "PUT", "HEAD", "DELETE", "OPTIONS", "CONNECT", "PATCH", "TRACE",
		])
		.allow_headers(["Content-Type", "Sec-WebSocket-Protocol"]);

	let register = warp::path("signup")
		// 2 KB limit to username + password
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(auth_db.clone())
		.and(money_db_filter.clone())
		.and(cookie_db_filter.clone())
		.and(warp::addr::remote())
		.and(log_filter.clone())
		.and(captcha_db_filter.clone())
		.and_then(register);

	let login = warp::path("login")
		// 2 KB limit to username + password
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(auth_db.clone())
		.and(cookie_db_filter.clone())
		.and(warp::addr::remote())
		.and(log_filter.clone())
		.and(captcha_db_filter.clone())
		.and_then(login);

	let deposit_req = warp::path("deposit_req")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(auth_db.clone())
		.and(cookie_db_filter.clone())
		.and(money_db_filter.clone())
		.and(wallet_filter)
		.and(cached_fee_filter.clone())
		.and(warp::addr::remote())
		.and(log_filter.clone())
		.and_then(deposit_req);

	let get_balance = warp::path("get_balance")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(auth_db.clone())
		.and(money_db_filter.clone())
		.and(cookie_db_filter.clone())
		.and(warp::addr::remote())
		.and(log_filter.clone())
		.and_then(get_balance);

	let server_profits = warp::path("server_profits")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(money_db_filter.clone())
		.and_then(server_profits);

	let attach_xmr_address = warp::path("attach_xmr_address")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(money_db_filter.clone())
		.and(auth_db.clone())
		.and(cookie_db_filter.clone())
		.and(warp::addr::remote())
		.and(log_filter.clone())
		.and_then(attach_xmr_address);

	let withdraw_monero = warp::path("withdraw_xmr")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(auth_db.clone())
		.and(cookie_db_filter.clone())
		.and(money_db_filter.clone())
		.and(cached_fee_filter)
		.and(monero_send_filter)
		.and(warp::addr::remote())
		.and(log_filter.clone())
		.and_then(withdraw_monero);

	let new_software = warp::path("new_software")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(auth_db.clone())
		.and(cookie_db_filter.clone())
		.and(software_db_filter.clone())
		.and(upload_id_db_filter.clone())
		.and(captcha_db_filter.clone())
		.and(current_software_id_filter.clone())
		.and(money_db_filter.clone())
		.map(new_software);

	let upload_software = warp::path("upload_software")
		.and(warp::ws())
		.and(cookie_db_filter.clone())
		.and(upload_id_db_filter.clone())
		.and(software_db_filter.clone())
		.and_then(upload_software);

	let search_software = warp::path("search_software")
		.and(warp::body::content_length_limit(2048))
		.and(warp::body::json())
		.and(software_db_filter.clone())
		.and(download_count_db_filter.clone())
		.and_then(search_software);

	let download_software = warp::path("download_software")
		.and(warp::ws())
		.and(cookie_db_filter.clone())
		.and(software_db_filter.clone())
		.and(download_count_db_filter.clone())
		.and(software_ownership_db_filter.clone())
		.and_then(download_software);

	let view_software_image = warp::path!("view_software_image" / String)
		.and(warp::addr::remote())
		.and(log_filter)
		.and_then(view_software_image);

	let create_captcha = warp::path("create_captcha")
		.and(captcha_db_filter.clone())
		.map(create_captcha);

	let view_captcha = warp::path!("view_captcha" / String)
		.and(captcha_db_filter)
		.map(view_captcha);

	let post_routes = warp::post().and(
		register
			.or(login)
			.or(deposit_req)
			.or(attach_xmr_address)
			.or(get_balance)
			.or(withdraw_monero)
			.or(server_profits)
			.or(new_software)
			.or(search_software),
	);

	let get_routes = warp::get().and(create_captcha.or(view_captcha).or(view_software_image));

	tokio_rt.spawn(async move {
		let wallet = wallet();

		let mut set_settings = false;

		println!("Configuring wallet...");

		while !set_settings {
			set_settings = tokio::try_join!(
				wallet.set_daemon(&DAEMON_ADDR, DAEMON_ADDR.contains("127.0.0.1")),
				wallet.set_refresh_time(60),
			)
			.is_ok();
		}

		println!("Successfully configured wallet!");
	});

	tokio_rt.spawn(update_transfers(
		wallet(),
		all_transfers.clone(),
		money_db.clone(),
	));
	tokio_rt.spawn(update_acc_balances(
		money_db.clone(),
		wallet(),
		all_transfers,
		log_send.clone(),
	));
	tokio_rt.spawn(update_cached_fee(cached_fee, wallet()));
	tokio_rt.spawn(send_monero(wallet(), monero_tx_rcv, money_db));
	tokio_rt.spawn(logging_service(log_rcv));
	tokio_rt.spawn_blocking(move || destroy_expired_auth_cookies(cookie_db));
	tokio_rt.spawn_blocking(move || destroy_expired_captchas(captcha_db.clone()));

	tokio_rt.block_on(
		warp::serve(
			post_routes
				.with(cors.clone())
				.or(upload_software.with(cors.clone()))
				.or(download_software.with(cors.clone()))
				.or(get_routes.with(cors))
				.recover(handle_rejection),
		)
		.run(([127, 0, 0, 1], 3030)),
	);
}

fn wallet() -> &'static Wallet {
	WALLET.get_or_init(|| {
		// Is it wasteful to spawn a whole tokio Runtime for a single function? Yes.
		// Should I TODO replace this with futures library? Also yes.
		let tokio_rt = Runtime::new().unwrap();
		println!("Initializing wallet...");
		let wallet = tokio_rt.block_on(Wallet::new(
			WALLET_RPC_ADDR.clone(),
			DAEMON_ADDR.clone(),
			Some(Duration::from_secs(3)),
		));
		println!("Initialized wallet!");

		wallet
	})
}

#[derive(Serialize)]
struct ErrorMessage {
	code: u16,
	message: String,
}

async fn handle_rejection(err: Rejection) -> Result<impl Reply, Infallible> {
	let code;
	let message: String;

	if err.is_not_found() {
		code = StatusCode::NOT_FOUND;
		message = "NOT_FOUND".to_string();
	} else if let Some(e) = err.find::<warp::filters::body::BodyDeserializeError>() {
		// This error happens if the body could not be deserialized correctly
		// We can use the cause to analyze the error and customize the error message
		message = match e.source() {
			Some(cause) => cause.to_string(),
			None => "BAD_REQUEST".to_string(),
		};

		code = StatusCode::BAD_REQUEST;
	} else if err.find::<warp::reject::MethodNotAllowed>().is_some() {
		// We can handle a specific error, here METHOD_NOT_ALLOWED,
		// and render it however we want
		code = StatusCode::METHOD_NOT_ALLOWED;
		message = "METHOD_NOT_ALLOWED".to_string();
	} else {
		// We should have expected this... Just log and say its a 500
		eprintln!("unhandled rejection: {:?}", err);
		code = StatusCode::INTERNAL_SERVER_ERROR;
		message = "UNHANDLED_REJECTION".to_string();
	}

	let json = warp::reply::json(&ErrorMessage {
		code: code.as_u16(),
		message,
	});

	Ok(warp::reply::with_status(json, code))
}

enum DenialFault {
	Server,
	User,
}

impl std::fmt::Display for DenialFault {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{}",
			match self {
				DenialFault::Server => "Server",
				DenialFault::User => "User",
			}
		)
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

	fn into_response(self, status_code: StatusCode) -> Response<String> {
		let json = simd_json::to_string(&self).unwrap();

		Response::builder().status(status_code).body(json).unwrap()
	}
}
