use std::time::Duration;
use reqwest::{Client, ClientBuilder, Method};
use simd_json::ValueAccess;


pub struct DaemonRPC {
	client: Client,
	daemon_url: String,
	/// Enables or disables certain functionality
	trusted: bool,
}

impl DaemonRPC {
	pub(crate) fn new(daemon_url: String, trusted: bool, connection_timeout: Option<Duration>, send_timeout: Option<Duration>) -> Self {
		Self {
			client: ClientBuilder::new()
				.connect_timeout(connection_timeout.unwrap_or(Duration::from_millis(10000)))
				.timeout(send_timeout.unwrap_or(Duration::from_millis(10000)))
				.build()
				.unwrap(),
			daemon_url,
			trusted,

		}
	}

	async fn request(&self, payload: &str) -> Result<simd_json::owned::Value, DaemonRPCError> {
		let req = self.client.request(Method::POST, self.daemon_url.clone() + "/json_rpc")
			.body(payload.to_string())
			.build()?;

		let response = self.client.execute(req).await?;
		Ok(response.json::<simd_json::owned::Value>().await?)

	}

	pub async fn get_fee(&self) -> Result<u64, DaemonRPCError> {
		const REQUEST_BODY: &'static str = r#"{"jsonrpc":"2.0","id":"0","method":"get_fee_estimate"}}"#;
		let response = self.request(REQUEST_BODY).await?;

		response["result"]["fee"].as_u64().ok_or(DaemonRPCError::MissingData)

	}

}

#[derive(Debug)]
pub enum DaemonRPCError {
	MissingData,
	ReqwestError(reqwest::Error),

}

impl From<reqwest::Error> for DaemonRPCError {
    fn from(e: reqwest::Error) -> Self {
        Self::ReqwestError(e)

    }
}
