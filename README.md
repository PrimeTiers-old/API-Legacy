# PrimeTiers API

Public read-only API for PrimeTiers, written in Rust and running on a Cloudflare Worker. It reads from MongoDB Atlas (database `primetiers`) through a small MongoDB wire-protocol client built for Workers.

Live URL: `https://api.primetiers.net` (custom domain on the `primetiers-api` Worker; the `workers.dev` address also still works)

## Endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/api/health` | `{"ok":true}` |
| GET | `/api/gamemodes` | Supported gamemode IDs |
| GET | `/api/players?names=a,b,c` | **Batch lookup.** Up to 50 names, one Mongo query for players plus one for tiers. Returns `items` (found) and `missing` (not found). |
| GET | `/api/leaderboard` | **Full leaderboard.** Every gamemode ranked HT1 first, in one request. Add `?gamemode=macespear` to filter. |
| GET | `/api/leaderboard/global?limit=100` | **Global leaderboard.** Players ranked by total points across all active gamemodes. `limit` max 500. |
| GET | `/api/players/:username` | Single player with all tiers. 404 if not found. |
| GET | `/api/gamemodes/:gamemode/tiers?limit=50` | Tier list for one gamemode, newest first. `limit` max 100. |
| GET | `/api/recent?limit=20` | Latest tests across all gamemodes. `limit` max 100. |
| POST | `/api/network/report` | Ping results from the website's `/network` page. Saved to the `network_reports` collection. Reports from VPN/WARP/iCloud Private Relay networks are flagged `relay: true`. One report per IP per minute. Never cached. |
| POST | `/api/network/report/extra` | Adds the hand-run Karachi ping (`khi_ms`) and/or `trace` to a report, using the `token` the first POST returned. Within 1 hour, once. |
| GET | `/api/network/reports?limit=100` | **Admin.** Needs `Authorization: Bearer <NETWORK_ADMIN_KEY>`. Latest reports plus average ping per target, grouped by country and ASN. `limit` max 1000. |

Example batch response:

```json
{
  "count": 2,
  "items": [
    { "id": 1, "username": "Burnet01", "region": "NA", "platform": "premium",
      "tiers": [{ "gamemode": "macespear", "tier": "HT3", "updatedAt": "..." }] }
  ],
  "missing": ["nobody999"]
}
```

Names in `names=` are matched case-insensitively and deduplicated. Usernames may contain only letters, digits, `_`, and `.` (max 32 characters). Anything else returns 400.

### Leaderboard
`GET /api/leaderboard` returns every gamemode sorted best-first:

```json
{ "gamemodes": { "macespear": [
  { "rank": 1, "username": "Burnet01", "region": "NA", "tier": "HT3" }
] } }
```

Tier order (best to worst): HT1, LT1, HT2, LT2, HT3, LT3, HT4, LT4, HT5, LT5. Ties are sorted by username. Retired gamemodes (vanilla) are hidden from every response, including player, batch, and recent-test results. Use `?gamemode=` to get one board.

### Global leaderboard and points
`GET /api/leaderboard/global` sums each player's points across all active gamemodes:

```json
{ "count": 1, "items": [
  { "rank": 1, "username": "napatc1", "region": "AS", "points": 20,
    "tiers": [{ "gamemode": "crystal", "tier": "HT3" }, { "gamemode": "uhc", "tier": "HT3" }] }
] }
```

Points per tier are set in `POINTS` in `src/lib.rs`. **The current values are placeholders** (HT1 60, LT1 45, HT2 30, LT2 20, HT3 10, LT3 6, HT4 4, LT4 3, HT5 2, LT5 1). Replace them with the real scoring, then rebuild and redeploy.

### Gamemode names
The API returns the website's gamemode names: `nethertite` (database `nethop`) and `mace-spear` (database `macespear`). Requests accept either name. Everything else uses the same ids in both places.

### MongoDB collections
The API reads these collections:

- `players` — `_id` (numeric player id), `username`, `username_lower`, `region`, `platform`
- `player_tiers` — `player_id`, `gamemode`, `tier`, `updated_at`
- `test_log` — `player_id`, `gamemode`, `tier`, `previous_tier`, `region`, `created_at`

MongoDB does not do joins here, so the Worker fetches the matching player rows separately and joins in memory. This is cheap because the tier tables are small and the API is edge-cached.

The migration left `updated_at` / `created_at` in a mix of ISO strings and BSON dates; the API accepts both.

### Website
The site lives in `Website/` (also a Cloudflare Worker, named `primetiers`). It calls this API from the browser, so every origin that loads the site must be listed in `ALLOWED_ORIGINS` in `wrangler.toml`. Current origins: `https://primetiers.net`, the site's `workers.dev` address, and `http://localhost:5173` for Vite dev.

### Caching
- Every GET response is cached for **5 minutes** (`CACHE_TTL_SECONDS` in `src/lib.rs`), keyed by full URL. Repeat requests return `x-cache: HIT` and don't touch MongoDB. Browsers also keep the response for 5 minutes (`max-age=300`), so a visitor's repeat loads don't hit the API at all.
- Cache is per Cloudflare data center. Visitors in the same region share one fetch every 5 minutes. Data can be up to 5 minutes old.
- 200, 400, and 404 responses are cached. Upstream errors (502) are never cached.
- CORS is allowed only for origins listed in `ALLOWED_ORIGINS` (`wrangler.toml`).

### Website usage
Fetch many players in one request:

```js
const res = await fetch(
  "https://api.primetiers.net/api/players?names=" +
    encodeURIComponent(names.join(","))
);
const { items, missing } = await res.json();
```

Keep batches at 50 names or fewer. Larger lists return 400.

## Project layout

```
Cargo.toml         Rust crate (worker 0.7, bson, tokio, crypto crates)
src/lib.rs         All routes, queries, and the edge cache
src/mongo.rs       Minimal MongoDB client: TCP/TLS, SRV, SCRAM auth, OP_MSG
wrangler.toml      Worker name, MONGODB_DB, ALLOWED_ORIGINS, build command
.dev.vars.example  Template for local secrets (copy to .dev.vars)
```

## MongoDB connection

The official `mongodb` Rust crate cannot compile to wasm, so `src/mongo.rs` implements the small part of the MongoDB wire protocol this API needs:

- outbound TCP/TLS through `worker::Socket` (`cloudflare:sockets`)
- `mongodb+srv://` discovery through DNS-over-HTTPS SRV records
- SCRAM-SHA-256, with automatic SCRAM-SHA-1 fallback for Atlas users that only expose SHA-1
- following the replica-set primary advertised in `hello`
- `find` / `getMore` with BSON encoding and decoding

Configuration:

- `MONGODB_URI` — **secret**. The full Atlas connection string.
- `MONGODB_DB` — plain var in `wrangler.toml`, currently `primetiers`.

Use an Atlas database user with read access to `primetiers`, plus `insert` on `network_reports`. That collection is the only thing the API writes.

## Prerequisites
- Rust (rustup) with the wasm target: `rustup target add wasm32-unknown-unknown`
- Node.js (for `npx wrangler`)
- `cargo install worker-build --version 0.7.5`

`worker` is pinned to 0.7 because `worker-build` 0.8.x fails in its wasm-bindgen step on Windows.

## Build

```bash
cargo check --target wasm32-unknown-unknown   # type-check only
worker-build --release                        # produces build/worker/
```

`wrangler deploy` builds the Worker itself, using the `[build]` command in `wrangler.toml`. You don't need a separate build step.

## Run locally

1. Copy the secret template and fill it in:
   ```bash
   cp .dev.vars.example .dev.vars
   ```
   Put the real Atlas connection string in `MONGODB_URI`.
2. Start the dev server:
   ```bash
   npx wrangler dev
   ```
3. Test it at `http://127.0.0.1:8787/api/health`.

## Deploy to Cloudflare

1. Log in once: `npx wrangler login`
2. Set the MongoDB URI as a secret. This must be redone if the Worker is deleted or recreated:
   ```bash
   npx wrangler secret put MONGODB_URI
   ```
3. Deploy (this also builds the Worker):
   ```bash
   npx wrangler deploy
   ```
4. Check the secret is present with `npx wrangler secret list`.

Every deploy is a manual upload from your machine. There is no Git-based auto-deploy. Changing `MONGODB_DB` or `ALLOWED_ORIGINS` means editing `wrangler.toml` and redeploying.

## Cloudflare Workers Builds (dashboard)

If the Worker is connected to a repo in the Cloudflare dashboard (**Workers & Pages → primetiers-api → Settings → Builds**), set these. The build image does not include Rust, so the build command installs it first.

- **Build command:**
  ```
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal && . "$HOME/.cargo/env" && rustup target add wasm32-unknown-unknown && cargo install -q worker-build --version 0.7.5 --locked && worker-build --release
  ```
- **Deploy command:** `npx wrangler deploy` (it runs the `[build]` command from `wrangler.toml`, so the build step above is only needed to install Rust)
- **Root directory:** the repo root (where `wrangler.toml` is)

The first build is slow (several minutes) because it compiles `worker-build` from source. Later builds are faster if Cloudflare caches `~/.cargo`.

## Troubleshooting

- **`502 upstream unavailable`**: the Worker can't reach MongoDB or authentication failed. Check:
  - `MONGODB_URI` exists (`npx wrangler secret list`)
  - the Atlas database user/password are correct
  - the user has read access to the `primetiers` database
  - Atlas network access allows Cloudflare Workers (Atlas normally allows all IPs by default)
  - the Worker logs (`wrangler tail`) show the detailed Mongo error
- **`not primary and secondaryOk=false`**: the client normally follows the primary automatically. If it persists, the primary host in the `hello` response is unreachable from Workers.
- **Empty `updatedAt` / tiers**: check that the collection still uses the expected field names listed above.

## Limits (Cloudflare Workers Free)
- 100,000 requests per day, resetting at midnight UTC
- 10 ms CPU per request
- Outbound TCP sockets are available on Workers; each request opens one MongoDB connection
- The edge cache means most requests do not touch MongoDB at all

## Never commit
`.env`, `.dev.vars`, and any database credentials. `.gitignore` already excludes them.
