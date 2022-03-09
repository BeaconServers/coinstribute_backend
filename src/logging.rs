//TODO: Use the log

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use monero::Address;
use serde::Serialize;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::fs;
use tokio::sync::Mutex;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Serialize)]
pub struct Log {
	log_type: LogType,
	username: Option<String>,
	/// Time in Unix time
	time: u64,
	ip: IpAddr,
	event: Event,
	reason: String,
	priority: Priority,
}

impl Log {
	pub fn new(username: Option<String>, ip: IpAddr, event: Event, log_type: LogType, reason: String, priority: Priority) -> Self {
		Self {
			username,
			time: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
			log_type,
			ip,
			event,
			reason,
			priority,
		}

	}

	pub async fn send(self, log_sender: &Sender<Self>) -> Result<(), SendError<Self>> {
		log_sender.send(self).await

	}

}

pub async fn logging_service(mut log_ev: Receiver<Log>) {
	let mut file = fs::OpenOptions::new()
		.append(true)
		.create(true)
		.read(false)
		.open("./coinstribute.log")
		.await
		.unwrap();

	let logs_to_write = Arc::new(Mutex::new(String::with_capacity(65_000)));
	let logs_to_write_clone = logs_to_write.clone();

	let (force_write_sender, force_write_rcv) = channel::<()>(1);

	tokio::task::spawn(async move {
		let logs_to_write = logs_to_write_clone;
		let mut force_write_rcv = force_write_rcv;

		loop {
			tokio::select!(
				_ = force_write_rcv.recv() => {},
				_ = tokio::time::sleep(Duration::from_secs(90)) => {},

			);

			let mut logs_to_write = logs_to_write.lock().await;

			file.write_all(logs_to_write.as_bytes()).await.unwrap();
			logs_to_write.clear();
		}
	});

	while let Some(log) = log_ev.recv().await {
		let mut logs_to_write = logs_to_write.lock().await;
		let log_string = simd_json::to_string(&log).unwrap();

		logs_to_write.push_str(&log_string);
		logs_to_write.push('\n');

		// Write to log file either every 90 seconds or if the log buffer is above 65 KB
		if logs_to_write.len() > 65_000 {
			// Drop the lock on the logs
			std::mem::drop(logs_to_write);
			force_write_sender.send(()).await.unwrap();

		}
	}
}

#[derive(Debug, Serialize)]
pub enum Event {
	Registration {
		success: bool,
	},
	Login {
		success: bool,
	},
	Withdrawal {
		amt: u64,
		addr: Address,
	},
	GetBalance,
	BalanceChange {
		from: u64,
		to: u64,
	},
	ServerError,
	UserError,
	DepositReq,
	AttachAddr,
	AuthFailureNonLogin,
}

#[derive(Debug, Serialize)]
pub enum LogType {
	Auth,
	Money,	
}

#[derive(Debug, Serialize)]
pub enum Priority {
	Low,
	Medium,
	High,
}
