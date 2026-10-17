# primetiers-api (Cloudflare Worker, Rust)

Public read-only API for PrimeTiers. Reads Supabase through PostgREST with the
**publishable/anon key** (existing RLS "public read" policies). Writes stay in
the Discord bot (`../Queue-Bot`), which uses the service role.

## Routes
- `GET /api/health`
- `GET /api/gamemodes`
- `GET /api/players/:username`
- `GET /api/gamemodes/:gamemode/tiers?limit=50` (max 100)
- `GET /api/recent?limit=20` (max 100)

## Setup
1. `npx wrangler login`
2. Put your project URL in `wrangler.toml` (`SUPABASE_URL`).
3. Store the anon key as a secret: `npx wrangler secret put SUPABASE_ANON_KEY`
   (never the service-role key).
4. Local dev: copy `.dev.vars.example` to `.dev.vars` and run `npx wrangler dev`.
5. Deploy: `npx wrangler deploy`

## Build
`worker-build --release` (install: `cargo install worker-build --version 0.7.5`). `worker` is pinned to 0.7 because worker-build 0.8.x fails in its wasm-bindgen step on Windows.

## Status
- Builds: `worker-build --release` succeeds.
- Tested locally with `wrangler dev` against a mock PostgREST: all routes, CORS, validation, and 404s behave correctly.
- Not yet deployed: needs `wrangler login` and your real Supabase URL and anon key.
