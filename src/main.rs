use ureq::{Agent};
use std::time::Instant;
use std::io::Read;
use std::sync::{Arc};
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::vec;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests;

mod locations;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

static CLOUDFLARE_SPEEDTEST_DOWNLOAD_URL : &str = "https://speed.cloudflare.com/__down?measId=0";
static CLOUDFLARE_SPEEDTEST_UPLOAD_URL : &str = "https://speed.cloudflare.com/__up?measId=0";
static CLOUDFLARE_SPEEDTEST_SERVER_URL : &str = "https://speed.cloudflare.com/__down?measId=0&bytes=0";
static CLOUDFLARE_SPEEDTEST_CGI_URL : &str = "https://speed.cloudflare.com/cdn-cgi/trace";
static OUR_USER_AGENT : &str = "cf_speedtest (0.30)";

impl std::io::Read for UploadHelper {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		// upload is finished, or we are exiting
		if self.byte_ctr.load(Ordering::SeqCst) >= self.bytes_to_send || self.exit_signal.load(Ordering::SeqCst) {
			// dbg!("Exiting");
			return Ok(0);
		}

		// fill the buffer with 1s
		for i in 0..buf.len() {
			buf[i] = 1;
		}

		self.byte_ctr.fetch_add(buf.len() as u64, Ordering::SeqCst);
		self.total_uploaded_counter.fetch_add(buf.len() as u64, Ordering::SeqCst);
        Ok(buf.len())
    }
}


struct UploadHelper {
	bytes_to_send : u64,
	byte_ctr: Arc<AtomicU64>,
	total_uploaded_counter: Arc<AtomicU64>,
	exit_signal: Arc<AtomicBool>,
}

fn get_secs_since_unix_epoch() -> u64 {
	let start = SystemTime::now();
	let since_the_epoch = start.duration_since(UNIX_EPOCH)
		.unwrap();

	since_the_epoch.as_secs()
}
// Given n bytes, return
// 	a: unit of measurement in sensible form of bytes
// 	b: unit of measurement in sensible form of bits 
// i.e 12939428 -> (12.34 MB, 98.76 Mb)
// 		 814811 -> (795.8 KB, 6.36 Mb)
// basically, the BYTE value should always be greater than 1
// and never more than 1024. the bit value should just be calculated off
// the byte value
fn get_appropriate_byte_unit(bytes: u64) -> Result<(String, String)>{
	let mut bytes = bytes as f64;
	let byte_unit 	: 	char;
	let bit_unit 	: 	char;
	let mut bits 	: 	f64;

	if bytes < 1024.0 {
		byte_unit = '\0';
	} else if bytes < 1024i32.pow(2) as f64 {
		byte_unit = 'K';
		bytes /= 1024.0;
	} else if bytes < 1024i32.pow(3) as f64 {
		byte_unit = 'M';
		bytes /= 1024i32.pow(2) as f64;
	} else if bytes < 1024i32.pow(4) as f64 {
		byte_unit = 'G';
		bytes /= 1024i32.pow(3) as f64;
	} else {
		byte_unit = 'T';
		bytes /= 1024i32.pow(4) as f64;

	}
	
	bits = bytes * 8.;
	// increment the bit_unit by 1
	if bytes*8. > 1000. {
		bits /= 1000.0;
		// increment the bit_unit by one metric
		// i.e. k becomes m, m becomes g, g becomes t
		bit_unit = match byte_unit {
			'\0' => 'k',
			'B' => 'k',
			'K' => 'm',
			'M' => 'g',
			'G' => 't',
			'T' => 'p',

			_ => '?',
		};
	} else {
		// set bit unit to lowercase of byte unit
		bit_unit = byte_unit.to_ascii_lowercase();
	}

	Ok((format!("{:.2} {}B", bytes, byte_unit), format!("{:.2} {}b", bits, bit_unit)))
}

// Use cloudflare's cdn-cgi endpoint to get our ip address country
// (they use Maxmind)
fn get_our_ip_address_country() -> Result<String> {
	let resp = ureq::get(CLOUDFLARE_SPEEDTEST_CGI_URL).call()?;
	let mut body = String::new();
	resp.into_reader().read_to_string(&mut body)?;

	for line in body.lines() {
		if let Some(loc) = line.strip_prefix("loc=") {
			return Ok(loc.to_string());
		}
	}

	panic!("Could not find loc= in cdn-cgi response\n
			Please update to the latest version and make a Github issue if the issue persists");
}

// Get http latency by requesting the cgi endpoint 8 times
// and taking the fastest
fn get_download_server_http_latency() -> Result<std::time::Duration> {
	let start = Instant::now();
	let my_agent = ureq::AgentBuilder::new().build();
	let mut latency_vec = Vec::new();

	for _ in 0..8 {
		// if vec length 2 or greater and we've spent a lot of time
		// exit early
		if latency_vec.len() >= 2 && start.elapsed() > std::time::Duration::from_secs(1) {
			break;
		}

		let now = Instant::now();
		let _response = my_agent.get(CLOUDFLARE_SPEEDTEST_CGI_URL)
				.set("accept-encoding", "mcdonalds") // https://github.com/algesten/ureq/issues/549
				.call()?
				.into_string()?;
		
		let total_time = now.elapsed();
		latency_vec.push(total_time);
	}

	let best_time = latency_vec.iter().min().unwrap().to_owned();
	Ok(best_time)
}

// return all cloufdlare headers from a request
fn get_download_server_info() -> Result<std::collections::HashMap<String, String>> {
	let mut server_headers = std::collections::HashMap::new();
	let resp = ureq::get(CLOUDFLARE_SPEEDTEST_SERVER_URL).call().expect("Failed to get server info");

	for key in resp.headers_names() {
		if key.starts_with("cf-") {
			server_headers.insert(key.clone(), resp.header(&key).unwrap().to_string());
		}
	}

	Ok(server_headers)
}

// send cloudflare some bytes
fn upload_test(bytes: u64, total_up_bytes_counter: &Arc<AtomicU64>, exit_signal: &Arc<AtomicBool>) -> Result<()> {
	let agent = Agent::new();

	let upload_helper = UploadHelper{
			bytes_to_send: bytes,
			byte_ctr:  Arc::new(AtomicU64::new(0)),
			total_uploaded_counter: total_up_bytes_counter.clone(),
			exit_signal: exit_signal.clone(),
	};

	let resp = agent.post(CLOUDFLARE_SPEEDTEST_UPLOAD_URL)
		.set("Content-Type", "text/plain;charset=UTF-8")
		.set("User-Agent", OUR_USER_AGENT)
		.send(upload_helper)
		.expect("Couldn't create upload request");

	// read the POST response body into the void
	let _ = std::io::copy(&mut resp.into_reader(), &mut std::io::sink());

	Ok(())
}

// download some bytes from cloudflare
fn download_test(bytes: u64, total_bytes_counter: &Arc<AtomicU64>, current_down_speed: &Arc<AtomicU64>, exit_signal: &Arc<AtomicBool>) -> Result<()>
{
	// not using an agent because we want each thread
	// to have its own connection
	let resp = ureq::get(format!("{}&bytes={}", CLOUDFLARE_SPEEDTEST_DOWNLOAD_URL, bytes).as_str())
		.set("User-Agent", OUR_USER_AGENT)
		.call()
		.expect("Couldn't create download request");

	let mut resp_reader = resp.into_reader();
	let mut total_bytes_sank = 0;

	loop {
		// exit if we have passed deadline
		if exit_signal.load(Ordering::Relaxed) {
			break;
		}

		// if we are fast, take big chunks
		// if we are slow, take small chunks
		let current_down_speed = current_down_speed.load(Ordering::Relaxed);
		let current_recv_buff = match current_down_speed{
			0..=1000 => 4,
			1001..=10000 => 32,
			10001..=100000 => 512,
			100001..=1000000 => 4096,
			1000001..=10000000 => 16384,
			_ => 16384,
		};

		// copy bytes into the void
		let bytes_sank = std::io::copy(&mut resp_reader.by_ref().take(current_recv_buff), &mut std::io::sink())?;

		if bytes_sank == 0 {
			if total_bytes_sank == 0 {
				panic!("Cloudflare is sending us empty responses?!")
			}
			
			break;
		}
		total_bytes_sank += bytes_sank;
		total_bytes_counter.fetch_add(bytes_sank, Ordering::SeqCst);
	}

	Ok(())
}

fn main() {
	let download_thread_count = 4;
	let upload_thread_count = 4;

	let now = chrono::Local::now();
	println!("{:<32} {} {}", 
				"Start:",
				now.format("%Y-%m-%d %H:%M:%S"), 
				now.format("%Z"));


	let iata_mapping = locations::generate_iata_to_city_map();
	let country_mapping = locations::generate_cca2_to_full_country_name_map();

	let our_country = get_our_ip_address_country().expect("Couldn't get our country");
	let our_country_full = country_mapping.get(&our_country as &str);
	let latency = get_download_server_http_latency().expect("Couldn't get server latency");
	let headers = get_download_server_info().expect("Couldn't get download server info");

	let unknown_colo = &"???".to_owned();
	let unknown_colo_info = &("UNKNOWN", "UNKNOWN");
	let cf_colo = headers.get("cf-meta-colo").unwrap_or(unknown_colo);
	let colo_info = iata_mapping.get(cf_colo as &str).unwrap_or(unknown_colo_info);

	println!("{:<32} {}", "Your Location:", our_country_full.unwrap_or(&"UNKNOWN"));
	println!("{:<32} {} - {}, {}", 
				"Server Location:",
				cf_colo, 
				colo_info.0, 
				country_mapping.get(colo_info.1).unwrap_or(&"UNKNOWN"));

	println!("{:<32} {:.2}ms\n", "Latency (HTTP):", latency.as_millis());

	let total_downloaded_bytes_counter = Arc::new(AtomicU64::new(0));
	let total_uploaded_bytes_counter = Arc::new(AtomicU64::new(0));

	let current_down_speed = Arc::new(AtomicU64::new(0));

	const BYTES_TO_UPLOAD: u64 = 50 * 1024 * 1024;
	const BYTES_TO_DOWNLOAD: u64 = 50 * 1024 * 1024;

	let mut down_deadline = get_secs_since_unix_epoch() + 12;
	let exit_signal = Arc::new(AtomicBool::new(false)); 

	let mut down_handles = vec![];
	for i in 0..download_thread_count {
		let total_downloaded_bytes_counter = Arc::clone(&total_downloaded_bytes_counter.clone());
		let current_down_clone = Arc::clone(&current_down_speed.clone());
		let exit_signal_clone = Arc::clone(&exit_signal.clone());
		let handle = std::thread::spawn(move || {
			// sleep a little to hit a new cloudflare metal
			// (each metal will throttle to 1 gigabit per ip in my testing)
			std::thread::sleep(std::time::Duration::from_millis(i*250));
			//println!("Thread {i} starting...");
			loop {
				let result = download_test(BYTES_TO_DOWNLOAD, &total_downloaded_bytes_counter, &current_down_clone, &exit_signal_clone);
				match result {
					Ok(_) => {},
					Err(e) => {
						println!("Error in download test thread {}: {:?}", i, e);
						return;
					}
				}

				// exit if we have passed the deadline
				if exit_signal_clone.load(Ordering::Relaxed) {
					// println!("Thread {} exiting...", i);
					return;
				}
			}
		});
		down_handles.push(handle);
	}

	let mut last_bytes_down = 0;
	total_downloaded_bytes_counter.store(0, Ordering::SeqCst);

	let mut down_measurements = vec![];

	// print download speed
	// adaptively spawn more threads if we are getting increasingly faster
	loop {
		let bytes_down = total_downloaded_bytes_counter.load(Ordering::Relaxed);
		let bytes_down_diff = bytes_down - last_bytes_down;

		// set current_down
		current_down_speed.store(bytes_down_diff, Ordering::SeqCst);
		down_measurements.push(bytes_down_diff);

		let speed_values = get_appropriate_byte_unit(bytes_down_diff).unwrap();
		// only print progress if we are before deadline
		if get_secs_since_unix_epoch() < down_deadline {
			println!("Download: {byte_speed:>12.*}/s {bit_speed:>14.*}it/s", 
					16,
					16,
					byte_speed = speed_values.0, 
					bit_speed=speed_values.1);
		}

		if down_measurements.len() > 6 {
			// average the last 3 elements to the previous 3
			// and compare them
			let last_3 = &down_measurements[down_measurements.len()-3..];
			let prev_3 = &down_measurements[down_measurements.len()-6..down_measurements.len()-3];
			let last_3_avg = last_3.iter().sum::<u64>() / 3;
			let prev_3_avg = prev_3.iter().sum::<u64>() / 3;

			// if last 3 is greater than previous 3 + 20% spawn another thread
			if last_3_avg as f64 > prev_3_avg as f64 + ((prev_3_avg as f64/3.0)*0.2) {
				// extend the deadline slightly
				down_deadline += 1;

				let total_downloaded_bytes_counter = Arc::clone(&total_downloaded_bytes_counter.clone());
				let current_down_clone = Arc::clone(&current_down_speed.clone());
				let exit_signal_clone = Arc::clone(&exit_signal.clone());
				let handle = std::thread::spawn(move || {
					std::thread::sleep(std::time::Duration::from_millis(250));
					// println!("Starting new thread");
					loop {
						let result = download_test(BYTES_TO_DOWNLOAD, &total_downloaded_bytes_counter, &current_down_clone, &exit_signal_clone);
						match result {
							Ok(_) => {},
							Err(e) => {
								println!("Error in download test thread {:?}", e);
								return;
							}
						}

						// exit if we have passed the deadline
						if exit_signal_clone.load(Ordering::Relaxed) {
							//println!("Thread {} exiting...", i);
							return;
						}
					}
				});
				down_handles.push(handle);
			}

		}
		
		
		std::thread::sleep(std::time::Duration::from_millis(1000));

		last_bytes_down = bytes_down;

		// dbg print seconds until deadline
		// dbg!(down_deadline - get_secs_since_unix_epoch());

		// exit if we have passed the deadline
		if get_secs_since_unix_epoch() > down_deadline {
			exit_signal.store(true, Ordering::SeqCst);
			break;
		}
	}

	println!("Waiting for download threads to finish...");
	for handle in down_handles {
		handle.join().expect("Couldn't join download thread");
	}

	// re-use exit_signal for upload tests
	exit_signal.store(false, Ordering::SeqCst);

	println!("Starting upload tests...");
	let mut up_deadline = get_secs_since_unix_epoch() + 12;

	// spawn x uploader threads
	let mut up_handles = vec![];
	for i in 0..upload_thread_count {
		let total_bytes_uploaded_counter = Arc::clone(&total_uploaded_bytes_counter);
		let exit_signal_clone = Arc::clone(&exit_signal);
		let handle = std::thread::spawn(move || {
			loop {
				let result = upload_test(BYTES_TO_UPLOAD, &total_bytes_uploaded_counter, &exit_signal_clone);
				match result {
					Ok(_) => {},
					Err(e) => {
						println!("Error in upload test thread {}: {:?}", i, e);
					}
				}

				// exit if we have passed the deadline
				if get_secs_since_unix_epoch() > up_deadline {
					return;
				}
			}
		});
		up_handles.push(handle);
	}

	let mut last_bytes_up 	= 0;
	let mut up_measurements = vec![];
	total_uploaded_bytes_counter.store(0, Ordering::SeqCst);
	// print total bytes downloaded in a loop
	loop {
		
		let bytes_up = total_uploaded_bytes_counter.load(Ordering::Relaxed);

		let bytes_up_diff = bytes_up - last_bytes_up;
		up_measurements.push(bytes_up_diff);

		let speed_values = get_appropriate_byte_unit(bytes_up_diff).unwrap();

		println!("Upload: {byte_speed:>14.*}/s {bit_speed:>14.*}it/s", 
				16,
				16,
				byte_speed = speed_values.0, 
				bit_speed =	 speed_values.1);

		if up_measurements.len() > 6 {
			// average the last 3 elements to the previous 3
			// and compare them
			let last_3 = &up_measurements[up_measurements.len()-3..];
			let prev_3 = &up_measurements[up_measurements.len()-6..up_measurements.len()-3];
			let last_3_avg = last_3.iter().sum::<u64>() / 3;
			let prev_3_avg = prev_3.iter().sum::<u64>() / 3;

			// if last 3 is greater than previous 3 + 20% spawn another thread
			if last_3_avg as f64 > prev_3_avg as f64 + ((prev_3_avg as f64/3.0)*0.2) {
				// extend the deadline slightly
				up_deadline += 1;

				let total_bytes_uploaded_counter = Arc::clone(&total_uploaded_bytes_counter.clone());
				let exit_signal_clone = Arc::clone(&exit_signal.clone());
				let handle = std::thread::spawn(move || {
					// println!("Starting new thread");
					loop {
						let result = upload_test(BYTES_TO_UPLOAD, &total_bytes_uploaded_counter, &exit_signal_clone);
						match result {
							Ok(_) => {},
							Err(e) => {
								println!("Error in upload test thread {:?}", e);
								return;
							}
						}

						// exit if we have passed the deadline
						if exit_signal_clone.load(Ordering::Relaxed) {
							//println!("Thread {} exiting...", i);
							return;
						}
					}
				});
				up_handles.push(handle);
			}

		}
		
		std::thread::sleep(std::time::Duration::from_millis(1000));
		
		last_bytes_up = bytes_up;

		// exit if we have passed the deadline
		if get_secs_since_unix_epoch() > up_deadline {
			exit_signal.store(true, Ordering::SeqCst);
			break;
		}
	}

	// wait for upload threads to finish
	println!("Waiting for upload threads to finish...");
	for handle in up_handles {
		handle.join().expect("Couldn't join upload thread");
	}

	println!("Work complete!");

}