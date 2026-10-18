# PrimeTiers API

Public read-only API for PrimeTiers, written in Rust and running on a Cloudflare Worker. It reads from Supabase through PostgREST using the **publishable (anon) key**. The existing Row Level Security "public read" policies control what's visible. Writes stay in the Discord bot, which uses the service role key.

Live URL: `https://api.primetiers.net` (custom domain on the `primetiers-api` Worker; the `workers.dev` address also still works)

## Endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/api/health` | `{"ok":true}` |
| GET | `/api/gamemodes` | Supported gamemode IDs |
| GET | `/api/players?names=a,b,c` | **Batch lookup.** Up to 50 names, one Supabase query. Returns `items` (found) and `missing` (not found). |
| GET | `/api/leaderboard` | **Full leaderboard.** Every gamemode ranked HT1 first, in one request. Add `?gamemode=macespear` to filter. |
| GET | `/api/leaderboard/global?limit=100` | **Global leaderboard.** Players ranked by total points across all active gamemodes. `limit` max 500. |
| GET | `/api/players/:username` | Single player with all tiers. 404 if not found. |
| GET | `/api/gamemodes/:gamemode/tiers?limit=50` | Tier list for one gamemode, newest first. `limit` max 100. |
| GET | `/api/recent?limit=20` | Latest tests across all gamemodes. `limit` max 100. |

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

### Website
The site lives in `Website/` (also a Cloudflare Worker, named `primetiers`). It calls this API from the browser, so every origin that loads the site must be listed in `ALLOWED_ORIGINS` in `wrangler.toml`. Current origins: `https://primetiers.net`, the site's `workers.dev` address, and `http://localhost:5173` for Vite dev.

### Caching
- Every GET response is cached for **5 minutes** (`CACHE_TTL_SECONDS` in `src/lib.rs`), keyed by full URL. Repeat requests return `x-cache: HIT` and don't call Supabase. Browsers also keep the response for 5 minutes (`max-age=300`), so a visitor's repeat loads don't hit the API at all.
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
Cargo.toml         Rust crate (worker 0.7, serde, serde_json)
src/lib.rs         All routes and the edge cache
wrangler.toml      Worker name, SUPABASE_URL, ALLOWED_ORIGINS, build command
.dev.vars.example  Template for local secrets (copy to .dev.vars)
```

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
   Put the **publishable key** in `.dev.vars`. Never use the service-role key.
2. Start the dev server:
   ```bash
   npx wrangler dev
   ```
3. Test it at `http://127.0.0.1:8787/api/health`.

To point local dev at a different Supabase project, pass `--var SUPABASE_URL:https://...`.

## Deploy to Cloudflare

1. Log in once: `npx wrangler login`
2. Set the Supabase key as a secret. This must be redone if the Worker is deleted or recreated:
   ```bash
   npx wrangler secret put SUPABASE_ANON_KEY
   ```
3. Deploy (this also builds the Worker):
   ```bash
   npx wrangler deploy
   ```
4. Check the secret is present with `npx wrangler secret list`.

Every deploy is a manual upload from your machine. There is no Git-based auto-deploy. Changing `SUPABASE_URL` or `ALLOWED_ORIGINS` means editing `wrangler.toml` and redeploying.

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

- **`502 upstream unavailable`**: the Worker can't reach Supabase. Check that the `SUPABASE_ANON_KEY` secret exists (`npx wrangler secret list`) and that the project URL is correct. Test the key directly against `<SUPABASE_URL>/rest/v1/players?select=username&limit=1`.
- **`permission denied for table ...`** from Supabase: the `anon` role needs `SELECT` on the table. Run:
  ```sql
  grant select on public.players, public.player_tiers, public.test_log,
    public.queue_entries, public.queue_testers, public.queue_closed,
    public.live_tests to anon;
  ```
- **`PGRST201` ambiguous relationship**: `test_log` has two foreign keys to `players`. The Worker uses `players!test_log_player_id_fkey`; keep that hint if you change the query.

## Limits (Cloudflare Workers Free)
- 100,000 requests per day, resetting at midnight UTC
- 10 ms CPU per request
- 50 subrequests per request (the API uses one)

## Never commit
`.env`, `.dev.vars`, and any service-role key. `.gitignore` already excludes them.
