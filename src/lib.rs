//! PrimeTiers public read API, running on Cloudflare Workers (Rust / Wasm).
//!
//! Reads from Supabase PostgREST with the publishable (anon) key. The tables
//! exposed here already have "public read" RLS policies, so no privileged key
//! is needed. Writes stay in the Discord bot, which uses the service role.
//!
//! Routes:
//!   GET /api/health
//!   GET /api/gamemodes
//!   GET /api/players/:username
//!   GET /api/gamemodes/:gamemode/tiers?limit=50
//!   GET /api/recent?limit=20

use serde::{Deserialize, Serialize};
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

#[derive(Deserialize)]
struct PlayerTier {
    gamemode: String,
    tier: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct PlayerRow {
    id: i64,
    username: String,
    region: Option<String>,
    platform: Option<String>,
    player_tiers: Vec<PlayerTier>,
}

#[derive(Deserialize)]
struct PlayerRef {
    username: String,
}

#[derive(Deserialize)]
struct TierRow {
    tier: String,
    updated_at: String,
    players: PlayerRef,
}

#[derive(Deserialize)]
struct LogRow {
    gamemode: String,
    tier: String,
    previous_tier: Option<String>,
    region: Option<String>,
    created_at: String,
    players: Option<PlayerRef>,
}

#[derive(Serialize)]
struct ApiError<'a> {
    error: &'a str,
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

/// Usernames: 1–32 chars of letters, digits, underscore, dot. Keeps the value
/// safe to put into a PostgREST query string without any encoding.
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

/// Calls PostgREST: GET {SUPABASE_URL}/rest/v1/{path_and_query}
async fn supabase_get<T: for<'de> Deserialize<'de>>(env: &Env, path_and_query: &str) -> Result<T> {
    let base = env.var("SUPABASE_URL")?.to_string();
    let key = env.secret("SUPABASE_ANON_KEY")?.to_string();
    let url = format!("{}/rest/v1/{}", base.trim_end_matches('/'), path_and_query);

    let headers = Headers::new();
    headers.set("apikey", &key)?;
    headers.set("authorization", &format!("Bearer {key}"))?;
    headers.set("accept", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = Request::new_with_init(&url, &init)?;

    let mut resp = Fetch::Request(request).send().await?;
    if resp.status_code() >= 400 {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::RustError(format!(
            "supabase {}: {}",
            resp.status_code(),
            text
        )));
    }
    resp.json::<T>().await
}

/// PostgREST caps each response at max_rows (1000 by default), so one request
/// silently truncates a big table. This pages through the whole table instead.
/// Ordered by (player_id, gamemode), which is unique, so pages never overlap.
const PAGE_SIZE: u32 = 1000;
const MAX_PAGES: u32 = 20; // 20,000 rows ceiling

async fn supabase_get_all<T: for<'de> Deserialize<'de>>(env: &Env, base: &str) -> Result<Vec<T>> {
    let mut out: Vec<T> = Vec::new();
    for page in 0..MAX_PAGES {
        let offset = page * PAGE_SIZE;
        let path = format!("{base}&order=player_id.asc,gamemode.asc&limit={PAGE_SIZE}&offset={offset}");
        let rows: Vec<T> = supabase_get(env, &path).await?;
        let n = rows.len();
        out.extend(rows);
        if n < PAGE_SIZE as usize {
            return Ok(out);
        }
    }
    console_error!("leaderboard reached {} rows, raise MAX_PAGES", MAX_PAGES * PAGE_SIZE);
    Ok(out)
}

/// Maps Supabase failures to 502 and logs the detail (never the key).
fn upstream_err(e: Error, origin: Option<&str>) -> Result<Response> {
    console_error!("upstream error: {e}");
    json_err(502, "upstream unavailable", origin)
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

    let path = format!(
        "players?select=id,username,region,platform,player_tiers(gamemode,tier,updated_at)&username_lower=eq.{}&limit=1",
        name.to_lowercase()
    );
    let rows: Vec<PlayerRow> = match supabase_get(&ctx.env, &path).await {
        Ok(r) => r,
        Err(e) => return upstream_err(e, o),
    };
    let Some(p) = rows.into_iter().next() else {
        return json_err(404, "player not found", o);
    };

    let tiers: Vec<Value> = p
        .player_tiers
        .iter()
        .filter(|t| is_active_gamemode(&t.gamemode))
        .map(|t| json!({ "gamemode": site_slug(&t.gamemode), "tier": t.tier, "updatedAt": t.updated_at }))
        .collect();
    json_ok(
        &json!({
            "id": p.id,
            "username": p.username,
            "region": p.region,
            "platform": p.platform,
            "tiers": tiers,
        }),
        o,
    )
}

#[derive(Deserialize)]
struct LbPlayer {
    id: i64,
    username: String,
    region: Option<String>,
}

#[derive(Deserialize)]
struct LbRow {
    gamemode: String,
    tier: String,
    players: Option<LbPlayer>,
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

/// GET /api/leaderboard[?gamemode=macespear]
/// Every gamemode's ranking in one Supabase query, sorted HT1 first.
/// Edge-cached like every other GET, so visitors share one upstream call per minute.
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

    let rows: Vec<LbRow> = match supabase_get_all(
        &ctx.env,
        "player_tiers?select=gamemode,tier,players(id,username,region)",
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return upstream_err(e, o),
    };

    // gamemode -> (rank, username, region, tier)
    let mut boards: Vec<(&str, Vec<(u8, String, Option<String>, String)>)> =
        GAMEMODES.iter().map(|gm| (*gm, Vec::new())).collect();

    for row in rows {
        let Some(board) = boards.iter_mut().find(|(gm, _)| *gm == row.gamemode) else {
            continue;
        };
        if filter.as_deref().is_some_and(|f| f != row.gamemode) {
            continue;
        }
        let (Some(rank), Some(p)) = (tier_rank(&row.tier), row.players) else {
            continue;
        };
        board
            .1
            .push((rank, p.username, p.region, row.tier.to_ascii_uppercase()));
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
/// Players ranked by total points across all active gamemodes (most first).
async fn get_global_leaderboard(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();
    let limit = clamp_limit(&req, 100, 500) as usize;

    let rows: Vec<LbRow> = match supabase_get_all(
        &ctx.env,
        "player_tiers?select=gamemode,tier,players(id,username,region)",
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return upstream_err(e, o),
    };

    let mut totals: HashMap<i64, GlobalAgg> = HashMap::new();
    for row in rows {
        if !is_active_gamemode(&row.gamemode) {
            continue;
        }
        let (Some(p), Some(pts)) = (row.players, tier_points(&row.tier)) else {
            continue;
        };
        let entry = totals.entry(p.id).or_insert_with(|| GlobalAgg {
            username: p.username.clone(),
            region: p.region.clone(),
            points: 0,
            tiers: Vec::new(),
        });
        entry.points += pts;
        entry
            .tiers
            .push((row.gamemode, row.tier.to_ascii_uppercase()));
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
/// One Supabase query for up to BATCH_MAX names. Returns found players in
/// `items` and any requested names that don't exist in `missing`.
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

    // Names are validated to [A-Za-z0-9_.], so no encoding is needed inside in.(...).
    let path = format!(
        "players?select=id,username,region,platform,player_tiers(gamemode,tier,updated_at)&username_lower=in.({})",
        names.join(",")
    );
    let rows: Vec<PlayerRow> = match supabase_get(&ctx.env, &path).await {
        Ok(r) => r,
        Err(e) => return upstream_err(e, o),
    };

    let found: Vec<String> = rows.iter().map(|p| p.username.to_lowercase()).collect();
    let missing: Vec<&String> = names.iter().filter(|n| !found.contains(n)).collect();
    let items: Vec<Value> = rows.iter().map(player_json).collect();

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
        .map(|t| json!({ "gamemode": site_slug(&t.gamemode), "tier": t.tier, "updatedAt": t.updated_at }))
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

    let path = format!(
        "player_tiers?select=tier,updated_at,players(username)&gamemode=eq.{gamemode}&order=updated_at.desc&limit={limit}"
    );
    let rows: Vec<TierRow> = match supabase_get(&ctx.env, &path).await {
        Ok(r) => r,
        Err(e) => return upstream_err(e, o),
    };
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| json!({ "username": r.players.username, "tier": r.tier, "updatedAt": r.updated_at }))
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

    let path = format!(
        "test_log?select=gamemode,tier,previous_tier,region,created_at,players!test_log_player_id_fkey(username)&order=created_at.desc&limit={limit}"
    );
    let rows: Vec<LogRow> = match supabase_get(&ctx.env, &path).await {
        Ok(r) => r,
        Err(e) => return upstream_err(e, o),
    };
    let items: Vec<Value> = rows
        .into_iter()
        .filter(|r| is_active_gamemode(&r.gamemode))
        .map(|r| {
            json!({
                "username": r.players.map(|p| p.username),
                "gamemode": site_slug(&r.gamemode),
                "tier": r.tier,
                "previousTier": r.previous_tier,
                "region": r.region,
                "createdAt": r.created_at,
            })
        })
        .collect();
    json_ok(&json!({ "count": items.len(), "items": items }), o)
}

/// Edge cache TTL for successful and client-error GET responses. Upstream
/// (Supabase) errors are never cached.
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
