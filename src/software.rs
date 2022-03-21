use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::auth::verify_auth_cookie;

use arrayvec::ArrayString;
use blake3::Hash;
use futures::{FutureExt, StreamExt};
use rand::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufWriter, AsyncReadExt};
use warp::{Rejection, Reply};
use warp::hyper::{Response, StatusCode};
use tokio::sync::mpsc;
use tokio::io::BufReader;
use tokio_stream::wrappers::UnboundedReceiverStream;
use async_compression::tokio::write::ZstdDecoder;

use crate::{DenialFault, RequestDenial};
use crate::db::{AuthDB, SoftwareDB, CaptchaDB, DB, CookieDB, UploadIdDB};

#[derive(Serialize, Deserialize)]
pub enum ArchType {
	/// WASM32 or anything else meant to run in a web browser
	Web,
	X86_64,
}

///While Zstd is the preferred compression algorithm, the current rust support is bindings to the C library.
/// This means that for the web version of the site, it can't be used. As an alternative, Brotli is used instead
#[derive(Serialize, Deserialize)]
pub enum CompressionType {
	ZSTD,
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
	compressed_software_hash: ArrayString<{ 2 * blake3::OUT_LEN }>,
	captcha_hash: ArrayString<{ 2 * blake3::OUT_LEN }>,
	captcha_answer: String,

}


#[derive(Serialize)]
pub struct NewSoftwareResp {
	upload_id: ArrayString<{ 2 * blake3::OUT_LEN }>,
}

pub fn new_software(new_software_req: CreateSoftwareReq, auth_db: AuthDB, cookie_db: CookieDB, software_db: SoftwareDB, upload_id_db: UploadIdDB, captcha_db: CaptchaDB, current_soft_id: Arc<AtomicU64>) -> Response<String> {
	let bad_captcha = || -> Response<String> {
		let denial = RequestDenial::new(
			DenialFault::User, 
			"Incorrect captcha".to_string(), 
			String::new()
		);

		denial.into_response(StatusCode::UNAUTHORIZED)
	};

	let captcha_hash = match hex_simd::decode_to_boxed_bytes(new_software_req.captcha_hash.as_bytes()) {
		Ok(captcha_hash) => captcha_hash,
		Err(_err) => {
			return bad_captcha();
		}

	};

	let captcha = match captcha_db.get(captcha_hash.as_ref().try_into().unwrap()).unwrap() {
		Some(captcha) => captcha,
		None => {
			return bad_captcha();
		}
	};

	if captcha.answer != new_software_req.captcha_answer {
		return bad_captcha();

	}

	if !auth_db.contains_key(&new_software_req.username).unwrap() || !verify_auth_cookie(&new_software_req.username, &new_software_req.auth_cookie, &cookie_db) {
		let denial = RequestDenial::new(DenialFault::User, String::from("Invalid username or cookie"), String::new());
		return denial.into_response(StatusCode::UNAUTHORIZED);

	}

	// While obviously modified clients can just lie about how large the game's size is, there are strict checks when actually uplaoading the game to ensure that the file is the exact size specified
	if new_software_req.compressed_size > 10_000_000 /*10 MB*/ {
		let denial = RequestDenial::new(DenialFault::User, String::from("File is too large"), "Please keep your compressed game files under 10 MB".to_string());
		return denial.into_response(StatusCode::BAD_REQUEST);

	}

	let compressed_hash = match hex_simd::decode_to_boxed_bytes(new_software_req.compressed_software_hash.as_bytes()) {
		Ok(hash) => {
			match hash.len() == 32 {
				true => {
					let mut hash_array: [u8; blake3::OUT_LEN] = [0; blake3::OUT_LEN];
					hash_array.copy_from_slice(&hash);

					hash_array
				},
				false => {
					let denial = RequestDenial::new(DenialFault::User, "Invalid hex string".to_string(), String::new());
					return denial.into_response(StatusCode::UNAUTHORIZED);
				}

			}

		},
		Err(_err) => {
			let denial = RequestDenial::new(DenialFault::User, "Invalid hex string".to_string(), String::new());
			return denial.into_response(StatusCode::UNAUTHORIZED);
		},
	};

	let software_id = current_soft_id.fetch_add(1, Ordering::Relaxed);
	let mut random_bytes: [u8; 32] = [0; 32];

	thread_rng().fill_bytes(&mut random_bytes);

	let mut bytes_to_hash = Vec::with_capacity(random_bytes.len() + new_software_req.title.len() + new_software_req.username.len());

	bytes_to_hash.extend_from_slice(&random_bytes);
	bytes_to_hash.extend_from_slice(new_software_req.title.as_bytes());
	bytes_to_hash.extend_from_slice(new_software_req.username.as_bytes());

	// Basically just a way of generating a pretty random ID
	let upload_id_bytes = blake3::hash(&bytes_to_hash);
	let upload_id_hex = hex_simd::encode_to_boxed_str(upload_id_bytes.as_bytes(), hex_simd::AsciiCase::Lower);

	let mut upload_id_str = ArrayString::new();
	upload_id_str.push_str(&upload_id_hex);

	let game = Item {
	    id: software_id,
	    title: new_software_req.title,
	    creator: new_software_req.username,
	    description: new_software_req.description,
	    arch_type: new_software_req.arch_type,
	    compression_type: new_software_req.compression_type,
	    compressed_size: new_software_req.compressed_size,
	    price: new_software_req.price,
	    compressed_hash,
	    upload_id: Some(upload_id_str),
	};

	software_db.insert(&software_id, &game).unwrap();
	upload_id_db.insert(upload_id_bytes.as_bytes(), &software_id).unwrap();

	let resp = NewSoftwareResp {
		upload_id: upload_id_str,
	};

	Response::builder()
		.status(StatusCode::OK)
		.body(simd_json::to_string(&resp).unwrap())
		.unwrap()
	
}

pub async fn upload_software(websocket: warp::ws::Ws, auth_db: AuthDB, cookie_db: CookieDB, upload_id_db: UploadIdDB, _software_db: SoftwareDB) -> Result<impl Reply, Rejection >{
	Ok(websocket.on_upgrade(|ws| async move {
		let (cli_ws_snd, mut cli_ws_rcv) = ws.split();
		let (_cli_snd, cli_rcv) = mpsc::unbounded_channel();

		let cli_rcv = UnboundedReceiverStream::new(cli_rcv);

		tokio::task::spawn(cli_rcv.forward(cli_ws_snd).map(|result| {
	        if let Err(e) = result {
	            println!("error sending websocket msg: {}", e);
	        }
	    }));

	    let mut software_info: Option<SoftwareInfo> = None;

        let mut num_chunks = 0;
        let mut amt_written_to_buffer = 0;
		let mut chunk_info: Option<ChunkInfo> = None;
        const CHUNK_BUFFER_LEN: usize = 65535;
	    let mut chunk_buffer: [u8; CHUNK_BUFFER_LEN] = [0; CHUNK_BUFFER_LEN];

	    while let Some(result) = cli_ws_rcv.next().await {
	        let _msg = match result {
	            Ok(msg) => {
                    let msg = msg.as_bytes();

	            	if let Some(software_info) = software_info.as_ref() {
                        if num_chunks == software_info.num_chunks {
                            let mut path = PathBuf::new();
                            path.push(".local/share/coinstribute/");
                            
                            for (hash, file) in software_info.files.iter() {
                                let mut path = PathBuf::new();
                                path.push(".cache/coinstribute/");
                                path.push(&hash.to_string());

                                let mut source = None;

                                while source.is_none() {
                                    tokio::time::sleep(Duration::from_secs(1)).await;

                                    source = match tokio::fs::OpenOptions::new().read(true).open(&path).await {
                                            Ok(source) => Some(source),
                                            Err(err) => {
                                                println!("Error on file read: {err:?}");
                                                None

                                            },

                                    };

                                }

                                let source = source.unwrap();

                                let dst = tokio::fs::OpenOptions::new()
                                    .write(true)
                                    .create(true)
                                    .open(&file)
                                    .await
                                    .unwrap();

                                let mut zstd = ZstdDecoder::new(dst);

                                // Before decompressing, check the hash of the file

                                let mut hasher = blake3::Hasher::new();
                                let hash_buffer = &mut chunk_buffer;
                                let mut file_src = BufReader::new(source);
                                let mut pos = 0;
                                
                                loop {
                                    let num_bytes_read = file_src.read(&mut hash_buffer[pos..]).await.unwrap();

                                    if num_bytes_read > 0 {
                                        pos += num_bytes_read; 

                                        if pos == hash_buffer.len() {
                                            tokio::task::block_in_place(|| hasher.update_rayon(&hash_buffer[..]));
                                            pos = 0;

                                        }

                                    // If num bytes read is 0, that means we've finished
                                    // reading from the file
                                    } else {
                                        tokio::task::block_in_place(|| hasher.update(&hash_buffer[..pos]));
                                        break;

                                    }


                                }

                                let computed_hash = hasher.finalize();

                                // TODO: Remove the whole cached folder, not just the single
                                // corrupted file
                                let remove_cached_file = || async { tokio::fs::remove_file(&path).await.unwrap() };

                                if *hash == computed_hash {

                                    println!("Hashes are equivalent");

                                } else {
                                    remove_cached_file().await;
                                    panic!("Hash: {}\nComputed Hash: {}", hash, computed_hash);

                                }

                                let source = tokio::fs::OpenOptions::new().read(true).open(&path).await.unwrap(); 

                                let mut src = BufReader::new(source.try_clone().await.unwrap());

                                tokio::io::copy_buf(&mut src, &mut zstd).await.unwrap();
                                remove_cached_file().await;

                            }

                            break;

                        }

						if let Some(chunk_info_ref) = chunk_info.as_ref() {
                            chunk_buffer[..(amt_written_to_buffer + msg.len())].as_mut().copy_from_slice(msg);
                            amt_written_to_buffer += msg.len();

                            let amt_written_to_buffer_as_u64: u64 = amt_written_to_buffer.try_into().unwrap();

                            if amt_written_to_buffer_as_u64 == chunk_info_ref.len {
                                // When we've finished writing a single chunk to memory, begin
                                // writing it to disk
                                let chunk_buffer_clone = chunk_buffer;
                                let file_hash = chunk_info_ref.file;

                                tokio::task::spawn(async move {
                                    let chunk_buffer = &chunk_buffer_clone[..amt_written_to_buffer];
                                    let mut path = PathBuf::new();
                                    path.push(".cache/coinstribute/");

                                    tokio::fs::create_dir_all(path.clone()).await.unwrap();
                                    path.push(&file_hash.to_string());
                                    let mut file = tokio::fs::OpenOptions::new()
                                        .append(true)
                                        .create(true)
                                        .open(path)
                                        .await
                                        .unwrap();
                                   
                                    file.write_all(chunk_buffer).await.unwrap();

                                });

                                // Reset the chunk buffer stuff
                                chunk_buffer = [0; CHUNK_BUFFER_LEN];
                                amt_written_to_buffer = 0;
                                chunk_info = None;

                                // TODO: Only increment num_chunks if the file is fully written
                                num_chunks += 1;

                            }

		            	} else {
		            		let chunk_info_bytes = &msg;
		            		chunk_info = Some(ChunkInfo {
		            			len: u64::from_le_bytes(chunk_info_bytes[0..8].try_into().unwrap()),
		            			file: {
		            				let checksum_array: [u8; 32] = chunk_info_bytes[8..40].try_into().unwrap();
		            				Hash::from(checksum_array)
		            			},
		            		});

		            	}
	            	} else {
	            		let software_info_bytes = &msg;

	            		//TODO: Super easy way to crash server here
	            		let num_chunks = u64::from_le_bytes(software_info_bytes[0..8].try_into().unwrap());
	            		let (auth_cookie, cookie_end_index) = {
	            			let auth_cookie_end_index: usize = software_info_bytes[8..].iter().position(|b| *b == 0).unwrap() + 8;
                            (String::from_utf8(software_info_bytes[8..auth_cookie_end_index].to_vec()), auth_cookie_end_index)
	            		};
                        let auth_cookie = auth_cookie.unwrap();

                        let (upload_id, id_end_index) = {
                            let end_index: usize = (software_info_bytes[cookie_end_index + 1..]).iter().position(|b| *b == 0).unwrap();
                            

                            (String::from_utf8(software_info_bytes[cookie_end_index - 1..end_index + cookie_end_index].to_vec()), end_index)
                            
                        };


                        let upload_id = upload_id.unwrap();
                        
                        let mut files = HashMap::new();

                        let mut bytes_iter = software_info_bytes[id_end_index + cookie_end_index + 2..].iter();

                        loop {
                            let mut checksum = [0; blake3::OUT_LEN];

                            if bytes_iter.size_hint().0 == 0 {
                                break;

                            }

                            let checksum_iter = bytes_iter.by_ref().take(blake3::OUT_LEN);

                            checksum_iter.into_iter().zip(checksum.iter_mut()).for_each(|(byte, chk_b)| {
                                *chk_b = *byte;
                                
                            }); 

                            let filename_iter = bytes_iter.by_ref().take_while(|b| **b != 0).copied();

                            let filename_vec = filename_iter.collect::<Vec<u8>>();
                            let filename = String::from_utf8_lossy(&filename_vec);

                            files.insert(Hash::from(checksum), filename.to_string());
                        };

	            		// Since each chunk can be up to a maximum of 8192 bytes, and the maximum size per game is 10_485_760 bytes, meaning max num of chunks is 1280
	            		const MAX_NUM_CHUNKS: u64 = 10_485_760 / 8192; // 1280 chunks

	            		if num_chunks <= MAX_NUM_CHUNKS {
                            software_info = Some(SoftwareInfo { 
                               auth_cookie,
                               upload_id,
                               num_chunks,
                               files,

                            });

	            		} else {
                            todo!("{num_chunks}")

	            		}

	            	}
	            	
	            },
	            Err(err) => {
	                println!("error receiving message {err:?}");
	                break;
	            }
	        };
	    }



	}))

}

pub struct SoftwareInfo {
	auth_cookie: String,
	upload_id: String,
    num_chunks: u64,
    files: HashMap<blake3::Hash, String>, 
}

pub struct ChunkInfo {
	len: u64,
	file: blake3::Hash,
}

#[derive(Serialize, Deserialize)]
pub enum ItemPrice {
	// For the pay what you want modle, the min and max must be the same CurrencyType
	PayWhatYouWant {
		min: CurrencyAmount,
		max: CurrencyAmount,
	},
	FixedAmount(CurrencyAmount),
	None,
}

#[derive(Serialize, Deserialize)]
pub enum CurrencyAmount {
	// The price is listed in Monero
	Monero(u64),
	// The price is listed in a fiat currency
	Fiat(Decimal),
}

#[derive(Serialize, Deserialize)]
pub struct Item {
	id: u64,
	title: String,
	creator: String,
	description: String,
	arch_type: ArchType,
	compressed_size: u64,
	price: ItemPrice,
	compression_type: CompressionType,
	compressed_hash: [u8; blake3::OUT_LEN],
	upload_id: Option<ArrayString<{ 2 * blake3::OUT_LEN }>>,
}
