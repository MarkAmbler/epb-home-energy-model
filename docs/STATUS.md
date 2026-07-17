# Project status & resume notes

A living, version-controlled summary so work can resume on any machine (and so a fresh Claude Code
session reading this repo has the context that would otherwise live only in local session memory).
Update this as the modelling-service work progresses.

_Last updated: 2026-07-17._

## Two parallel bodies of work

### 1. Engine parity (COMPLETE, on `main`)
The Rust HEM engine is at 1:1 behavioural parity with the Python reference: `cargo test --test e2e`
= 0 diffs / 4,993,698 fields, on Windows and Linux CI. Achieved via three solver-substitution fixes
(RK45 `solve_ivp`, emitter warm-up start temp, ventilation `p_z_ref` brentq). e2e is a CI gate on
every push. See git history around merge `b7dadf97`.

### 2. Modelling service (MERGED TO MAIN; core feature set complete)
A web-service/API layer on top of the engine so non-experts can run simulations from an archetype +
a few glazing parameters instead of authoring the ~8,000-line HEM input JSON. Full design and scope:
[`docs/design/modelling-service-design.md`](design/modelling-service-design.md). API reference (endpoints,
request/response shapes, overrides, economics): [`docs/modelling-service-api.md`](modelling-service-api.md).

All of the below is on `main` (merged via PRs #4–#7). Both CI workflows are green on `main`: the
engine parity gate (`Rust project`) and product-crate tests (`Modelling service crates`, added in #5
so the new crates are covered — `just unit` only tests the engine crate). The engine crate is
unchanged, so parity/rebaseability are intact.

**Architecture principle:** the engine crate stays a thin fork of upstream; ALL product code lives
in new workspace crates (`hem-profiles`, `hem-api`, `hem-server`). Zero engine source changed.

**Delivered & verified (all on `main`):**
- `hem-profiles` — four archetype templates: `flat_nat_vent` (realistic nat-vent flat, 4 windows,
  4380 h, ~1435 kWh baseline heat), `flat_new_build_uk` (same envelope, glazing at the current UK
  new-build standard U=1.4/g=0.63 — an illustrative preset), `detached_bungalow_uk` (illustrative
  single-storey detached dwelling; see remaining-work note on its indicative status), and
  `detached_demo` (fast 8-step test fixture, figures NOT meaningful).
- `hem-api` — transport-agnostic core: `GlazingOverrides` (`u_value` is the primary knob and is a
  DIRECT passthrough — the engine's `UValueInput` takes either `u_value` OR
  `thermal_resistance_construction`, mutually exclusive; no conversion needed), `simulate`,
  `compare`, typed `ApiError`. Returns `OutputSummary` (the full `Output` can't serialize to JSON —
  it has `Option`-keyed maps).
- `hem-server` — thin Axum transport: `GET /healthz`, `GET /archetypes`, `GET /weather`,
  `POST /simulate`, `POST /compare`. CPU-bound runs go on the blocking pool. Requests reject unknown
  fields (`deny_unknown_fields`) so a typo like `glazing_override` is a 422, not a silent baseline run.
- **Cost & carbon** (design doc D2): `simulate`/`compare` turn delivered energy into running cost
  (£) and carbon (kgCO₂e). Factors are per **fuel type** (`Economics`/`FuelFactors`, keyed by the
  engine's snake_case fuel names), caller-supplied and **echoed in the response** (`economics_used`)
  so a result is self-documenting; omitted ⇒ `Economics::uk_defaults` (Ofgem price cap 1 Jul–30 Sep
  2026 + DESNZ/DEFRA 2025 GHG factors — illustrative, not authoritative). Unit-rate only (standing
  charges excluded — they cancel in an A/B comparison). Engine-internal supplies
  (`_energy_from_environment`, `_unmet_demand`) are correctly zero-costed; a real fuel with no
  supplied factors is a 422.
- **Per-window / by-orientation targeting** (design doc D4): `glazing_overrides` stays the global
  default (all windows); `targeted_overrides: [{select:{names,orientations}, overrides}]` refine
  specific windows, applied after the global with later rules winning per field. A `WindowSelector`
  matches when every non-empty criterion matches (AND); empty = all. `compare` has
  `baseline_targeted`/`upgrade_targeted`; `upgrade_overrides` is now optional (upgrade can be
  expressed purely by targeting). Responses carry a `windows` inventory (zone/name/orientation/pitch)
  so callers know what to target.
- **Shading & treatment overrides** (design doc §6.1): `GlazingOverrides` gains `shading` and
  `treatment` (both `Option<Vec<Value>>`, so both are per-window targetable). `Some(list)` replaces
  the element's array, `Some([])` clears it, `None` leaves it. Entries pass through verbatim; the
  engine's core schema validates the assembled input, so a malformed entry surfaces as a **422**
  (this deliberately avoids re-encoding the unstable schema, constraint C2). Shading = overhang/
  sidefin/reveal/obstacle geometry (self-contained). Treatment = blinds/curtains; only control-free
  (fixed `is_open`) treatments work today — `Control_*` fields reference `$.Control` keys the current
  archetypes don't have.
- **Weather selection**: `weather` field on requests (default `london_cibse`), `GET /weather`,
  id→conditions resolution, unknown id → 404, echoed in responses. Only London is listed — the
  bundled CIBSE and EPW files are the *same* London year (verified), so exposing both would be a
  false choice; `WEATHER_SOURCES` in `hem-api` is the one-line extension point for real locations.
- Live check: `flat_nat_vent`, as-built vs U=0.8/g=0.5 → space-heat demand 1435→861 kWh (~40% cut),
  cost −£150.08, carbon −101.7 kgCO₂e over the simulated period (4380 h — NOT a full year), correct
  direction. Targeting: all-4 windows U=0.8 → −735.1 kWh; 2 living-room windows only → −363.3 kWh
  (~half, as expected); orientation `[90]` → all 4 (all face east). Shading: as-built 1435 kWh,
  shading removed 1188 kWh (more solar → less heat), deep overhang 1775 kWh (less solar → more heat),
  monotonic and correct. Treatment: closed manual curtains all windows → +175.8 kWh heat (block
  winter sun). `detached_bungalow_uk`: 1754.8 kWh (> the flat, as expected for more external
  surface). 38 unit tests pass (34 `hem-api` + 4 `hem-profiles`; `cargo test -p hem-api -p hem-profiles`).

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

## Remaining work (all data/product-gated — the core code levers are done)

The feature set (cost/carbon, per-window/orientation targeting, shading/treatment, weather selection,
two illustrative archetypes, request-field hardening, CI coverage) is delivered and on `main`. What's
left is gated on external data or product decisions, not code:

1. **Real regional weather** — unblocks weather-by-location beyond London. Source regional weather
   files (provenance/licensing is a user decision), then add each as a one-line `WEATHER_SOURCES`
   entry in `hem-api`. Only one real dataset exists today (the bundled CIBSE and EPW files are the
   same London year).
2. **A faithful (surveyed) dwelling archetype (design doc D5).** The two illustrative archetypes are
   the honest limit without real data. **Key constraint (discovered, keep in mind):** the engine does
   NOT cycle schedules ("Schedule length is less than the expected length"), and of the 111 core
   demos only `flat_nat_vent` has a validated long period (4380 h) — the longer demos are engine
   *test fixtures* (23–48 kWh space-heat over ~300 days, several with PV), less realistic than
   `flat_nat_vent`. So any archetype must reuse `flat_nat_vent`'s machinery; a structurally new /
   full-year / detached dwelling needs real dwelling parameters, or accepts fabricated schedules
   (which is why `detached_bungalow_uk` is labelled indicative). A real dwelling spec from the user
   would turn into a validated archetype.
3. **PV / on-site-generation cost net-off** — deferred until a generation-bearing archetype exists
   (see Known issues). Would base cost on net grid import + an export credit instead of gross
   consumption; needs an export/feed-in price decision and can only be verified against a real PV
   archetype.
4. **Pluggable engine-backend trait (local vs ECaaS)** — deferred until ECaaS is concrete (autumn
   2026 at the earliest); its API shape is unknown, so abstracting against it now would be guesswork.

## Known issues
- **Cost/carbon assume no on-site generation.** The cost/carbon base is delivered energy = *gross
  consumption per fuel*, which equals metered grid import only when generation/export are zero (true
  for all current archetypes). A PV/generation archetype would need the base switched to
  `energy_supply` net import + an export credit. Verified boundary, not a current bug.
- `hem-lambda` does NOT compile — it `include_str!`s `../../src/weather.epw`, which doesn't exist and
  is untracked. Pre-existing (present at `main`). Neither CI workflow builds it (engine `test.yml`
  runs `just unit` + e2e; `modelling-service.yml` tests `-p hem-api -p hem-profiles -p hem-server` by
  name), so `cargo build --workspace` still fails on this crate alone. The three product crates build
  and test cleanly. Fix/replace hem-lambda's weather bundling if we base a real Lambda transport on
  `hem-api`.

## How to run / verify
```bash
# Unit tests for the product crates (this is also the modelling-service CI workflow)
cargo test -p hem-api -p hem-profiles -p hem-server

# Run the service (Bash tool; picks up the .claude/settings.json cargo allowlist)
HEM_SERVER_ADDR=127.0.0.1:8099 cargo run -p hem-server
# then: GET /archetypes ; GET /weather
#       POST /simulate {"archetype":"flat_nat_vent","glazing_overrides":{"u_value":0.8}}
#       POST /compare  {"archetype":"flat_nat_vent","upgrade_overrides":{"u_value":0.8,"g_value":0.5}}
# request fields: glazing_overrides {u_value|thermal_resistance_construction, g_value,
#   frame_area_fraction, shading[], treatment[]}, targeted_overrides[{select,overrides}],
#   weather, economics {fuels}, options.  See docs/modelling-service-api.md for the full contract.
```
Prereqs: `rustup` toolchain (Rust ≥ 1.85) and git. No weather/tariff files needed for the bundled
archetypes — the service bundles the CIBSE London weather.

**On Windows (learned this session):** kill a stale server with `Get-Process hem-server |
Stop-Process -Force` before rebuilding (a running exe locks the binary); pipe curl → python via
stdin rather than writing to `/tmp` (Git-Bash `/tmp` ≠ Windows python paths); axum error bodies can
be plain text, not JSON. GitHub ops (PRs/merges/CI status) go via the REST API + cached git
credential — `gh` is not installed.
