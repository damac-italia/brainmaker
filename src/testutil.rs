// SPDX-License-Identifier: GPL-3.0-or-later

//! Helpers for the tests that run the client against a local server.
//!
//! This module is built for tests only. It holds a signer whose key the client
//! under test trusts, a small HTTP server on the loopback address, and a builder
//! for zip archives. No test that uses it needs the network, a fixed port, or a
//! fixed path.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

/// What the server records about each request: the method, the path, and the
/// value of the `Authorization` header, or an empty string when it has none.
type Recorded = Vec<(String, String, String)>;

/// The longest request head that the server reads.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// How long the server waits for a client to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the server waits for a client to close after the reply.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// A fresh, empty directory under the system temporary directory.
///
/// The name holds the tag, the process id, the time in nanoseconds since the
/// epoch, and a counter, so that no two calls share a directory.
pub fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = std::env::temp_dir().join(format!(
        "brainmaker-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes bytes as lower-case hexadecimal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// An Ed25519 key pair that a test signs with.
pub struct Signer {
    pair: Ed25519KeyPair,
}

impl Signer {
    /// Makes a signer with a new key pair. The key exists in memory only.
    pub fn new() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("the system random source works");
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a generated key parses");
        Self { pair }
    }

    /// The public key as 64 hexadecimal characters, the form of `PUBLIC_KEYS`.
    pub fn public_hex(&self) -> String {
        hex(self.pair.public_key().as_ref())
    }

    /// The JSON that a signed route returns: the payload as a string, and the
    /// signature over the bytes of that string.
    pub fn envelope(&self, payload: &str) -> String {
        let signature = hex(self.pair.sign(payload.as_bytes()).as_ref());
        serde_json::json!({ "payload": payload, "signature": signature }).to_string()
    }

    /// Makes the client trust this signer's key, in the calling thread only.
    ///
    /// The key replaces both compiled-in key lists for that thread.
    pub fn trust(&self) {
        crate::signature::trust_in_this_test(&[self.public_hex()]);
    }
}

/// Returns a zip archive that holds the given (name, contents) pairs.
///
/// The archive is written to a file in a fresh temporary directory, read back,
/// and removed.
pub fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let dir = temp_dir("zip");
    let path = dir.join("archive.zip");

    let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    for (name, contents) in files {
        writer.start_file(*name, options).unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap();

    let bytes = fs::read(&path).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    bytes
}

/// A base URL that nothing listens on, which stands for a server that cannot be
/// reached.
///
/// The function takes a free port from the operating system and gives it back,
/// so a request to the URL fails to connect.
pub fn closed_port_base() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port is free");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// One answer that a [`Server`] gives: the request it matches, and the reply.
pub struct Route {
    pub method: &'static str,
    pub path: String,
    pub status: u16,
    pub body: Vec<u8>,
}

impl Route {
    /// A `GET` route that answers 200 with `body`.
    pub fn get(path: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "GET",
            path: path.to_string(),
            status: 200,
            body: body.into(),
        }
    }

    /// A `POST` route that answers 200 with `body`.
    pub fn post(path: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "POST",
            ..Self::get(path, body)
        }
    }
}

/// A minimal HTTP/1.1 server on the loopback address.
///
/// The server answers each request from a fixed list of routes, on one thread,
/// one connection at a time. A request that matches no route gets a 404. The
/// server records every request. Dropping the value stops the thread.
pub struct Server {
    port: u16,
    requests: Arc<Mutex<Recorded>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds a free loopback port and starts answering from `routes`.
    ///
    /// The first route with the method and the path of a request answers it.
    pub fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port is free");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Recorded::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve(listener, &routes, &requests, &stop))
        };

        Self {
            port,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// The base URL of the server, without a trailing slash.
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request that the server has answered so far, oldest first.
    pub fn requests(&self) -> Recorded {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // One connection makes the blocked `accept` return, so that the thread
        // reads the flag. With no wake-up, the join below could wait forever.
        let woken = TcpStream::connect(("127.0.0.1", self.port)).is_ok();
        if let (true, Some(thread)) = (woken, self.thread.take()) {
            let _ = thread.join();
        }
    }
}

/// Accepts connections and answers each one, until `stop` is set.
fn serve(listener: TcpListener, routes: &[Route], requests: &Mutex<Recorded>, stop: &AtomicBool) {
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Ok((stream, _)) = accepted {
            answer(stream, routes, requests);
        }
    }
}

/// Reads one request from `stream`, records it, and writes the reply.
fn answer(mut stream: TcpStream, routes: &[Route], requests: &Mutex<Recorded>) {
    // A client that connects and says nothing must not hold the server.
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));

    let Some(head) = read_head(&mut stream) else {
        return;
    };

    // The request needs its head only. A body, such as the form of a token
    // request, is never read as part of the request.
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let path = request_line.next().unwrap_or("").to_string();
    let authorization = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default();

    requests
        .lock()
        .unwrap()
        .push((method.clone(), path.clone(), authorization));

    let (status, body) = match routes
        .iter()
        .find(|route| route.method == method && route.path == path)
    {
        Some(route) => (route.status, route.body.clone()),
        None => (404, br#"{"error": "no such route"}"#.to_vec()),
    };
    let reason = if status == 200 { "OK" } else { "Error" };
    let mut reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(&body);

    let _ = stream.write_all(&reply);
    let _ = stream.flush();

    // Close the way a server that reads no body does: send the reply, end the
    // sending side, and read what the client still sends until it closes. A
    // close with unread bytes in the buffer resets the connection, and a reset
    // can destroy the reply before the client reads it.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(CLOSE_TIMEOUT));
    let mut sink = [0u8; 1024];
    while matches!(stream.read(&mut sink), Ok(read) if read > 0) {}
}

/// Reads from `stream` until the blank line that ends a request head.
///
/// Returns the bytes read, which can run past the blank line into a body.
/// Returns `None` when the client closes, or stops sending, before the head
/// ends.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() > MAX_HEAD_BYTES {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => head.extend_from_slice(&chunk[..read]),
        }
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn the_server_answers_a_route_and_records_the_request() {
        let server = Server::start(vec![Route::get("/x", "hello")]);
        let dir = temp_dir("testutil-route");
        let config = Config::for_test(&dir, &server.base());

        let body =
            crate::remote::fetch_text(&config, &format!("{}/x", server.base()), 1024).unwrap();

        assert_eq!(body, "hello");
        assert_eq!(
            server.requests(),
            vec![("GET".to_string(), "/x".to_string(), String::new())]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_envelope_verifies_once_its_key_is_trusted() {
        let signer = Signer::new();
        signer.trust();

        let envelope: serde_json::Value =
            serde_json::from_str(&signer.envelope(r#"{"a":1}"#)).unwrap();
        let payload = envelope["payload"].as_str().unwrap();
        let signature = envelope["signature"].as_str().unwrap();

        assert_eq!(payload, r#"{"a":1}"#);
        crate::signature::verify_content(payload.as_bytes(), signature).unwrap();
    }
}
