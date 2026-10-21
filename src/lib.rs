//! PrimeTiers public read API, running on Cloudflare Workers (Rust / Wasm).
//!
//! Reads from MongoDB Atlas through the small wire-protocol client in
//! `mongo.rs`. The API is read-only; all writes stay in the Discord bot.
//!
//! Routes:
//!   GET /api/health
//!   GET /api/gamemodes
//!   GET /api/players/:username
//!   GET /api/gamemodes/:gamemode/tiers?limit=50
//!   GET /api/recent?limit=20

mod mongo;

use bson::{doc, Bson, Document};
use mongo::Mongo;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use worker::*;

const GAMEMODES: &[&str] = &[
    "crystal",
    "axe",
    "sword",
    "mace",
    "macespear",
    "nethop",
    "pot",
    "smp",
    "uhc",
    "cart",
];

/// Browser and edge cache window for successful GET responses (5 minutes).
const CACHE_TTL_SECONDS: u32 = 300;
const CACHE_CONTROL: &str = "public, max-age=300, s-maxage=300";

#[derive(Clone)]
struct PlayerTier {
    gamemode: String,
    tier: String,
    updated_at: String,
}

struct PlayerRow {
    id: i64,
    username: String,
    region: Option<String>,
    platform: Option<String>,
    player_tiers: Vec<PlayerTier>,
}

#[derive(Serialize)]
struct ApiError<'a> {
    error: &'a str,
}

fn doc_i64(d: &Document, key: &str) -> Option<i64> {
    match d.get(key)? {
        Bson::Int32(n) => Some(*n as i64),
        Bson::Int64(n) => Some(*n),
        Bson::Double(n) => Some(*n as i64),
        _ => None,
    }
}

fn doc_string(d: &Document, key: &str) -> Option<String> {
    match d.get(key)? {
        Bson::String(s) => Some(s.clone()),
        // The Supabase -> MongoDB migration left some timestamp columns as
        // strings and some as BSON dates. Accept both.
        Bson::DateTime(dt) => dt.try_to_rfc3339_string().ok(),
        Bson::Int32(n) => Some(n.to_string()),
        Bson::Int64(n) => Some(n.to_string()),
        _ => None,
    }
}

fn tier_from_doc(d: &Document) -> Option<PlayerTier> {
    Some(PlayerTier {
        gamemode: doc_string(d, "gamemode")?,
        tier: doc_string(d, "tier")?,
        updated_at: doc_string(d, "updated_at")?,
    })
}

fn player_from_doc(d: &Document) -> Option<PlayerRow> {
    Some(PlayerRow {
        id: doc_i64(d, "_id")?,
        username: doc_string(d, "username")?,
        region: doc_string(d, "region"),
        platform: doc_string(d, "platform"),
        player_tiers: Vec::new(),
    })
}

fn with_common_headers(
    mut resp: Response,
    cache: bool,
    origin_ok: Option<&str>,
) -> Result<Response> {
    let headers = resp.headers_mut();
    headers.set("content-type", "application/json; charset=utf-8")?;
    headers.set("x-content-type-options", "nosniff")?;
    if cache {
        headers.set("cache-control", CACHE_CONTROL)?;
    } else {
        headers.set("cache-control", "no-store")?;
    }
    if let Some(origin) = origin_ok {
        headers.set("access-control-allow-origin", origin)?;
        headers.set("vary", "Origin")?;
    }
    Ok(resp)
}

fn json_ok<T: Serialize>(body: &T, origin: Option<&str>) -> Result<Response> {
    let resp = Response::from_json(body)?;
    with_common_headers(resp, true, origin)
}

fn json_err(status: u16, msg: &str, origin: Option<&str>) -> Result<Response> {
    let resp = Response::from_json(&ApiError { error: msg })?.with_status(status);
    with_common_headers(resp, false, origin)
}

/// Returns the request's Origin if it's on the allow list.
fn allowed_origin(req: &Request, env: &Env) -> Option<String> {
    let origin = req.headers().get("origin").ok().flatten()?;
    let list = env
        .var("ALLOWED_ORIGINS")
        .map(|v| v.to_string())
        .unwrap_or_default();
    list.split(',')
        .map(|s| s.trim())
        .any(|s| !s.is_empty() && s == origin)
        .then_some(origin)
}

/// Usernames: 1–32 chars of letters, digits, underscore, dot.
fn valid_username(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn clamp_limit(req: &Request, default: u32, max: u32) -> u32 {
    req.url()
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "limit")
                .and_then(|(_, v)| v.parse::<u32>().ok())
        })
        .unwrap_or(default)
        .clamp(1, max)
}

/// Maps MongoDB failures to 502 and logs the detail (never the URI/password).
fn db_err(e: Error, origin: Option<&str>) -> Result<Response> {
    console_error!("mongo error: {e}");
    json_err(502, "upstream unavailable", origin)
}

fn ids_filter(field: &str, ids: &[i64]) -> Document {
    let values: Vec<Bson> = ids.iter().copied().map(Bson::Int64).collect();
    doc! { field: { "$in": Bson::Array(values) } }
}

async fn fetch_players_by_ids(db: &mut Mongo, ids: &[i64]) -> Result<Vec<PlayerRow>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(1000) {
        let rows = db
            .find(
                "players",
                ids_filter("_id", chunk),
                None,
                None,
                None,
            )
            .await?;
        out.extend(rows.iter().filter_map(player_from_doc));
    }
    Ok(out)
}

async fn fetch_tiers_by_player_ids(db: &mut Mongo, ids: &[i64]) -> Result<Vec<(i64, PlayerTier)>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(1000) {
        let rows = db
            .find(
                "player_tiers",
                ids_filter("player_id", chunk),
                None,
                None,
                None,
            )
            .await?;
        for row in rows {
            if let (Some(player_id), Some(tier)) = (doc_i64(&row, "player_id"), tier_from_doc(&row)) {
                out.push((player_id, tier));
            }
        }
    }
    Ok(out)
}

async fn get_player(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();
    let name = match ctx.param("username") {
        Some(n) => n.to_string(),
        None => return json_err(400, "missing username", o),
    };
    if !valid_username(&name) {
        return json_err(400, "invalid username", o);
    }

    let mut db = match Mongo::connect(&ctx.env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, o),
    };

    let filter = doc! { "username_lower": name.to_lowercase() };
    let rows = match db.find("players", filter, None, Some(1), None).await {
        Ok(r) => r,
        Err(e) => return db_err(e, o),
    };
    let Some(mut player) = rows.first().and_then(player_from_doc) else {
        return json_err(404, "player not found", o);
    };

    let tiers = match fetch_tiers_by_player_ids(&mut db, &[player.id]).await {
        Ok(t) => t,
        Err(e) => return db_err(e, o),
    };
    player.player_tiers = tiers.into_iter().map(|(_, t)| t).collect();

    json_ok(&player_json(&player), o)
}

struct LbPlayer {
    username: String,
    region: Option<String>,
}

/// HT1 is the highest tier. Lower rank number = better.
fn tier_rank(tier: &str) -> Option<u8> {
    match tier.to_ascii_uppercase().as_str() {
        "HT1" => Some(1),
        "LT1" => Some(2),
        "HT2" => Some(3),
        "LT2" => Some(4),
        "HT3" => Some(5),
        "LT3" => Some(6),
        "HT4" => Some(7),
        "LT4" => Some(8),
        "HT5" => Some(9),
        "LT5" => Some(10),
        _ => None,
    }
}

/// Active gamemode filter for a MongoDB query.
fn active_gamemodes_filter() -> Document {
    let values: Vec<Bson> = GAMEMODES
        .iter()
        .map(|g| Bson::String((*g).to_string()))
        .collect();
    doc! { "$in": Bson::Array(values) }
}

/// GET /api/leaderboard[?gamemode=macespear]
async fn get_leaderboard(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();

    let filter = req.url().ok().and_then(|u| {
        u.query_pairs()
            .find(|(k, _)| k == "gamemode")
            .map(|(_, v)| db_slug(&v))
    });
    if let Some(gm) = &filter {
        if !GAMEMODES.contains(&gm.as_str()) {
            return json_err(404, "unknown gamemode", o);
        }
    }

    let mut db = match Mongo::connect(&ctx.env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, o),
    };

    let gm_filter = match &filter {
        Some(gm) => doc! { "gamemode": gm.clone() },
        None => doc! { "gamemode": active_gamemodes_filter() },
    };
    let rows = match db.find("player_tiers", gm_filter, None, None, None).await {
        Ok(r) => r,
        Err(e) => return db_err(e, o),
    };

    let mut player_ids: Vec<i64> = rows.iter().filter_map(|r| doc_i64(r, "player_id")).collect();
    player_ids.sort_unstable();
    player_ids.dedup();
    let players = match fetch_players_by_ids(&mut db, &player_ids).await {
        Ok(p) => p,
        Err(e) => return db_err(e, o),
    };
    let by_id: HashMap<i64, LbPlayer> = players
        .into_iter()
        .map(|p| {
            (
                p.id,
                LbPlayer {
                    username: p.username,
                    region: p.region,
                },
            )
        })
        .collect();

    // gamemode -> (rank, username, region, tier)
    let mut boards: Vec<(&str, Vec<(u8, String, Option<String>, String)>)> =
        GAMEMODES.iter().map(|gm| (*gm, Vec::new())).collect();

    for row in rows {
        let Some(gamemode) = doc_string(&row, "gamemode") else {
            continue;
        };
        let Some(board) = boards.iter_mut().find(|(gm, _)| *gm == gamemode) else {
            continue;
        };
        let Some(player_id) = doc_i64(&row, "player_id") else {
            continue;
        };
        let Some(p) = by_id.get(&player_id) else {
            continue;
        };
        let Some(tier) = doc_string(&row, "tier") else {
            continue;
        };
        let Some(rank) = tier_rank(&tier) else {
            continue;
        };
        board
            .1
            .push((rank, p.username.clone(), p.region.clone(), tier.to_ascii_uppercase()));
    }

    let mut out = serde_json::Map::new();
    for (gm, mut entries) in boards {
        if filter.as_deref().is_some_and(|f| f != gm) {
            continue;
        }
        entries.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        let list: Vec<Value> = entries
            .into_iter()
            .enumerate()
            .map(|(i, (_, username, region, tier))| {
                json!({ "rank": i + 1, "username": username, "region": region, "tier": tier })
            })
            .collect();
        out.insert(site_slug(gm).to_string(), Value::Array(list));
    }

    json_ok(&json!({ "gamemodes": Value::Object(out) }), o)
}

/// Website slug for a database gamemode id. The site uses these names.
fn site_slug(db: &str) -> &str {
    match db {
        "nethop" => "nethertite",
        "macespear" => "mace-spear",
        other => other,
    }
}

/// Database gamemode id for a name sent by the site or the database. Accepts both.
fn db_slug(input: &str) -> String {
    match input.to_ascii_lowercase().as_str() {
        "nethertite" => "nethop".to_string(),
        "mace-spear" => "macespear".to_string(),
        other => other.to_string(),
    }
}

/// Retired gamemodes (e.g. vanilla) are hidden from every response.
fn is_active_gamemode(gm: &str) -> bool {
    GAMEMODES.contains(&gm)
}

/// Points per tier for the global leaderboard. HT1 is the most, LT5 the least.
/// PLACEHOLDER values (standard MCTiers scheme). Edit to match the real scoring.
const POINTS: &[(&str, u32)] = &[
    ("HT1", 60),
    ("LT1", 45),
    ("HT2", 30),
    ("LT2", 20),
    ("HT3", 10),
    ("LT3", 6),
    ("HT4", 4),
    ("LT4", 3),
    ("HT5", 2),
    ("LT5", 1),
];

fn tier_points(tier: &str) -> Option<u32> {
    let t = tier.to_ascii_uppercase();
    POINTS.iter().find(|(k, _)| *k == t).map(|(_, p)| *p)
}

struct GlobalAgg {
    username: String,
    region: Option<String>,
    points: u32,
    tiers: Vec<(String, String)>,
}

/// GET /api/leaderboard/global[?limit=100]
async fn get_global_leaderboard(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();
    let limit = clamp_limit(&req, 100, 500) as usize;

    let mut db = match Mongo::connect(&ctx.env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, o),
    };

    let filter = doc! { "gamemode": active_gamemodes_filter() };
    let rows = match db.find("player_tiers", filter, None, None, None).await {
        Ok(r) => r,
        Err(e) => return db_err(e, o),
    };

    let mut player_ids: Vec<i64> = rows.iter().filter_map(|r| doc_i64(r, "player_id")).collect();
    player_ids.sort_unstable();
    player_ids.dedup();
    let players = match fetch_players_by_ids(&mut db, &player_ids).await {
        Ok(p) => p,
        Err(e) => return db_err(e, o),
    };
    let by_id: HashMap<i64, LbPlayer> = players
        .into_iter()
        .map(|p| {
            (
                p.id,
                LbPlayer {
                    username: p.username,
                    region: p.region,
                },
            )
        })
        .collect();

    let mut totals: HashMap<i64, GlobalAgg> = HashMap::new();
    for row in rows {
        let Some(gamemode) = doc_string(&row, "gamemode") else {
            continue;
        };
        if !is_active_gamemode(&gamemode) {
            continue;
        }
        let Some(player_id) = doc_i64(&row, "player_id") else {
            continue;
        };
        let Some(p) = by_id.get(&player_id) else {
            continue;
        };
        let Some(tier) = doc_string(&row, "tier") else {
            continue;
        };
        let Some(pts) = tier_points(&tier) else {
            continue;
        };
        let entry = totals.entry(player_id).or_insert_with(|| GlobalAgg {
            username: p.username.clone(),
            region: p.region.clone(),
            points: 0,
            tiers: Vec::new(),
        });
        entry.points += pts;
        entry.tiers.push((gamemode, tier.to_ascii_uppercase()));
    }

    let mut list: Vec<GlobalAgg> = totals.into_values().collect();
    list.sort_by(|a, b| {
        b.points
            .cmp(&a.points)
            .then_with(|| a.username.to_lowercase().cmp(&b.username.to_lowercase()))
    });

    let items: Vec<Value> = list
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, e)| {
            let tiers: Vec<Value> = e
                .tiers
                .iter()
                .map(|(gm, t)| json!({ "gamemode": site_slug(gm), "tier": t }))
                .collect();
            json!({
                "rank": i + 1,
                "username": e.username,
                "region": e.region,
                "points": e.points,
                "tiers": tiers,
            })
        })
        .collect();

    json_ok(&json!({ "count": items.len(), "items": items }), o)
}

const BATCH_MAX: usize = 50;

/// GET /api/players?names=a,b,c
async fn get_players_batch(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();

    let raw = req
        .url()
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "names")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_default();

    // Lowercase, dedupe, keep the original casing for the response order.
    let mut names: Vec<String> = Vec::new();
    for part in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !valid_username(part) {
            return json_err(400, "invalid username in names", o);
        }
        let lower = part.to_lowercase();
        if !names.contains(&lower) {
            names.push(lower);
        }
    }
    if names.is_empty() {
        return json_err(400, "names is required (comma-separated)", o);
    }
    if names.len() > BATCH_MAX {
        return json_err(400, "too many names (max 50)", o);
    }

    let mut db = match Mongo::connect(&ctx.env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, o),
    };

    let values: Vec<Bson> = names.iter().map(|n| Bson::String(n.clone())).collect();
    let filter = doc! { "username_lower": { "$in": Bson::Array(values) } };
    let rows = match db.find("players", filter, None, None, None).await {
        Ok(r) => r,
        Err(e) => return db_err(e, o),
    };

    let mut players: Vec<PlayerRow> = rows.iter().filter_map(player_from_doc).collect();
    let ids: Vec<i64> = players.iter().map(|p| p.id).collect();
    let tiers = match fetch_tiers_by_player_ids(&mut db, &ids).await {
        Ok(t) => t,
        Err(e) => return db_err(e, o),
    };
    for (player_id, tier) in tiers {
        if let Some(player) = players.iter_mut().find(|p| p.id == player_id) {
            player.player_tiers.push(tier);
        }
    }

    let found: Vec<String> = players.iter().map(|p| p.username.to_lowercase()).collect();
    let missing: Vec<&String> = names.iter().filter(|n| !found.contains(n)).collect();
    let items: Vec<Value> = players.iter().map(player_json).collect();

    json_ok(
        &json!({ "count": items.len(), "items": items, "missing": missing }),
        o,
    )
}

fn player_json(p: &PlayerRow) -> Value {
    let tiers: Vec<Value> = p
        .player_tiers
        .iter()
        .filter(|t| is_active_gamemode(&t.gamemode))
        .map(|t| {
            json!({
                "gamemode": site_slug(&t.gamemode),
                "tier": t.tier,
                "updatedAt": t.updated_at
            })
        })
        .collect();
    json!({
        "id": p.id,
        "username": p.username,
        "region": p.region,
        "platform": p.platform,
        "tiers": tiers,
    })
}

async fn get_gamemode_tiers(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();
    let gamemode = ctx
        .param("gamemode")
        .map(|s| db_slug(&s))
        .unwrap_or_default();
    if !GAMEMODES.contains(&gamemode.as_str()) {
        return json_err(404, "unknown gamemode", o);
    }
    let limit = clamp_limit(&req, 50, 100);

    let mut db = match Mongo::connect(&ctx.env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, o),
    };

    let filter = doc! { "gamemode": gamemode.clone() };
    let sort = doc! { "updated_at": -1i32 };
    let rows = match db
        .find("player_tiers", filter, Some(sort), Some(limit as i64), None)
        .await
    {
        Ok(r) => r,
        Err(e) => return db_err(e, o),
    };

    let ids: Vec<i64> = rows.iter().filter_map(|r| doc_i64(r, "player_id")).collect();
    let players = match fetch_players_by_ids(&mut db, &ids).await {
        Ok(p) => p,
        Err(e) => return db_err(e, o),
    };
    let by_id: HashMap<i64, String> = players.into_iter().map(|p| (p.id, p.username)).collect();

    let items: Vec<Value> = rows
        .into_iter()
        .filter_map(|r| {
            let player_id = doc_i64(&r, "player_id")?;
            let username = by_id.get(&player_id)?;
            Some(json!({
                "username": username,
                "tier": doc_string(&r, "tier")?,
                "updatedAt": doc_string(&r, "updated_at")?,
            }))
        })
        .collect();
    json_ok(
        &json!({ "gamemode": site_slug(&gamemode), "count": items.len(), "items": items }),
        o,
    )
}

async fn get_recent(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();
    let limit = clamp_limit(&req, 20, 100);

    let mut db = match Mongo::connect(&ctx.env).await {
        Ok(db) => db,
        Err(e) => return db_err(e, o),
    };

    let filter = doc! { "gamemode": active_gamemodes_filter() };
    let sort = doc! { "created_at": -1i32 };
    let rows = match db
        .find("test_log", filter, Some(sort), Some(limit as i64), None)
        .await
    {
        Ok(r) => r,
        Err(e) => return db_err(e, o),
    };

    let ids: Vec<i64> = rows.iter().filter_map(|r| doc_i64(r, "player_id")).collect();
    let players = match fetch_players_by_ids(&mut db, &ids).await {
        Ok(p) => p,
        Err(e) => return db_err(e, o),
    };
    let by_id: HashMap<i64, String> = players.into_iter().map(|p| (p.id, p.username)).collect();

    let items: Vec<Value> = rows
        .into_iter()
        .filter_map(|r| {
            let gamemode = doc_string(&r, "gamemode")?;
            if !is_active_gamemode(&gamemode) {
                return None;
            }
            Some(json!({
                "username": doc_i64(&r, "player_id").and_then(|id| by_id.get(&id).cloned()),
                "gamemode": site_slug(&gamemode),
                "tier": doc_string(&r, "tier")?,
                "previousTier": doc_string(&r, "previous_tier"),
                "region": doc_string(&r, "region"),
                "createdAt": doc_string(&r, "created_at")?,
            }))
        })
        .collect();
    json_ok(&json!({ "count": items.len(), "items": items }), o)
}

/// Edge cache TTL for successful and client-error GET responses. Upstream
/// (MongoDB) errors are never cached.
const EDGE_TTL_SECONDS: u32 = CACHE_TTL_SECONDS;

fn set_cors(headers: &Headers, origin: Option<&str>) -> Result<()> {
    headers.delete("access-control-allow-origin")?;
    if let Some(origin) = origin {
        headers.set("access-control-allow-origin", origin)?;
    }
    headers.set("vary", "Origin")?;
    Ok(())
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    let origin = allowed_origin(&req, &env);
    let o = origin.as_deref();

    // CORS preflight.
    if req.method() == Method::Options {
        let mut resp = Response::empty()?.with_status(204);
        let h = resp.headers_mut();
        set_cors(h, o)?;
        h.set("access-control-allow-methods", "GET, OPTIONS")?;
        h.set("access-control-allow-headers", "content-type")?;
        h.set("access-control-max-age", "86400")?;
        return Ok(resp);
    }

    if req.method() != Method::Get {
        return json_err(405, "method not allowed", o);
    }

    // Edge cache, keyed by full URL (path + query). Origin is not part of the
    // key, so CORS headers are set on every response below, not cached.
    let key = req.url()?.to_string();
    let cache = Cache::default();

    if let Some(mut hit) = cache.get(key.as_str(), false).await? {
        // Cache API responses have immutable headers, so rebuild the response.
        let status = hit.status_code();
        let body = hit.bytes().await?;
        let headers = Headers::new();
        headers.set("content-type", "application/json; charset=utf-8")?;
        headers.set("x-content-type-options", "nosniff")?;
        headers.set("cache-control", CACHE_CONTROL)?;
        set_cors(&headers, o)?;
        headers.set("x-cache", "HIT")?;
        return Ok(Response::from_bytes(body)?
            .with_status(status)
            .with_headers(headers));
    }

    let mut resp = Router::new()
        .get("/api/health", |_, _| json_ok(&json!({ "ok": true }), None))
        .get("/api/gamemodes", |_, _| {
            json_ok(
                &json!({ "gamemodes": GAMEMODES.iter().map(|g| site_slug(g)).collect::<Vec<_>>() }),
                None,
            )
        })
        .get_async("/api/leaderboard", get_leaderboard)
        .get_async("/api/leaderboard/global", get_global_leaderboard)
        .get_async("/api/players", get_players_batch)
        .get_async("/api/players/:username", get_player)
        .get_async("/api/gamemodes/:gamemode/tiers", get_gamemode_tiers)
        .get_async("/api/recent", get_recent)
        .or_else_any_method_async("/*path", |_, _| async { json_err(404, "not found", None) })
        .run(req, env)
        .await?;

    let status = resp.status_code();
    if status == 200 || status == 400 || status == 404 {
        // Store a copy with an explicit edge TTL. Cache-Control is what the
        // Cache API reads, so override the browser-facing value on the copy only.
        let mut copy = resp.cloned()?;
        copy.headers_mut().set(
            "cache-control",
            &format!("public, max-age={EDGE_TTL_SECONDS}, s-maxage={EDGE_TTL_SECONDS}"),
        )?;
        if let Err(e) = cache.put(key.as_str(), copy).await {
            console_error!("cache put failed: {e}");
        }
    }

    set_cors(resp.headers_mut(), o)?;
    resp.headers_mut().set("x-cache", "MISS")?;
    Ok(resp)
}
