//! Network diagnostics: the website's /network page pings a few hosting
//! providers from the visitor's browser and posts the results here.
//!
//! Reports are saved to the `network_reports` MongoDB collection. This is the
//! only thing the API writes, so the Atlas user needs `insert` and `update`
//! on that collection (reads elsewhere are unchanged).
//!
//! Routes:
//!   POST /api/network/report        public, one report per IP per minute.
//!                                   Returns a token for the follow-up below.
//!   POST /api/network/report/extra  adds the hand-run Karachi ping and/or
//!                                   tracert to that report (within an hour)
//!
//! Read the reports straight from MongoDB; there is no read endpoint.

use crate::mongo::Mongo;
use bson::{doc, Bson, DateTime, Document};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::*;

const COLLECTION: &str = "network_reports";
const COOLDOWN_MS: i64 = 60_000;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_TRACE_CHARS: usize = 8000;
/// How long the page may attach extra data to a report it just sent.
const EXTRA_WINDOW_MS: i64 = 60 * 60 * 1000;

/// Networks whose IPs belong to a VPN / privacy relay rather than the
/// player's ISP (Cloudflare WARP and iCloud Private Relay egress via
/// Cloudflare, Akamai, Fastly). Reports from these don't show a straight
/// route, so they're flagged with `relay: true`.
const RELAY_ASNS: &[u32] = &[13335, 36183, 54113];

/// Most results one report may hold. Target ids come from the page
/// (`Website/src/network.js`), e.g. `vultr-bom`, so adding a provider there
/// needs no API change.
const MAX_RESULTS: usize = 40;

/// Target ids: 1-32 chars of lowercase letters, digits and `-`.
fn valid_target(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Deserialize)]
struct ReportIn {
    #[serde(default)]
    name: Option<String>,
    results: Vec<ResultIn>,
    #[serde(default)]
    trace: Option<String>,
}

#[derive(Deserialize)]
struct ExtraIn {
    token: String,
    #[serde(default)]
    khi_ms: Option<f64>,
    #[serde(default)]
    trace: Option<String>,
}

#[derive(Deserialize)]
struct ResultIn {
    id: String,
    /// Median round trip in ms; null when the test failed.
    ms: Option<f64>,
    #[serde(default)]
    min: Option<f64>,
    /// true when the visitor typed the number in (ICMP-only targets).
    #[serde(default)]
    manual: bool,
}

fn reply(status: u16, body: &Value, origin: Option<&str>) -> Result<Response> {
    let mut resp = Response::from_json(body)?.with_status(status);
    let h = resp.headers_mut();
    h.set("content-type", "application/json; charset=utf-8")?;
    h.set("cache-control", "no-store")?;
    h.set("x-content-type-options", "nosniff")?;
    if let Some(o) = origin {
        h.set("access-control-allow-origin", o)?;
    }
    h.set("vary", "Origin")?;
    Ok(resp)
}

fn err(status: u16, msg: &str, origin: Option<&str>) -> Result<Response> {
    reply(status, &json!({ "error": msg }), origin)
}

fn db_err(e: Error, origin: Option<&str>) -> Result<Response> {
    console_error!("network mongo error: {e}");
    err(502, "upstream unavailable", origin)
}

fn clean_ms(v: Option<f64>) -> Bson {
    v.filter(|n| n.is_finite() && *n >= 0.0 && *n <= 10_000.0)
        .map(|n| Bson::Double((n * 10.0).round() / 10.0))
        .unwrap_or(Bson::Null)
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut b);
    hex::encode(b)
}

fn clean_trace(t: Option<String>) -> Option<String> {
    t.map(|t| t.trim().chars().take(MAX_TRACE_CHARS).collect::<String>())
        .filter(|t| !t.is_empty())
}

/// Current time from the Workers runtime. Don't use bson's `DateTime::now()`:
/// it calls `SystemTime::now()`, which panics on wasm32-unknown-unknown.
fn now_utc() -> DateTime {
    DateTime::from_millis(Date::now().as_millis() as i64)
}

fn opt_str(v: Option<String>) -> Bson {
    v.map(Bson::String).unwrap_or(Bson::Null)
}

/// Handles everything under /api/network. Returns None for other paths.
pub async fn handle(req: Request, env: &Env, origin: Option<&str>) -> Option<Result<Response>> {
    let path = req.path();
    match (req.method(), path.as_str()) {
        (Method::Post, "/api/network/report") => Some(post_report(req, env, origin).await),
        (Method::Post, "/api/network/report/extra") => Some(post_extra(req, env, origin).await),
        _ if path.starts_with("/api/network/") => Some(err(405, "method not allowed", origin)),
        _ => None,
    }
}

async fn post_report(mut req: Request, env: &Env, origin: Option<&str>) -> Result<Response> {
    // Browsers send Origin on cross-site POSTs; only accept our own site.
    if origin.is_none() {
        return err(403, "origin not allowed", None);
    }

    let body = req.text().await.unwrap_or_default();
    if body.len() > MAX_BODY_BYTES {
        return err(413, "report too large", origin);
    }
    let input: ReportIn = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return err(400, "invalid report", origin),
    };

    let mut seen: Vec<String> = Vec::new();
    let results: Vec<Bson> = input
        .results
        .into_iter()
        .filter_map(|r| {
            if !valid_target(&r.id) || seen.contains(&r.id) || seen.len() >= MAX_RESULTS {
                return None;
            }
            seen.push(r.id.clone());
            Some(Bson::Document(doc! {
                "target": r.id,
                "ms": clean_ms(r.ms),
                "min": clean_ms(r.min),
                "manual": r.manual,
            }))
        })
        .collect();
    if results.is_empty() {
        return err(400, "no results", origin);
    }

    let name = input
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| crate::valid_username(n));
    let trace = clean_trace(input.trace);

    let ip = req.headers().get("cf-connecting-ip").ok().flatten();
    let cf = req.cf();
    let asn = cf.and_then(|c| c.asn());
    let relay = asn.is_some_and(|a| RELAY_ASNS.contains(&a));
    let token = random_token();
    let now = now_utc();

    let mut db = match Mongo::connect(env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, origin),
    };

    // One report per IP per minute.
    if let Some(ip) = &ip {
        let since = DateTime::from_millis(now.timestamp_millis() - COOLDOWN_MS);
        let recent = db
            .find(
                COLLECTION,
                doc! { "ip": ip, "created_at": { "$gt": since } },
                None,
                Some(1),
                Some(doc! { "_id": 1 }),
            )
            .await;
        match recent {
            Ok(rows) if !rows.is_empty() => {
                return err(429, "you already sent a report, try again in a minute", origin)
            }
            Ok(_) => {}
            Err(e) => return db_err(e, origin),
        }
    }

    let report = doc! {
        "created_at": now,
        "username": opt_str(name),
        "ip": opt_str(ip),
        "asn": asn.map(|a| Bson::Int64(a as i64)).unwrap_or(Bson::Null),
        "relay": relay,
        "token": &token,
        "as_org": opt_str(cf.and_then(|c| c.as_organization())),
        "country": opt_str(cf.and_then(|c| c.country())),
        "city": opt_str(cf.and_then(|c| c.city())),
        "colo": opt_str(cf.map(|c| c.colo())),
        "results": results,
        "traceroute": opt_str(trace),
    };

    if let Err(e) = db.insert_one(COLLECTION, report).await {
        return db_err(e, origin);
    }
    reply(201, &json!({ "ok": true, "token": token, "relay": relay }), origin)
}

async fn post_extra(mut req: Request, env: &Env, origin: Option<&str>) -> Result<Response> {
    if origin.is_none() {
        return err(403, "origin not allowed", None);
    }
    let body = req.text().await.unwrap_or_default();
    if body.len() > MAX_BODY_BYTES {
        return err(413, "report too large", origin);
    }
    let input: ExtraIn = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return err(400, "invalid report", origin),
    };
    if input.token.len() != 32 || !input.token.chars().all(|c| c.is_ascii_hexdigit()) {
        return err(400, "invalid token", origin);
    }

    let mut update = Document::new();
    let mut set = Document::new();
    if let Some(trace) = clean_trace(input.trace) {
        set.insert("traceroute", trace);
    }
    if let Bson::Double(ms) = clean_ms(input.khi_ms) {
        update.insert(
            "$push",
            doc! { "results": { "target": "lightnode-khi", "ms": ms, "min": Bson::Null, "manual": true } },
        );
    }
    if set.is_empty() && update.is_empty() {
        return err(400, "nothing to add", origin);
    }
    if !set.is_empty() {
        update.insert("$set", set);
    }

    let now = now_utc().timestamp_millis();
    let filter = doc! {
        "token": &input.token,
        "created_at": { "$gt": DateTime::from_millis(now - EXTRA_WINDOW_MS) },
        // Only one hand-entered Karachi result per report.
        "results.target": { "$ne": "lightnode-khi" },
    };
    let mut db = match Mongo::connect(env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, origin),
    };
    match db.update_one(COLLECTION, filter, update).await {
        Ok(0) => err(404, "report not found or already updated", origin),
        Ok(_) => reply(200, &json!({ "ok": true }), origin),
        Err(e) => db_err(e, origin),
    }
}
