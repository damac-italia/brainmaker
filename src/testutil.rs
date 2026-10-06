// SPDX-License-Identifier: GPL-3.0-or-later

//! Helpers for the tests that run the client against a local server.
//!
//! This module is built for tests only. It holds a signer whose key the client
//! under test trusts, a small HTTP server on the loopback address, and a builder
//! for zip archives. No test that uses it needs the network, a fixed port, or a
//! fixed path.
//!
//! The server records each request with its headers and its body. A route can
//! send response headers, give a different reply to each later request, and,
//! for the token route, answer only the scope that the form asks for.

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

/// The method, the path, and the value of the `Authorization` header, or an
/// empty string when it has none. [`Server::requests`] returns this short form.
type Recorded = Vec<(String, String, String)>;

/// The longest request head that the server reads.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// The longest request body that the server reads.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

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

/// The SHA-256 of `bytes`, as 64 lower-case hexadecimal characters.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
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
    /// The key replaces the software list and the content list for that
    /// thread. It signs no removal order.
    pub fn trust(&self) {
        crate::signature::trust_in_this_test(&[self.public_hex()]);
    }

    /// Makes the client trust this signer's key for a removal order, in the
    /// calling thread only.
    pub fn trust_for_removal(&self) {
        crate::signature::trust_for_removal_in_this_test(&[self.public_hex()]);
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

/// One request as the server received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub method: String,
    pub path: String,
    /// Every header, in the order it arrived, with its name in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    /// The value of the header `name`, which the caller writes in lower case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The value of `key` in a form body, decoded.
    pub fn form(&self, key: &str) -> Option<String> {
        form_value(&self.body, key)
    }
}

/// One reply: the status, the body, and the headers besides
/// `Content-Length` and `Connection`.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

/// One answer that a [`Server`] gives: the request it matches, and the reply.
pub struct Route {
    pub method: &'static str,
    pub path: String,
    pub status: u16,
    pub body: Vec<u8>,
    /// Headers of the reply, besides `Content-Length` and `Connection`.
    pub headers: Vec<(String, String)>,
    /// The replies to the second request and to the ones after it, in order.
    /// The last one repeats. Empty means the first reply repeats.
    pub then: Vec<Reply>,
    /// When set, the route matches only a form body that asks for this
    /// scope, as a token request does.
    pub scope: Option<String>,
}

impl Route {
    /// A `GET` route that answers 200 with `body`.
    pub fn get(path: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "GET",
            path: path.to_string(),
            status: 200,
            body: body.into(),
            headers: Vec::new(),
            then: Vec::new(),
            scope: None,
        }
    }

    /// A `POST` route that answers 200 with `body`.
    pub fn post(path: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "POST",
            ..Self::get(path, body)
        }
    }

    /// A `PUT` route that answers 200 with `body`.
    pub fn put(path: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "PUT",
            ..Self::get(path, body)
        }
    }

    /// The token route `POST /oauth2/token`, for a request that asks for
    /// `scope`. It answers 200 with `body`.
    pub fn token(scope: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            scope: Some(scope.to_string()),
            ..Self::post("/oauth2/token", body)
        }
    }

    /// The same route with the status `status`.
    pub fn status(self, status: u16) -> Self {
        Self { status, ..self }
    }

    /// The same route, with one more header in its first reply.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// The same route, with one more reply for a later request.
    pub fn then(mut self, status: u16, body: impl Into<Vec<u8>>) -> Self {
        self.then.push(Reply {
            status,
            body: body.into(),
            headers: Vec::new(),
        });
        self
    }

    /// True when the route answers `request`.
    fn matches(&self, request: &Received) -> bool {
        self.method == request.method
            && self.path == request.path
            && self
                .scope
                .as_deref()
                .is_none_or(|scope| request.form("scope").as_deref() == Some(scope))
    }

    /// The reply to the request that is number `served` for this route,
    /// counting from 0.
    fn reply(&self, served: usize) -> Reply {
        match served.checked_sub(1) {
            Some(index) if !self.then.is_empty() => {
                self.then[index.min(self.then.len() - 1)].clone()
            }
            _ => Reply {
                status: self.status,
                body: self.body.clone(),
                headers: self.headers.clone(),
            },
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
    requests: Arc<Mutex<Vec<Received>>>,
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
        let requests = Arc::new(Mutex::new(Vec::new()));
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

    /// Every request that the server has answered so far, oldest first, as
    /// the method, the path, and the `Authorization` header.
    pub fn requests(&self) -> Recorded {
        self.received()
            .into_iter()
            .map(|request| {
                let authorization = request.header("authorization").unwrap_or("").to_string();
                (request.method, request.path, authorization)
            })
            .collect()
    }

    /// Every request that the server has answered so far, oldest first, whole.
    pub fn received(&self) -> Vec<Received> {
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
fn serve(
    listener: TcpListener,
    routes: &[Route],
    requests: &Mutex<Vec<Received>>,
    stop: &AtomicBool,
) {
    // How many requests each route has answered, for its list of replies.
    let mut served = vec![0usize; routes.len()];
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Ok((stream, _)) = accepted {
            answer(stream, routes, &mut served, requests);
        }
    }
}

/// Reads one request from `stream`, records it, and writes the reply.
fn answer(
    mut stream: TcpStream,
    routes: &[Route],
    served: &mut [usize],
    requests: &Mutex<Vec<Received>>,
) {
    // A client that connects and says nothing must not hold the server.
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));

    let Some(request) = read_request(&mut stream) else {
        return;
    };
    requests.lock().unwrap().push(request.clone());

    let reply = match routes.iter().position(|route| route.matches(&request)) {
        Some(index) => {
            let reply = routes[index].reply(served[index]);
            served[index] += 1;
            reply
        }
        None => Reply {
            status: 404,
            body: br#"{"error": "no such route"}"#.to_vec(),
            headers: Vec::new(),
        },
    };
    let status = reply.status;
    let reason = if status == 200 { "OK" } else { "Error" };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    let mut reply_bytes = head.into_bytes();
    reply_bytes.extend_from_slice(&reply.body);
    let reply = reply_bytes;

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

/// Reads one request from `stream`: the head, then as many body bytes as
/// `Content-Length` names.
///
/// Returns `None` when the client closes, or stops sending, before the head
/// ends.
fn read_request(stream: &mut TcpStream) -> Option<Received> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if bytes.len() > MAX_HEAD_BYTES {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
        }
    };

    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let path = request_line.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();

    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_BODY_BYTES);
    let mut body = bytes[end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    body.truncate(length);

    Some(Received {
        method,
        path,
        headers,
        body,
    })
}

/// The value of `key` in a form body, with `+` and `%XX` decoded.
fn form_value(body: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| decode(name) == key)
        .map(|(_, value)| decode(value))
}

/// Decodes one form field.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    fn the_server_records_headers_and_the_body_and_sends_headers() {
        let server = Server::start(vec![
            Route::post("/notes/a", r#"{"ok":true}"#)
                .status(201)
                .header("Retry-After", "7"),
        ]);

        let agent = crate::remote::build_agent(crate::remote::TEXT_TIMEOUT);
        let response = agent
            .post(&format!("{}/notes/a", server.base()))
            .header("X-Test", "one")
            .send(&b"the body"[..])
            .unwrap();

        assert_eq!(response.status().as_u16(), 201);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("7")
        );
        let received = server.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].header("x-test"), Some("one"));
        assert_eq!(received[0].body, b"the body");
    }

    #[test]
    fn a_route_gives_its_replies_in_order_and_repeats_the_last() {
        let server = Server::start(vec![
            Route::get("/x", "first")
                .then(429, "second")
                .then(500, "third"),
        ]);
        let dir = temp_dir("testutil-sequence");
        let config = Config::for_test(&dir, &server.base());
        let url = format!("{}/x", server.base());

        assert_eq!(
            crate::remote::fetch_text(&config, &url, 1024).unwrap(),
            "first"
        );
        for expected in ["HTTP 429", "HTTP 500", "HTTP 500"] {
            let error = crate::remote::fetch_text(&config, &url, 1024).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "got {error:#}");
        }

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_token_route_answers_only_the_scope_it_names() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::token("outbox:write", r#"{"error":"invalid_scope"}"#).status(400),
        ]);
        let agent = crate::remote::build_agent(crate::remote::TEXT_TIMEOUT);
        let url = format!("{}/oauth2/token", server.base());
        let ask = |scope: &str| {
            agent
                .post(&url)
                .send_form([("grant_type", "client_credentials"), ("scope", scope)])
                .unwrap()
                .status()
                .as_u16()
        };

        assert_eq!(ask("sync"), 200);
        assert_eq!(ask("outbox:write"), 400);
        assert_eq!(ask("publish"), 404);
        assert_eq!(
            server.received()[1].form("scope").as_deref(),
            Some("outbox:write")
        );
    }

    #[test]
    fn a_form_field_decodes_its_escapes() {
        assert_eq!(
            form_value(b"a=1&scope=outbox%3Awrite+now", "scope").as_deref(),
            Some("outbox:write now")
        );
        assert_eq!(form_value(b"a=1", "scope"), None);
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
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
