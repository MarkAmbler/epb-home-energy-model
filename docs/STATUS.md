# Project status & resume notes

A living, version-controlled summary so work can resume on any machine (and so a fresh Claude Code
session reading this repo has the context that would otherwise live only in local session memory).
Update this as the modelling-service work progresses.

_Last updated: 2026-07-07._

## Two parallel bodies of work

### 1. Engine parity (COMPLETE, on `main`)
The Rust HEM engine is at 1:1 behavioural parity with the Python reference: `cargo test --test e2e`
= 0 diffs / 4,993,698 fields, on Windows and Linux CI. Achieved via three solver-substitution fixes
(RK45 `solve_ivp`, emitter warm-up start temp, ventilation `p_z_ref` brentq). e2e is a CI gate on
every push. See git history around merge `b7dadf97`.

### 2. Modelling service (IN PROGRESS, branch `feat/modelling-service-phases-1-2`)
A web-service/API layer on top of the engine so non-experts can run simulations from an archetype +
a few glazing parameters instead of authoring the ~8,000-line HEM input JSON. Full design and scope:
[`docs/design/modelling-service-design.md`](design/modelling-service-design.md).

**Architecture principle:** the engine crate stays a thin fork of upstream; ALL product code lives
in new workspace crates (`hem-profiles`, `hem-api`, `hem-server`). Phase 1–2 changed zero engine
source, so parity is unaffected.

**Delivered & verified (Phases 1–2):**
- `hem-profiles` — archetype templates: `detached_demo` (fast 8-step test fixture, figures NOT
  meaningful) and `flat_nat_vent` (realistic full-period flat, 4 windows, ~1435 kWh baseline heat).
- `hem-api` — transport-agnostic core: `GlazingOverrides` (`u_value` is the primary knob and is a
  DIRECT passthrough — the engine's `UValueInput` takes either `u_value` OR
  `thermal_resistance_construction`, mutually exclusive; no conversion needed), `simulate`,
  `compare`, typed `ApiError`. Returns `OutputSummary` (the full `Output` can't serialize to JSON —
  it has `Option`-keyed maps).
- `hem-server` — thin Axum transport: `GET /healthz`, `GET /archetypes`, `POST /simulate`,
  `POST /compare`. CPU-bound runs go on the blocking pool.
- Live check: `flat_nat_vent`, as-built vs U=0.8/g=0.5 → space-heat demand 1435→861 kWh (~40% cut),
  correct direction. 10 unit tests pass.

## Strategic context (verified 2026-07-07)
- HEM is NOT statutory yet: SAP **10.3** is the sole approved method at Future Homes Standard launch
  (FHS in force 24 Mar 2027); HEM available ≥3 months later, parallel ≥24 months; HEM-based EPC
  reform slipped to H2 2027. So this product's near-term value is design/sales/research, NOT
  compliance certificates.
- DESNZ is building an official cloud HEM API — **ECaaS** (Energy Calculation as a Service), due
  autumn 2026.
- **Agreed direction:** don't chase compliance output from our fork (route that via ECaaS/the
  official engine when it lands); make the calculation engine a **pluggable backend** behind
  `hem-api` (local engine now, ECaaS later) — this is the recommended next architectural step.

## Recommended next steps
1. Pluggable engine-backend trait in `hem-api` (local vs ECaaS).
2. Cost & carbon figures in the `/compare` delta (not just kWh).
3. Weather-by-location; per-window targeting; shading/`treatment` overrides.
4. More realistic archetypes (a true full 8760-hour year; a detached house).

## Known issues
- `hem-lambda` does NOT compile — it `include_str!`s `../../src/weather.epw`, which doesn't exist and
  is untracked. Pre-existing (present at `main`); CI misses it because it runs `just unit` + the
  engine e2e, not `cargo build --workspace`. `cargo build --workspace` therefore fails on this crate.
  The three new crates build and test cleanly in isolation. Fix/replace hem-lambda's weather bundling
  if we base a real Lambda transport on `hem-api`.

## How to run / verify
```bash
# Unit tests for the new crates
cargo test -p hem-api -p hem-profiles

# Run the service (Bash tool; picks up the .claude/settings.json cargo allowlist)
HEM_SERVER_ADDR=127.0.0.1:8099 cargo run -p hem-server
# then: GET /archetypes ; POST /simulate {"archetype":"flat_nat_vent","glazing_overrides":{"u_value":0.8}}
#       POST /compare  {"archetype":"flat_nat_vent","upgrade_overrides":{"u_value":0.8,"g_value":0.5}}
```
Prereqs: `rustup` toolchain (Rust ≥ 1.85) and git. No weather/tariff files needed for the two
bundled archetypes — the service bundles the CIBSE London weather.
