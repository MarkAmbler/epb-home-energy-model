# hem-web — frontend for the HEM modelling service

A single-page, compare-focused UI over [`hem-server`](../hem-server): pick an archetype and weather,
describe an upgraded glazing spec, and see the baseline-vs-upgrade space-heat demand, running cost,
and carbon.

**Written entirely in Rust** (the [Yew](https://yew.rs) framework), compiled to WebAssembly with
[`trunk`](https://trunkrs.dev). There is no JavaScript, npm, or Node toolchain — only `cargo` and
`trunk`. The compiled bundle is static files that `hem-server` serves at `/`, so the UI and API share
an origin and no CORS layer is needed.

## Why it's excluded from the workspace

This crate targets `wasm32-unknown-unknown` and is bundled by `trunk`, not `cargo build`. It is in
the root `Cargo.toml`'s `[workspace] exclude` list so its wasm-world dependency graph never unifies
Cargo features with the native tokio/axum server crates (which would break one or the other). It
keeps its own `Cargo.lock`.

## Prerequisites (one-time)

```bash
rustup target add wasm32-unknown-unknown   # already present on the dev machine
cargo install trunk                        # the WASM bundler; manages its own wasm-bindgen
```

## Build & run

```bash
# 1. Build the static bundle (outputs to hem-web/dist/)
cd hem-web && trunk build --release && cd ..

# 2. Run the server (serves the API + the dist/ bundle at /)
HEM_SERVER_ADDR=127.0.0.1:8080 cargo run -p hem-server
# open http://127.0.0.1:8080/
```

`hem-server` looks for the bundle at `hem-web/dist` by default; override with `HEM_WEB_DIST`.

### Live-reload dev loop (optional)

`trunk serve` runs its own dev server with auto-rebuild. Point its API calls at a running
`hem-server` with a proxy:

```bash
# terminal 1
HEM_SERVER_ADDR=127.0.0.1:8080 cargo run -p hem-server
# terminal 2
cd hem-web && trunk serve --proxy-backend=http://127.0.0.1:8080/
```

## Scope

First cut is **compare-only** (design doc D-series): global glazing overrides (`u_value`, `g_value`,
`frame_area_fraction`) against the archetype's as-built windows. Per-window targeting,
shading/treatment, and single-run `/simulate` are supported by the API but not yet surfaced here.
