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
[`docs/design/modelling-service-design.md`](design/modelling-service-design.md). API reference (endpoints,
request/response shapes, overrides, economics): [`docs/modelling-service-api.md`](modelling-service-api.md).

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
- Live check: `flat_nat_vent`, as-built vs U=0.8/g=0.5 → space-heat demand 1435→861 kWh (~40% cut),
  cost −£150.08, carbon −101.7 kgCO₂e over the simulated period (4380 h — NOT a full year), correct
  direction. Targeting: all-4 windows U=0.8 → −735.1 kWh; 2 living-room windows only → −363.3 kWh
  (~half, as expected); orientation `[90]` → all 4 (all face east). Shading: as-built 1435 kWh,
  shading removed 1188 kWh (more solar → less heat), deep overhang 1775 kWh (less solar → more heat),
  monotonic and correct. Treatment: closed manual curtains all windows → +175.8 kWh heat (block
  winter sun). 28 unit tests pass (`cargo test -p hem-api -p hem-profiles`).

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
1. ~~Cost & carbon figures in the `/compare` delta (not just kWh).~~ **DONE**.
2. ~~Per-window / by-orientation targeting (D4).~~ **DONE**.
3. ~~Shading/`treatment` overrides (blinds/curtains/overhangs — design doc §6.1).~~ **DONE**
   (treatment limited to control-free/fixed-`is_open` until an archetype carries `$.Control`
   scaffolding, or we add control injection).
4. Illustrative archetype — **PARTIAL / DONE within constraints.** Added `flat_new_build_uk`: the
   `flat_nat_vent` envelope with glazing at the current UK new-build standard (whole-window U=1.4
   W/m²K, Approved Document L 2021 England, effective 15 Jun 2023; g=0.63 modern low-e double). A
   curated **fabric preset** (design doc G1 — non-experts pick by name), clearly labelled
   illustrative / not a surveyed dwelling / not compliance. Baseline heat 1244.8 kWh (vs 1435.2 for
   `flat_nat_vent`). Verified it runs and gives sensible, correct-direction figures.
   **Why only a preset, not a new dwelling:** surveyed all 111 core demos (2026-07-17) — none run a
   full year (max 7296 h) and the long-period ones are engine *test fixtures* (23–48 kWh space-heat
   over ~300 days, several with PV), *less* realistic than `flat_nat_vent`. The engine does NOT cycle
   schedules (probed: "Schedule length is less than the expected length"), so extending any short
   demo's period means fabricating a full period of internal-gains/cold-water/control schedules.
   Only `flat_nat_vent` has a validated long period, so any archetype must reuse its machinery;
   safe edits are scalar fabric params only (geometry/topology surgery risks silently-wrong physics
   with no oracle). A structurally-different (detached / full-year) dwelling needs **real dwelling
   data (design doc D5)** or accepting fabricated schedules. NB: the survey also revealed multi-fuel
   cost/carbon was untested — added a synthetic dual-fuel unit test to close that gap.
5. Weather-by-location — **mechanism DONE; more locations blocked on data.** Added a `weather`
   request field (defaults to `london_cibse`), `GET /weather`, id→conditions resolution, unknown→404,
   and the id echoed in responses. Only London is listed: the bundled CIBSE and EPW files are the
   *same* London year (identical temps/wind/diffuse radiation — verified), so exposing both would be
   a false choice. Adding a genuinely different location is now a one-line registry entry once a
   real regional weather file is sourced (provenance/licensing = user decision).
6. Pluggable engine-backend trait in `hem-api` (local vs ECaaS) — deferred until ECaaS is concrete
   (its API shape is unknown, so abstracting against it now would be guesswork).

## Known issues
- **Cost/carbon assume no on-site generation.** The cost/carbon base is delivered energy = *gross
  consumption per fuel*, which equals metered grid import only when generation/export are zero (true
  for both current archetypes). A PV/generation archetype would need the base switched to
  `energy_supply` net import + an export credit. Verified boundary, not a current bug.
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
