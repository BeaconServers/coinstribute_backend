use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use arrayvec::ArrayString;
use blake3::{Hash, Hasher};
use futures_util::{SinkExt, StreamExt};
use rand::prelude::*;
// use rayon::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::Sender;
use warp::hyper::{Response, StatusCode};
use warp::ws::Message;
use warp::{Rejection, Reply};

use crate::auth::verify_auth_cookie;
use crate::db::{
	AuthDB,
	CaptchaDB,
	CookieDB,
	DownloadCountDB,
	MoneyDB,
	SoftwareDB,
	SoftwareOwnershipDB,
	UploadIdDB,
	DB,
};
use crate::logging::{Event, Log, LogType, Priority};
use crate::{DenialFault, RequestDenial};

#[derive(Clone, Serialize, Deserialize)]
pub enum ArchType {
	/// WASM32 or anything else meant to run in a web browser
	Web,
	X86_64,
}

/// While Zstd is the preferred compression algorithm, the current rust support is bindings to the C library.
/// This means that for the web version of the site, it can't be used. As an alternative, Brotli is used instead
#[derive(Clone, Serialize, Deserialize)]
pub enum CompressionType {
	Zstd,
	Brotli,
}

#[derive(Deserialize)]
pub struct CreateSoftwareReq {
	username: String,
	auth_cookie: String,
	arch_type: ArchType,
	title: String,
	description: String,
	compressed_size: u64,
	price: ItemPrice,
	compression_type: CompressionType,
	captcha_hash: ArrayString<{ 2 * blake3::OUT_LEN }>,
	captcha_answer: String,
}

#[derive(Serialize)]
pub struct NewSoftwareResp {
	upload_id: ArrayString<{ 2 * blake3::OUT_LEN }>,
}

pub fn new_software(
	new_software_req: CreateSoftwareReq, auth_db: AuthDB, cookie_db: CookieDB,
	software_db: SoftwareDB, upload_id_db: UploadIdDB, captcha_db: CaptchaDB,
	current_soft_id: Arc<AtomicU64>, money_db: MoneyDB,
) -> Response<String> {
	let bad_captcha = || -> Response<String> {
		let denial = RequestDenial::new(
			DenialFault::User,
			"Incorrect captcha".to_string(),
			String::new(),
		);

		denial.into_response(StatusCode::UNAUTHORIZED)
	};

	let captcha_hash =
		match hex_simd::decode_to_boxed_bytes(new_software_req.captcha_hash.as_bytes()) {
			Ok(captcha_hash) => captcha_hash,
			Err(_err) => {
				return bad_captcha();
			},
		};

	let captcha = match captcha_db
		.get(captcha_hash.as_ref().try_into().unwrap())
		.unwrap()
	{
		Some(captcha) => captcha,
		None => {
			return bad_captcha();
		},
	};

	if captcha.answer != new_software_req.captcha_answer {
		return bad_captcha();
	}

	if !auth_db.contains_key(&new_software_req.username).unwrap() ||
		!verify_auth_cookie(
			&new_software_req.username,
			&new_software_req.auth_cookie,
			&cookie_db,
		) {
		let denial = RequestDenial::new(
			DenialFault::User,
			String::from("Invalid username or cookie"),
			String::new(),
		);
		return denial.into_response(StatusCode::UNAUTHORIZED);
	}

	// Make sure the user has at least 4271880000 piconero ($1 at the time of writing)
	let mut financial_info = money_db.get(&new_software_req.username).unwrap().unwrap();
	if let Err(balance) = financial_info.spend_money(4271880000) {
		let denial = RequestDenial::new(
			DenialFault::User,
			format!("Balance is too low"),
			balance.to_string(),
		);

		return denial.into_response(StatusCode::PRECONDITION_FAILED);
	}

	money_db
		.insert(&new_software_req.username, &financial_info)
		.unwrap();

	// While obviously modified clients can just lie about how large the game's size is, there are strict checks when actually uploading the game to ensure that the file is the exact size specified
	if new_software_req.compressed_size > 10_485_760
	// 10 MiB
	{
		let denial = RequestDenial::new(
			DenialFault::User,
			String::from("File is too large"),
			"Please keep your compressed game files under 10 MiB".to_string(),
		);
		return denial.into_response(StatusCode::BAD_REQUEST);
	}

	let software_id = current_soft_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let mut random_bytes: [u8; 32] = [0; 32];

	thread_rng().fill_bytes(&mut random_bytes);

	let mut bytes_to_hash = Vec::with_capacity(
		random_bytes.len() + new_software_req.title.len() + new_software_req.username.len(),
	);

	bytes_to_hash.extend_from_slice(&random_bytes);
	bytes_to_hash.extend_from_slice(new_software_req.title.as_bytes());
	bytes_to_hash.extend_from_slice(new_software_req.username.as_bytes());

	// Basically just a way of generating a pretty random ID
	let upload_id_hash = blake3::hash(&bytes_to_hash);
	let upload_id_bytes = *upload_id_hash.as_bytes();

	let upload_id_str = upload_id_hash.to_hex();

	let mut image_hash_bytes = [0_u8; 32];

	// A generic image of a controller
	let boxed_image_hash_bytes = hex_simd::decode_to_boxed_bytes(
		"6651eb8fd8216a717634973da55a5971dc15b29efc37cdc2d7f70bb9a45acf5f".as_bytes(),
	)
	.unwrap();

	image_hash_bytes.copy_from_slice(&boxed_image_hash_bytes);

	let game = Item {
		id: software_id,
		title: new_software_req.title,
		creator: new_software_req.username,
		description: new_software_req.description,
		arch_type: new_software_req.arch_type,
		compression_type: new_software_req.compression_type,
		compressed_size: new_software_req.compressed_size,
		price: new_software_req.price,
		files: HashMap::new(),
		upload_id: Some(upload_id_bytes),
		// Since the software hasn't been uploaded yet, it hasn't been created
		creation_time: None,
		title_image_hash: image_hash_bytes,
	};

	software_db.insert(&software_id, &game).unwrap();
	upload_id_db.insert(&upload_id_bytes, &software_id).unwrap();

	let resp = NewSoftwareResp {
		upload_id: upload_id_str,
	};

	Response::builder()
		.status(StatusCode::OK)
		.body(simd_json::to_string(&resp).unwrap())
		.unwrap()
}

// TODO: Replace all the sudden disconnects with sending errors back to the client
// TODO: Remove all cached files on sudden breaks or disconnects or errors or whatever
pub async fn upload_software(
	websocket: warp::ws::Ws, cookie_db: CookieDB, upload_id_db: UploadIdDB, software_db: SoftwareDB,
) -> Result<impl Reply, Rejection> {
	Ok(websocket.on_upgrade(|ws| async move {
		let (_cli_ws_snd, mut cli_ws_rcv) = ws.split();

		let mut software_info: Option<(SoftwareInfo, Item)> = None;

		let mut total_size = 0;
		let mut num_chunks = 0;
		let mut amt_written_to_buffer = 0;
		let mut chunk_info: Option<ChunkInfo> = None;
		let mut uploaded_image = false;
		let mut image_hash_info: Option<([u8; 32], Option<File>)> = None;
		let mut image_hasher = Hasher::new();

		let mut total_image_size = 0;

		const CHUNK_BUFFER_LEN: usize = 65535;
		let mut chunk_buffer = [0_u8; CHUNK_BUFFER_LEN];

		let mut file_hashes_seen: HashSet<[u8; 32]> = HashSet::new();

		'connection: while let Some(result) = cli_ws_rcv.next().await {
			match result {
				Ok(msg) => {
					let msg = msg.as_bytes();

					if let Some((software_info, software_item)) = software_info.as_mut() {
						if num_chunks == software_info.num_chunks {
							if uploaded_image {
								for (hash, _file) in software_info.files.iter() {
									let mut src_path = PathBuf::new();
									src_path.push(".cached_files/");
									src_path.push(&hash.to_string());

									let source = tokio::fs::OpenOptions::new()
										.read(true)
										.open(&src_path)
										.await
										.unwrap();

									let mut dst_path = PathBuf::new();
									dst_path.push(".game_files/");
									tokio::fs::create_dir_all(&dst_path).await.unwrap();

									dst_path.push(&hash.to_string());

									// Before decompressing, check the hash of the file

									let mut hasher = blake3::Hasher::new();
									let mut file_src = BufReader::new(source);
									let buffer = &mut chunk_buffer;

									loop {
										match file_src.read(buffer).await {
											Ok(0) => break,
											Ok(n) => hasher.update(&buffer[..n]),
											Err(ref e) => {
												if e.kind() == tokio::io::ErrorKind::Interrupted {
													continue;
												} else {
													panic!("{e}");
												}
											},
										};
									}

									let computed_hash = hasher.finalize();

									// corrupted file
									let remove_cached_files = || async {
										tokio::fs::remove_file(&src_path).await.unwrap()
									};

									if *hash == computed_hash {
										println!("Hashes are equivalent");
									} else {
										remove_cached_files().await;
										panic!("Hash: {}\nComputed Hash: {}", hash, computed_hash);
									}

									tokio::fs::copy(&src_path, &dst_path).await.unwrap();
									remove_cached_files().await;

									println!("Finished writing");
								}

								// When all of the files are properly checksummed, finish up.
								software_item.files = software_info
									.files
									.iter()
									.map(|(k, v)| (*k.as_bytes(), v.clone()))
									.collect();
								software_item.creation_time = Some(
									OffsetDateTime::now_utc()
										.unix_timestamp()
										.try_into()
										.unwrap(),
								);

								software_db
									.insert(&software_item.id, software_item)
									.unwrap();

								println!("Added software");
								// TODO: Remove upload id n stuff like that
								break 'connection;
							} else if let Some((image_hash_bytes, image_file)) =
								image_hash_info.as_mut()
							{
								while total_image_size < software_info.compressed_image_len {
									if let Some(image_file) = image_file.as_mut() {
										image_file.write_all(msg).await.unwrap();
										image_hasher.update(msg);
									}
									// Don't do any file wrrites if the image hash already
									// exists

									let msg_len: u32 = msg.len().try_into().unwrap();
									total_image_size += msg_len;
								}

								let calced_image_hash = image_hasher.finalize();
								if image_file.is_some() &&
									*calced_image_hash.as_bytes() != *image_hash_bytes
								{
									eprintln!("Image hashes don't match!");
									break 'connection;
								} else {
									uploaded_image = true;
								}
							} else {
								// The 32 byte hash of the image
								let image_hash_bytes: [u8; 32] = msg[..32].try_into().unwrap();
								let image_hash_hex = hex_simd::encode_to_boxed_str(
									&image_hash_bytes,
									hex_simd::AsciiCase::Lower,
								);

								let mut path = PathBuf::new();
								path.push("./.game_images/");
								path.push(&*image_hash_hex);

								let file_exists = File::open(&path).await.is_ok();

								if !file_exists {
									let file = OpenOptions::new()
										.create(true)
										.write(true)
										.open(&path)
										.await
										.unwrap();

									image_hash_info = Some((image_hash_bytes, Some(file)));
								} else {
									image_hash_info = Some((image_hash_bytes, None));
								}
							}
						} else {
							if let Some(chunk_info_ref) = chunk_info.as_ref() {
								chunk_buffer[..(amt_written_to_buffer + msg.len())]
									.as_mut()
									.copy_from_slice(msg);
								amt_written_to_buffer += msg.len();

								let amt_written_to_buffer_as_u64: u64 =
									amt_written_to_buffer.try_into().unwrap();

								total_size += amt_written_to_buffer_as_u64;

								if total_size > software_item.compressed_size {
									eprintln!("Software info size doesn't match given size");
									break 'connection;
								}

								if amt_written_to_buffer_as_u64 == chunk_info_ref.len {
									// When we've finished writing a single chunk to memory, begin
									// writing it to disk
									let file_hash = chunk_info_ref.file;

									let chunk_buffer = &chunk_buffer[..amt_written_to_buffer];
									let mut path = PathBuf::new();
									path.push(".cached_files/");

									tokio::fs::create_dir_all(path.clone()).await.unwrap();
									path.push(&file_hash.to_string());
									let mut file = tokio::fs::OpenOptions::new()
										.append(true)
										.create(true)
										.open(&path)
										.await
										.unwrap();

									file.write_all(chunk_buffer).await.unwrap();
									file.flush().await.unwrap();
									file.shutdown().await.unwrap();

									// Check if this is the first chunk being uploaded for the file
									let is_first_chunk =
										file_hashes_seen.insert(*file_hash.as_bytes());

									if is_first_chunk && !infer::is(chunk_buffer, "zst") {
										tokio::fs::remove_file(path).await.unwrap();
										panic!("The file given is NOT a zst file");
									}

									// Reset the chunk buffer stuff
									amt_written_to_buffer = 0;
									chunk_info = None;

									num_chunks += 1;
								}
							} else {
								let chunk_info_bytes = &msg;
								chunk_info = Some(ChunkInfo {
									len: u64::from_le_bytes(
										chunk_info_bytes[0..8].try_into().unwrap(),
									),
									file: {
										let checksum_array: [u8; 32] =
											chunk_info_bytes[8..40].try_into().unwrap();
										Hash::from(checksum_array)
									},
								});
							}
						}
					} else {
						let software_info_bytes = &msg;

						let num_chunks =
							u64::from_le_bytes(software_info_bytes[0..8].try_into().unwrap());
						let (username, username_end_index) = {
							let username_end_index: usize = software_info_bytes[8..]
								.iter()
								.position(|b| *b == 0)
								.unwrap() + 8;
							(
								String::from_utf8(
									software_info_bytes[8..username_end_index].to_vec(),
								),
								username_end_index,
							)
						};
						let username = match username {
							Ok(username) => username,
							Err(_err) => {
								eprintln!("Invalid username");
								break;
							},
						};

						let (auth_cookie, cookie_end_index) = {
							let auth_cookie_end_index: usize = software_info_bytes
								[username_end_index + 1..]
								.iter()
								.position(|b| *b == 0)
								.unwrap() + username_end_index + 1;
							(
								String::from_utf8(
									software_info_bytes
										[username_end_index + 1..auth_cookie_end_index]
										.to_vec(),
								),
								auth_cookie_end_index,
							)
						};

						let auth_cookie = match auth_cookie {
							Ok(cookie) => {
								match verify_auth_cookie(&username, &cookie, &cookie_db) {
									true => cookie,
									false => {
										eprintln!("Invalid auth cookie");
										break;
									},
								}
							},
							Err(_err) => {
								eprintln!("Invalid auth cookie string");
								break;
							},
						};

						let upload_id: [u8; blake3::OUT_LEN] = {
							software_info_bytes[cookie_end_index + 1..cookie_end_index + 33]
								.try_into()
								.unwrap()
						};

						if !upload_id_db.contains_key(&upload_id).unwrap() {
							eprintln!("Upload ID not found");
							break 'connection;
						}

						let image_len_bytes: [u8; 4] = software_info_bytes
							[cookie_end_index + 33..cookie_end_index + 37]
							.try_into()
							.unwrap();
						let compressed_image_len: u32 = u32::from_le_bytes(image_len_bytes);

						// 1 MiB
						if compressed_image_len > 1048576 {
							eprintln!("Image too large");
							break 'connection;

						// Because of ZSTD header stuff, it's impossible for the image to be under
						// 4 bytes. It'll probably be bigger than this anyway lol
						} else if compressed_image_len < 4 {
							eprintln!("Image too small");
							break 'connection;
						}

						let mut files = HashMap::new();

						let mut bytes_iter = software_info_bytes[cookie_end_index + 37..].iter();

						loop {
							let mut checksum = [0; blake3::OUT_LEN];

							if bytes_iter.size_hint().0 == 0 {
								break;
							}

							let checksum_iter = bytes_iter.by_ref().take(blake3::OUT_LEN);

							checksum_iter.into_iter().zip(checksum.iter_mut()).for_each(
								|(byte, chk_b)| {
									*chk_b = *byte;
								},
							);

							let filename_iter =
								bytes_iter.by_ref().take_while(|b| **b != 0).copied();

							let filename_vec = filename_iter.collect::<Vec<u8>>();
							let filename = String::from_utf8_lossy(&filename_vec);

							files.insert(Hash::from(checksum), filename.to_string());
						}

						// Since each chunk can be up to a maximum of 8192 bytes, and the maximum size per game is 10_485_760 bytes, meaning max num of chunks is 1280
						const MAX_NUM_CHUNKS: u64 = 10_485_760 / 8192; // 1280 chunks

						if num_chunks <= MAX_NUM_CHUNKS {
							let software_id = upload_id_db.get(&upload_id).unwrap().unwrap();
							let software_item = software_db.get(&software_id).unwrap().unwrap();

							software_info = Some((
								SoftwareInfo {
									username,
									auth_cookie,
									upload_id,
									num_chunks,
									files,
									compressed_image_len,
								},
								software_item,
							));
						} else {
							todo!("{num_chunks}")
						}
					}
				},
				Err(err) => {
					println!("error receiving message {err:?}");
					break;
				},
			};
		}
	}))
}

#[derive(Deserialize)]
enum SearchAlg {
	HighestRated,
	LowestRated,
	MostDownloaded,
	LeastDownloaded,
	Newest,
	Oldest,
	Trending,
	HighestPrice,
	LowestPrice,
}

#[derive(Deserialize)]
pub struct SearchSoftwareReq {
	term: String,
	sort_by: SearchAlg,
	start_index: u64,
	num_results: u16,
}

#[derive(Serialize)]
struct SearchResult {
	title: String,
	item_id: u64,
	creation_time: u64,
	num_downloads: u64,
}

impl SearchResult {
	fn from_item(item: Item, download_count_db: &DownloadCountDB) -> Self {
		SearchResult {
			title: item.title,
			item_id: item.id,
			creation_time: item.creation_time.unwrap(),
			num_downloads: download_count_db
				.get_or_insert(&item.id, &SoftwareDownloads(BTreeSet::new()))
				.unwrap()
				.0
				.len()
				.try_into()
				.unwrap(),
		}
	}
}

pub async fn search_software(
	search_req: SearchSoftwareReq, software_db: SoftwareDB, download_count_db: DownloadCountDB,
) -> Result<impl Reply, Rejection> {
	const MAX_NUM_RESULTS: u16 = 100;

	if search_req.num_results > MAX_NUM_RESULTS {
		let denial = RequestDenial::new(
			DenialFault::User,
			String::from("Too many results requested"),
			format!("Try requesting {MAX_NUM_RESULTS} results or less"),
		);
		return Ok(denial.into_response(StatusCode::BAD_REQUEST));
	}

	let capacity = software_db.len().min(search_req.num_results.into());

	let mut results = Vec::with_capacity(capacity);

	software_db.iter().for_each(|res| {
		let (_item_id, item) = res.unwrap();

		if item.creation_time.is_some() && item.title.to_lowercase().contains(&search_req.term) {
			if results.len() < capacity {
				results.push(item);
			} else {
				let search_alg = match search_req.sort_by {
					SearchAlg::Newest => newest_item,
					SearchAlg::Oldest => oldest_item,
					SearchAlg::MostDownloaded => most_downloaded_item,
					SearchAlg::LeastDownloaded => least_downloaded_item,
					_ => todo!(),
				};

				let worst_item = results.iter_mut().reduce(|item1, item2| {
					match search_alg(item1, item2, &download_count_db) {
						// We find the worst item by just doing the search algorithm, and seeing
						// returning the item that is worse (greater)
						Ordering::Greater => item1,
						Ordering::Less => item2,
						Ordering::Equal => item1,
					}
				});

				// If there's only 1 item in the Vec, then just get the first one since it's the
				// "worst"
				let worst_item = match worst_item {
					Some(item) => item,
					None => results.get_mut(0).unwrap(),
				};

				if search_alg(&item, worst_item, &download_count_db) == Ordering::Less {
					*worst_item = item;
				}
			}
		}
	});

	let results: Vec<SearchResult> = results
		.into_iter()
		.map(|item| SearchResult::from_item(item, &download_count_db))
		.collect();

	Ok(Response::builder()
		.status(StatusCode::OK)
		.body(simd_json::to_string(&results).unwrap())
		.unwrap())
}

// For each SoftwareItem, the database contains a list of users who have downloaded that software
// and the time they downloaded it. Thanks to BTreeSet and my magic ordering, it should sort itself
#[derive(Serialize, Deserialize)]
pub struct SoftwareDownloads(pub BTreeSet<SoftwareDownload>);

// I do this so the BTreeSet only sorts by the time, but I can still store the username
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub struct SoftwareDownload {
	time: UnixTimeUTC,
	username: String,
}

impl PartialOrd for SoftwareDownload {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for SoftwareDownload {
	fn cmp(&self, other: &Self) -> Ordering {
		self.time.cmp(&other.time)
	}
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixTimeUTC(pub u64);

impl UnixTimeUTC {
	fn now_utc() -> Self {
		let time = OffsetDateTime::now_utc();
		let unix_time: u64 = time.unix_timestamp().try_into().unwrap();

		Self(unix_time)
	}
}

#[derive(Serialize, Deserialize)]
pub struct SoftwareOwnershipInfoList(HashMap<u64, OwnershipInfo>);

#[derive(Serialize, Deserialize)]
pub struct OwnershipInfo {
	downloaded: bool,
}

pub async fn download_software(
	websocket: warp::ws::Ws, cookie_db: CookieDB, software_db: SoftwareDB,
	download_db: DownloadCountDB, ownership_db: SoftwareOwnershipDB,
) -> Result<impl Reply, Rejection> {
	Ok(websocket.on_upgrade(|ws| async move {
		let (mut cli_ws_snd, mut cli_ws_rcv) = ws.split();

		'connection: while let Some(result) = cli_ws_rcv.next().await {
			match result {
				Ok(msg) => {
					let msg = msg.as_bytes();

					// The first 32 bytes is the auth cookie, andt the final 8 bytes is the id of the software they want to download
					if msg.is_empty() {
						println!("Closing connection");
						break 'connection;
					}

					let username_end_index = msg.iter().position(|b| *b == 0_u8).unwrap();
					let username = String::from_utf8(msg[..username_end_index].to_vec()).unwrap();
					// TODO: DONT JUST ASSUME AUTH COOKIE IS 50 BYTES, THIS IS LAZY
					let auth_cookie = String::from_utf8(
						msg[username_end_index + 1..username_end_index + 1 + 50].to_vec(),
					)
					.unwrap();

					if !verify_auth_cookie(&username, &auth_cookie, &cookie_db) {
						eprintln!("Invalid auth cookie");
						break 'connection;
					}

					let software_id = u64::from_le_bytes(
						msg[username_end_index + 52..username_end_index + 52 + 8]
							.try_into()
							.unwrap(),
					);
					let software_info = match software_db.get(&software_id).unwrap() {
						Some(item) => item,
						None => {
							eprintln!("Software not found");
							break 'connection;
						},
					};

					for (file_hash, file_name) in software_info.files.iter() {
						let file_hash_hex =
							hex_simd::encode_to_boxed_str(file_hash, hex_simd::AsciiCase::Lower);
						let mut file_path = PathBuf::new();
						file_path.push("./.game_files/");
						file_path.push(&*file_hash_hex);

						let mut file = tokio::fs::OpenOptions::new()
							.read(true)
							.open(file_path)
							.await
							.unwrap();
						let file_size = {
							let file_metadata = file.metadata().await.unwrap();
							file_metadata.len()
						};

						// First, send the path that the file should be saved to on the client side
						// as well as the size of the file
						let mut message = Vec::with_capacity(32 + file_name.len() + 1 + 8);
						message.extend_from_slice(file_hash);
						message.extend_from_slice(file_name.as_bytes());
						message.push(0);
						message.extend_from_slice(&file_size.to_le_bytes());

						cli_ws_snd.send(Message::binary(message)).await.unwrap();

						// Then, start reading the file and sending it over the network
						let mut file_buffer = [0_u8; 8192];

						'read_loop: loop {
							match file.read(&mut file_buffer).await {
								Ok(0) => break 'read_loop,
								Ok(n) => {
									cli_ws_snd
										.send(Message::binary(&file_buffer[..n]))
										.await
										.unwrap();
								},
								Err(ref err) => match err.kind() {
									tokio::io::ErrorKind::Interrupted => continue,
									_ => panic!("{err}"),
								},
							};
						}
					}

					let add_to_download_count = || {
						let mut downloads = match download_db.get(&software_id).unwrap() {
							Some(downloads) => downloads,
							None => SoftwareDownloads(BTreeSet::new()),
						};

						let download_info = SoftwareDownload {
							username: username.clone(),
							time: UnixTimeUTC::now_utc(),
						};

						downloads.0.insert(download_info);
						download_db.insert(&software_id, &downloads).unwrap();

						println!(
							"{} total downloads for software_id: {software_id}",
							downloads.0.len()
						);
					};

					let mut ownership_info_list = ownership_db
						.get_or_insert(&username, &SoftwareOwnershipInfoList(HashMap::new()))
						.unwrap();

					match ownership_info_list.0.get_mut(&software_id) {
						Some(ownership_info) => {
							if !ownership_info.downloaded {
								add_to_download_count();
								ownership_info.downloaded = true;
							}
						},
						None => {
							ownership_info_list.0.insert(
								software_id,
								OwnershipInfo {
									downloaded: true,
								},
							);
							add_to_download_count();
						},
					};

					ownership_db
						.insert(&username, &ownership_info_list)
						.unwrap();

					println!("Finished sending files");
				},
				Err(err) => {
					eprintln!("Error while downloading software: {err}");
					break 'connection;
				},
			};
		}
	}))
}

fn newest_item(item1: &Item, item2: &Item, _download_count_db: &DownloadCountDB) -> Ordering {
	item2.id.cmp(&item1.id)
}

fn oldest_item(item1: &Item, item2: &Item, _download_count_db: &DownloadCountDB) -> Ordering {
	item1.id.cmp(&item2.id)
}

fn most_downloaded_item(
	item1: &Item, item2: &Item, download_count_db: &DownloadCountDB,
) -> Ordering {
	let count1 = download_count_db
		.get_or_insert(&item1.id, &SoftwareDownloads(BTreeSet::new()))
		.unwrap();
	let count2 = download_count_db
		.get_or_insert(&item2.id, &SoftwareDownloads(BTreeSet::new()))
		.unwrap();

	count2.0.len().cmp(&count1.0.len())
}

fn least_downloaded_item(
	item1: &Item, item2: &Item, download_count_db: &DownloadCountDB,
) -> Ordering {
	let count1 = download_count_db
		.get_or_insert(&item1.id, &SoftwareDownloads(BTreeSet::new()))
		.unwrap();
	let count2 = download_count_db
		.get_or_insert(&item2.id, &SoftwareDownloads(BTreeSet::new()))
		.unwrap();

	count1.0.len().cmp(&count2.0.len())
}

pub struct SoftwareInfo {
	username: String,
	auth_cookie: String,
	upload_id: [u8; 32],
	num_chunks: u64,
	compressed_image_len: u32,
	files: HashMap<blake3::Hash, String>,
}

pub struct ChunkInfo {
	len: u64,
	file: blake3::Hash,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ItemPrice {
	// For the pay what you want modle, the min and max must be the same CurrencyType
	PayWhatYouWant {
		min: CurrencyAmount,
		max: CurrencyAmount,
	},
	FixedAmount(CurrencyAmount),
	None,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum CurrencyAmount {
	// The price is listed in Monero
	Monero(u64),
	// The price is listed in a fiat currency
	Fiat(Decimal),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
	id: u64,
	title: String,
	creator: String,
	description: String,
	arch_type: ArchType,
	compressed_size: u64,
	price: ItemPrice,
	compression_type: CompressionType,
	files: HashMap<[u8; 32], String>,
	upload_id: Option<[u8; 32]>,
	creation_time: Option<u64>,
	title_image_hash: [u8; 32],
}

// TODO: Make view_captcha use async and look like this function
pub async fn view_software_image(
	path: String, ip: Option<SocketAddr>, log_sender: Sender<Log>,
) -> Result<Box<dyn Reply>, Rejection> {
	let mut path_bytes = path.clone().into_bytes();

	// Make sure the path given is 32 byte hex
	if path.len() == 64 && hex_simd::decode_inplace(&mut path_bytes).is_ok() {
		let mut full_path = PathBuf::new();
		full_path.push("./.game_images/");
		full_path.push(&path);

		match File::open(&full_path).await {
			Ok(mut file) => {
				let mut bytes = Vec::new();
				file.read_to_end(&mut bytes).await.unwrap();

				Ok(Box::new(Response::builder().body(bytes).unwrap()))
			},
			Err(err) => match err.kind() {
				tokio::io::ErrorKind::NotFound => {
					let denial = RequestDenial::new(
						DenialFault::User,
						"Image not found".to_string(),
						String::new(),
					);

					Ok(Box::new(denial.into_response(StatusCode::NOT_FOUND)))
				},
				_ => {
					Log::new(
						None,
						ip.unwrap().ip(),
						Event::ServerError,
						LogType::Auth,
						err.to_string(),
						Priority::Medium,
					)
					.send(&log_sender)
					.await;
					let denial = RequestDenial::new(
						DenialFault::Server,
						"Internal Server Error".to_string(),
						String::new(),
					);
					Ok(Box::new(
						denial.into_response(StatusCode::INTERNAL_SERVER_ERROR),
					))
				},
			},
		}
	} else {
		let denial = RequestDenial::new(
			DenialFault::User,
			"Image not found".to_string(),
			String::new(),
		);

		Ok(Box::new(denial.into_response(StatusCode::NOT_FOUND)))
	}
}
