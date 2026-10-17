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
use worker::*;

const GAMEMODES: &[&str] = &[
    "crystal", "axe", "sword", "mace", "macespear", "nethop", "pot", "smp", "uhc", "cart",
];

const CACHE_CONTROL: &str = "public, max-age=15, s-maxage=60, stale-while-revalidate=300";

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

fn with_common_headers(mut resp: Response, cache: bool, origin_ok: Option<&str>) -> Result<Response> {
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
    let list = env.var("ALLOWED_ORIGINS").map(|v| v.to_string()).unwrap_or_default();
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
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
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
        .map(|t| json!({ "gamemode": t.gamemode, "tier": t.tier, "updatedAt": t.updated_at }))
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

async fn get_gamemode_tiers(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let origin = allowed_origin(&req, &ctx.env);
    let o = origin.as_deref();
    let gamemode = ctx.param("gamemode").map(|s| s.to_lowercase()).unwrap_or_default();
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
    json_ok(&json!({ "gamemode": gamemode, "count": items.len(), "items": items }), o)
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
        .map(|r| {
            json!({
                "username": r.players.map(|p| p.username),
                "gamemode": r.gamemode,
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
const EDGE_TTL_SECONDS: u32 = 60;

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
        return Ok(Response::from_bytes(body)?.with_status(status).with_headers(headers));
    }

    let mut resp = Router::new()
        .get("/api/health", |_, _| json_ok(&json!({ "ok": true }), None))
        .get("/api/gamemodes", |_, _| json_ok(&json!({ "gamemodes": GAMEMODES }), None))
        .get_async("/api/players/:username", get_player)
        .get_async("/api/gamemodes/:gamemode/tiers", get_gamemode_tiers)
        .get_async("/api/recent", get_recent)
        .or_else_any_method_async("/*path", |_, _| async {
            json_err(404, "not found", None)
        })
        .run(req, env)
        .await?;

    let status = resp.status_code();
    if status == 200 || status == 400 || status == 404 {
        // Store a copy with an explicit edge TTL. Cache-Control is what the
        // Cache API reads, so override the browser-facing value on the copy only.
        let mut copy = resp.cloned()?;
        copy.headers_mut().set(
            "cache-control",
            &format!("public, max-age=15, s-maxage={EDGE_TTL_SECONDS}"),
        )?;
        if let Err(e) = cache.put(key.as_str(), copy).await {
            console_error!("cache put failed: {e}");
        }
    }

    set_cors(resp.headers_mut(), o)?;
    resp.headers_mut().set("x-cache", "MISS")?;
    Ok(resp)
}
