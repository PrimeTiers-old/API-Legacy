//! Minimal MongoDB client for Cloudflare Workers.
//!
//! The official `mongodb` crate depends on native sockets/TLS and cannot be
//! compiled to wasm. Workers expose outbound TCP through `cloudflare:sockets`
//! (wrapped by `worker::Socket`), so this module speaks just enough of the
//! MongoDB wire protocol, TLS, SRV discovery, and SCRAM-SHA-256 authentication
//! for the read-only commands this API needs.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bson::{doc, spec::BinarySubtype, Binary, Bson, Document};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use hmac::{Hmac, Mac};
use md5::Md5;
use serde::Deserialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicI32, Ordering};
use worker::{
    console_error, Env, Error, Fetch, Headers, Method, Request, RequestInit, Result, SecureTransport,
    Socket,
};

type HmacSha256 = Hmac<Sha256>;
type HmacSha1 = Hmac<Sha1>;

#[derive(Clone, Copy)]
enum ScramHash {
    Sha1,
    Sha256,
}

impl ScramHash {
    fn mechanism(self) -> &'static str {
        match self {
            ScramHash::Sha1 => "SCRAM-SHA-1",
            ScramHash::Sha256 => "SCRAM-SHA-256",
        }
    }

    fn key_len(self) -> usize {
        match self {
            ScramHash::Sha1 => 20,
            ScramHash::Sha256 => 32,
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            ScramHash::Sha1 => Sha1::digest(data).to_vec(),
            ScramHash::Sha256 => Sha256::digest(data).to_vec(),
        }
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        match self {
            ScramHash::Sha1 => {
                let mut mac = HmacSha1::new_from_slice(key).expect("HMAC key");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
            ScramHash::Sha256 => hmac(key, data),
        }
    }

    fn pbkdf2(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut out = vec![0u8; self.key_len()];
        match self {
            ScramHash::Sha1 => {
                pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, iterations, &mut out)
            }
            ScramHash::Sha256 => {
                pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut out)
            }
        }
        out
    }
}

static REQUEST_ID: AtomicI32 = AtomicI32::new(1);

fn next_request_id() -> i32 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn io_err(e: std::io::Error) -> Error {
    Error::RustError(format!("mongo socket: {e}"))
}

fn bson_i64(doc: &Document, key: &str) -> Option<i64> {
    match doc.get(key)? {
        Bson::Int32(n) => Some(*n as i64),
        Bson::Int64(n) => Some(*n),
        Bson::Double(n) => Some(*n as i64),
        _ => None,
    }
}

fn bson_ok(doc: &Document) -> bool {
    bson_i64(doc, "ok").unwrap_or(0) == 1
}

fn as_binary(doc: &Document, key: &str) -> Option<Vec<u8>> {
    match doc.get(key)? {
        Bson::Binary(b) => Some(b.bytes.clone()),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

struct MongoUri {
    username: String,
    password: String,
    host: String,
    port: u16,
    srv: bool,
    auth_source: String,
}

fn parse_mongo_uri(uri: &str) -> Result<MongoUri> {
    let (srv, rest) = if let Some(r) = uri.strip_prefix("mongodb+srv://") {
        (true, r)
    } else if let Some(r) = uri.strip_prefix("mongodb://") {
        (false, r)
    } else {
        return Err(Error::RustError(
            "MONGODB_URI must start with mongodb:// or mongodb+srv://".into(),
        ));
    };

    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };

    let (userinfo, hosts) = match authority.rfind('@') {
        Some(i) => (&authority[..i], &authority[i + 1..]),
        None => ("", authority),
    };

    let (username, password) = match userinfo.split_once(':') {
        Some((u, p)) => (percent_decode(u), percent_decode(p)),
        None => (percent_decode(userinfo), String::new()),
    };

    // For a normal mongodb:// URI there may be several comma-separated hosts.
    // Atlas uses SRV; for a direct URI we use the first host.
    let host_port = hosts.split(',').next().unwrap_or(hosts);
    let (host, port) = if srv {
        (host_port.to_string(), 27017)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        (h.to_string(), p.parse::<u16>().unwrap_or(27017))
    } else {
        (host_port.to_string(), 27017)
    };

    let mut auth_source = "admin".to_string();
    for pair in tail.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.eq_ignore_ascii_case("authSource") {
                auth_source = percent_decode(v);
            }
        }
    }

    Ok(MongoUri {
        username,
        password,
        host,
        port,
        srv,
        auth_source,
    })
}

#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer")]
    answer: Option<Vec<DohAnswer>>,
}

#[derive(Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    answer_type: u16,
    data: String,
}

async fn resolve_srv(host: &str) -> Result<(String, u16)> {
    let url = format!("https://cloudflare-dns.com/dns-query?name=_mongodb._tcp.{host}&type=SRV");
    let headers = Headers::new();
    headers.set("accept", "application/dns-json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = Request::new_with_init(&url, &init)?;
    let mut resp = Fetch::Request(request).send().await?;
    if resp.status_code() >= 400 {
        return Err(Error::RustError(format!(
            "SRV lookup failed with HTTP {}",
            resp.status_code()
        )));
    }
    let body: DohResponse = resp.json().await?;
    let record = body
        .answer
        .unwrap_or_default()
        .into_iter()
        .find(|a| a.answer_type == 33)
        .ok_or_else(|| Error::RustError(format!("no SRV record found for {host}")))?;

    // SRV data: "<priority> <weight> <port> <target>."
    let mut parts = record.data.split_whitespace();
    let _priority = parts.next();
    let _weight = parts.next();
    let port = parts
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(27017);
    let target = parts
        .next()
        .ok_or_else(|| Error::RustError("malformed SRV record".into()))?
        .trim_end_matches('.')
        .to_string();

    Ok((target, port))
}

fn wire_message(request_id: i32, command: &Document) -> Result<Vec<u8>> {
    let mut bson = Vec::new();
    command
        .to_writer(&mut bson)
        .map_err(|e| Error::RustError(format!("bson encode: {e}")))?;

    let length = 16 + 4 + 1 + bson.len();
    let mut msg = Vec::with_capacity(length);
    msg.extend_from_slice(&(length as i32).to_le_bytes());
    msg.extend_from_slice(&request_id.to_le_bytes());
    msg.extend_from_slice(&0i32.to_le_bytes()); // responseTo
    msg.extend_from_slice(&2013i32.to_le_bytes()); // OP_MSG
    msg.extend_from_slice(&0u32.to_le_bytes()); // flagBits
    msg.push(0); // section kind 0 (body)
    msg.extend_from_slice(&bson);
    Ok(msg)
}

async fn write_message(socket: &mut Socket, command: &Document) -> Result<()> {
    let request_id = next_request_id();
    let msg = wire_message(request_id, command)?;
    socket.write_all(&msg).await.map_err(io_err)?;
    socket.flush().await.map_err(io_err)?;
    Ok(())
}

async fn read_message(socket: &mut Socket) -> Result<Document> {
    let mut header = [0u8; 16];
    socket.read_exact(&mut header).await.map_err(io_err)?;

    let length = i32::from_le_bytes(header[0..4].try_into().unwrap());
    if length < 21 {
        return Err(Error::RustError(format!(
            "invalid mongo message length {length}"
        )));
    }
    let opcode = i32::from_le_bytes(header[12..16].try_into().unwrap());
    if opcode != 2013 {
        return Err(Error::RustError(format!("unsupported mongo opcode {opcode}")));
    }

    let mut body = vec![0u8; (length as usize) - 16];
    socket.read_exact(&mut body).await.map_err(io_err)?;

    if body.len() < 5 {
        return Err(Error::RustError("short OP_MSG body".into()));
    }
    // Skip flagBits (4 bytes). The first section must be kind 0 for replies
    // to the commands used here.
    let kind = body[4];
    if kind != 0 {
        return Err(Error::RustError(format!(
            "unsupported mongo section kind {kind}"
        )));
    }
    let doc = Document::from_reader(&body[5..])
        .map_err(|e| Error::RustError(format!("bson decode: {e}")))?;
    Ok(doc)
}

async fn command(socket: &mut Socket, command: Document) -> Result<Document> {
    write_message(socket, &command).await?;
    let response = read_message(socket).await?;
    if !bson_ok(&response) {
        let message = response
            .get_str("errmsg")
            .unwrap_or("unknown MongoDB error")
            .to_string();
        return Err(Error::RustError(format!("mongo command failed: {message}")));
    }
    Ok(response)
}

fn mongo_sha1_password(username: &str, password: &str) -> String {
    let digest = Md5::digest(format!("{username}:mongo:{password}").as_bytes());
    hex::encode(digest)
}

fn sasl_escape_username(name: &str) -> String {
    name.replace('=', "=3D").replace(',', "=2C")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn xor(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| x ^ y).collect()
}

async fn authenticate(socket: &mut Socket, uri: &MongoUri, hash: ScramHash) -> Result<()> {
    let mut nonce_bytes = [0u8; 24];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| Error::RustError(format!("random nonce: {e}")))?;
    let client_nonce = B64.encode(nonce_bytes).replace(',', "");

    let client_first_bare = format!(
        "n={},r={}",
        sasl_escape_username(&uri.username),
        client_nonce
    );
    let client_first = format!("n,,{client_first_bare}");

    let start = doc! {
        "saslStart": 1i32,
        "mechanism": hash.mechanism(),
        "payload": Binary { subtype: BinarySubtype::Generic, bytes: client_first.into_bytes() },
        "autoAuthorize": 1i32,
        "options": { "skipEmptyExchange": true },
        "$db": uri.auth_source.clone(),
    };
    let response = command(socket, start).await?;
    let conversation_id = bson_i64(&response, "conversationId").unwrap_or(1);
    let server_first_bytes = as_binary(&response, "payload")
        .ok_or_else(|| Error::RustError("SCRAM response had no payload".into()))?;
    let server_first = String::from_utf8(server_first_bytes)
        .map_err(|_| Error::RustError("SCRAM payload was not UTF-8".into()))?;

    let mut server_nonce = String::new();
    let mut salt = Vec::new();
    let mut iterations = 0u32;
    for part in server_first.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "r" => server_nonce = v.to_string(),
                "s" => {
                    salt = B64
                        .decode(v)
                        .map_err(|e| Error::RustError(format!("SCRAM salt: {e}")))?
                }
                "i" => iterations = v.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    if !server_nonce.starts_with(&client_nonce) || salt.is_empty() || iterations == 0 {
        return Err(Error::RustError(
            "invalid SCRAM server-first message".into(),
        ));
    }

    let password = match hash {
        ScramHash::Sha1 => mongo_sha1_password(&uri.username, &uri.password),
        ScramHash::Sha256 => uri.password.clone(),
    };
    let salted_password = hash.pbkdf2(password.as_bytes(), &salt, iterations);
    let client_key = hash.hmac(&salted_password, b"Client Key");
    let stored_key = hash.digest(&client_key);
    let client_final_without_proof = format!("c=biws,r={server_nonce}");
    let auth_message =
        format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = hash.hmac(&stored_key, auth_message.as_bytes());
    let client_proof = xor(&client_key, &client_signature);
    let server_key = hash.hmac(&salted_password, b"Server Key");
    let expected_server_signature = B64.encode(hash.hmac(&server_key, auth_message.as_bytes()));

    let client_final = format!(
        "{client_final_without_proof},p={}",
        B64.encode(client_proof)
    );
    let cont = doc! {
        "saslContinue": 1i32,
        "conversationId": conversation_id,
        "payload": Binary { subtype: BinarySubtype::Generic, bytes: client_final.into_bytes() },
        "$db": uri.auth_source.clone(),
    };
    let response = command(socket, cont).await?;
    let payload = as_binary(&response, "payload")
        .ok_or_else(|| Error::RustError("SCRAM final response had no payload".into()))?;
    let final_message = String::from_utf8(payload)
        .map_err(|_| Error::RustError("SCRAM final payload was not UTF-8".into()))?;

    let server_signature = final_message
        .split(',')
        .find_map(|p| p.strip_prefix("v="))
        .ok_or_else(|| Error::RustError("SCRAM server signature missing".into()))?;
    if server_signature != expected_server_signature {
        return Err(Error::RustError("SCRAM server signature mismatch".into()));
    }

    // Older servers may not set done=true on the first continue.
    if !matches!(response.get("done"), Some(Bson::Boolean(true))) {
        let finish = doc! {
            "saslContinue": 1i32,
            "conversationId": conversation_id,
            "payload": Binary { subtype: BinarySubtype::Generic, bytes: Vec::<u8>::new() },
            "$db": uri.auth_source.clone(),
        };
        let response = command(socket, finish).await?;
        if !matches!(response.get("done"), Some(Bson::Boolean(true))) {
            return Err(Error::RustError(
                "SCRAM conversation did not finish".into(),
            ));
        }
    }

    Ok(())
}

struct ServerInfo {
    mechanisms: Vec<String>,
    primary: Option<String>,
    writable: bool,
}

async fn server_info(socket: &mut Socket, uri: &MongoUri) -> Result<ServerInfo> {
    let hello = doc! {
        "hello": 1i32,
        "saslSupportedMechs": format!("{}.{}", uri.auth_source, uri.username),
        "$db": "admin",
    };
    let response = command(socket, hello).await?;
    let mut mechanisms = Vec::new();
    if let Some(Bson::Array(values)) = response.get("saslSupportedMechs") {
        for value in values {
            if let Bson::String(s) = value {
                mechanisms.push(s.clone());
            }
        }
    }
    Ok(ServerInfo {
        mechanisms,
        primary: response.get_str("primary").ok().map(|s| s.to_string()),
        writable: matches!(response.get("isWritablePrimary"), Some(Bson::Boolean(true))),
    })
}

fn split_host_port(value: &str, default_port: u16) -> (String, u16) {
    match value.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>().unwrap_or(default_port),
        ),
        None => (value.to_string(), default_port),
    }
}

/// A short-lived MongoDB connection for one API request.
pub struct Mongo {
    socket: Socket,
    db: String,
}

impl Mongo {
    pub async fn connect(env: &Env) -> Result<Self> {
        let uri_string = env
            .secret("MONGODB_URI")
            .map(|s| s.to_string())
            .or_else(|_| env.var("MONGODB_URI").map(|s| s.to_string()))
            .map_err(|_| Error::RustError("MONGODB_URI is not configured".into()))?;
        let db = env
            .var("MONGODB_DB")
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "primetiers".to_string());

        let uri = parse_mongo_uri(&uri_string)?;
        let (host, port) = if uri.srv {
            match resolve_srv(&uri.host).await {
                Ok(host_port) => host_port,
                Err(e) => {
                    console_error!("SRV lookup failed: {e}");
                    (uri.host.clone(), uri.port)
                }
            }
        } else {
            (uri.host.clone(), uri.port)
        };

        let mut socket = Socket::builder()
            .secure_transport(SecureTransport::On)
            .connect(host, port)?;
        socket.opened().await?;

        let mut info = server_info(&mut socket, &uri).await?;

        // SRV/host lists can point at a secondary. Follow the advertised
        // primary once before authenticating, otherwise reads fail with
        // "not primary and secondaryOk=false".
        if !info.writable {
            if let Some(primary) = info.primary.clone() {
                let (primary_host, primary_port) = split_host_port(&primary, 27017);
                if let Ok(mut next) = Socket::builder()
                    .secure_transport(SecureTransport::On)
                    .connect(primary_host, primary_port)
                {
                    if next.opened().await.is_ok() {
                        if let Ok(next_info) = server_info(&mut next, &uri).await {
                            socket = next;
                            info = next_info;
                        }
                    }
                }
            }
        }

        let mechanism = if info.mechanisms.iter().any(|m| m == "SCRAM-SHA-256") {
            ScramHash::Sha256
        } else if info.mechanisms.iter().any(|m| m == "SCRAM-SHA-1") {
            ScramHash::Sha1
        } else {
            ScramHash::Sha1
        };

        let mut mongo = Self { socket, db };
        authenticate(&mut mongo.socket, &uri, mechanism).await?;
        Ok(mongo)
    }

    pub async fn find(
        &mut self,
        collection: &str,
        filter: Document,
        sort: Option<Document>,
        limit: Option<i64>,
        projection: Option<Document>,
    ) -> Result<Vec<Document>> {
        let mut cmd = doc! {
            "find": collection,
            "filter": filter,
            "batchSize": 10_000i32,
            "$db": self.db.clone(),
        };
        if let Some(sort) = sort {
            cmd.insert("sort", sort);
        }
        if let Some(limit) = limit {
            cmd.insert("limit", limit);
        }
        if let Some(projection) = projection {
            cmd.insert("projection", projection);
        }

        let response = command(&mut self.socket, cmd).await?;
        let cursor = response
            .get_document("cursor")
            .map_err(|_| Error::RustError("find response had no cursor".into()))?;
        let mut out = docs_from_cursor(cursor, "firstBatch");
        let mut cursor_id = bson_i64(cursor, "id").unwrap_or(0);

        while cursor_id != 0 {
            let get_more = doc! {
                "getMore": cursor_id,
                "collection": collection,
                "batchSize": 10_000i32,
                "$db": self.db.clone(),
            };
            let response = command(&mut self.socket, get_more).await?;
            let cursor = response
                .get_document("cursor")
                .map_err(|_| Error::RustError("getMore response had no cursor".into()))?;
            out.extend(docs_from_cursor(cursor, "nextBatch"));
            cursor_id = bson_i64(cursor, "id").unwrap_or(0);
        }

        Ok(out)
    }

    /// Inserts one document. Needs a database user with write access to
    /// `collection`.
    pub async fn insert_one(&mut self, collection: &str, document: Document) -> Result<()> {
        let cmd = doc! {
            "insert": collection,
            "documents": [document],
            "$db": self.db.clone(),
        };
        let response = command(&mut self.socket, cmd).await?;
        // A failed write still returns ok:1, with the reason in writeErrors.
        if let Ok(errors) = response.get_array("writeErrors") {
            let message = errors
                .first()
                .and_then(|e| e.as_document())
                .and_then(|e| e.get_str("errmsg").ok())
                .unwrap_or("unknown write error");
            return Err(Error::RustError(format!("mongo insert failed: {message}")));
        }
        Ok(())
    }
}

fn docs_from_cursor(cursor: &Document, key: &str) -> Vec<Document> {
    match cursor.get(key) {
        Some(Bson::Array(rows)) => rows
            .iter()
            .filter_map(|b| b.as_document().cloned())
            .collect(),
        _ => Vec::new(),
    }
}
