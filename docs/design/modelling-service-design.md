# HEM Modelling Service — Design Document

**Status:** Draft for review
**Date:** 2026-07-07
**Author:** Mark Ambler (with Claude Code)
**Scope owner:** Patent Glazing

---

## 1. Purpose

Define the scope, architecture, and delivery plan for a **web service / API** built on top of
the existing Rust Home Energy Model (HEM) engine in this repository. The service lets non-expert
users run HEM simulations by supplying a small set of building and glazing parameters, rather than
authoring the full ~8,000-line HEM input JSON by hand.

This document is the foundational scope artifact. It is meant to be argued with. Where a decision is
provisional or rests on an assumption, that is called out explicitly with a confidence level.

## 2. Background & context

- **HEM** is the UK Department for Energy Security and Net Zero (DESNZ) methodology for calculating
  the energy performance of buildings, expected in time to replace SAP/RdSAP. This repository is a
  Rust port of the Python candidate specification, forked from `communitiesuk/epb-home-energy-model`.
- **This fork already achieves 1:1 behavioural parity** with the Python reference across all 111 core
  demo scenarios (`cargo test --test e2e` = 0 diffs / 4.99M fields), verified on Windows and Linux CI.
- **The engine is already partly a service.** `hem-lambda` is a working `lambda_http` handler that
  takes a JSON request body, runs the calculation, and returns output or a structured `422` error.
- **Licensing (MIT, Crown Copyright / MHCLG)** explicitly permits use, modification, distribution,
  sublicensing, **and sale**. There is no licensing blocker to a commercial product.

### 2.1 Material constraints (read before committing)

These are the real constraints on what is honest to build *now*. They are not blockers, but they
define the product as a **design / research / forecasting tool**, not a compliance tool, today.

| # | Constraint | Confidence | Implication |
|---|-----------|-----------|-------------|
| C1 | HEM is **not the statutory method**. The statutory method for EPCs / Part L remains SAP 10.2 / RdSAP. | High | The service must not present outputs as official EPC/compliance figures today. Compliance output is a *deferred* goal (Phase 4), gated on HEM becoming statutory. |
| C2 | The HEM input schema is **explicitly unstable/undocumented** (engine version `1.0.0-alpha-7`). | High | Input-construction logic will churn with upstream. Isolate it; template from known-good baselines rather than hand-building. |
| C3 | Parity is with the **Python candidate spec**, itself not final. | High | "Correct" means "matches the current candidate", not "matches the final statutory model". Track upstream. |
| C4 | This fork carries **local engine patches** (three parity fixes). | High | Keep the engine crate as close to upstream as possible so DESNZ methodology updates remain rebaseable. Build product code *outside* the engine crate. |

## 3. Goals & non-goals

### Goals
- **G1** Run a HEM simulation from a compact, user-facing parameter set (archetype + glazing spec).
- **G2** Serve three use cases: (a) product design / R&D, (b) sales / customer-facing, (c) general
  research / bulk scenario runs.
- **G3** Return **structured, machine-readable JSON** results suitable for UIs and integration.
- **G4** Support **A vs B comparison** (baseline dwelling vs glazing-upgraded dwelling) with deltas.
- **G5** Keep the engine crate rebaseable on upstream (see C4) and track the HEM version in every result.

### Non-goals (this phase)
- **N1** Producing statutory / compliance outputs (EPC, Part L). Deferred to Phase 4, gated on C1.
- **N2** A general HEM authoring UI covering the full schema. We expose a curated subset.
- **N3** Re-deriving or altering the HEM methodology. We consume the engine as-is.
- **N4** Multi-tenant SaaS hardening (billing, orgs). Considered in Phase 3, not designed here.

## 4. Users & use cases

| Use case | User | What they supply | What they get | Priority |
|----------|------|------------------|---------------|----------|
| UC1 Design / R&D | Internal engineer | Archetype + full glazing spec overrides | Full structured results + heat balance | P0 |
| UC2 Glazing comparison | Sales / internal | Archetype + "before" and "after" glazing spec | Baseline vs upgrade deltas (energy, cost proxy, comfort) | P0 (Phase 2) |
| UC3 Customer-facing | End customer (via a front-end we do not design here) | Minimal: dwelling type + product choice | Simplified headline results | P1 |
| UC4 Bulk research | Analyst | Set of scenarios (matrix of params) | Tabulated results across runs | P1 |
| UC5 Compliance | — | — | — | **Deferred (C1)** |

## 5. Architecture

### 5.1 Principle
Keep `home-energy-model` (the engine) a **thin fork of upstream**. All product logic lives in **new
workspace crates that depend on it**. This directly serves G5/C4: the more product code we bolt onto
the engine, the harder each upstream rebase becomes, and rebaseability is exactly what the
"compliant-as-the-standard-evolves" future goal requires.

### 5.2 Crate layout (Cargo workspace)

```
home-energy-model      (engine — existing; keep minimal-diff vs upstream)
hem-lambda             (existing HTTP transport for AWS Lambda)
hem-profiles   (NEW)   archetype templates + glazing parameter → schema mapping
hem-api        (NEW)   request/response contracts, input assembly, structured output,
                       weather resolution; framework-agnostic core logic
hem-server     (NEW)   thin Axum binary exposing hem-api over HTTP (local + container)
```

`hem-api` holds the logic; `hem-server` and `hem-lambda` are two thin transports over the same
`hem-api` core. This avoids duplicating orchestration between local and cloud deployment.

### 5.3 Request flow

```
HTTP request (compact JSON: archetype id + glazing overrides + location)
  │
  ▼  hem-api
1. Load archetype baseline Input (from hem-profiles; a validated known-good template)
2. Apply glazing overrides   (typed patch → BuildingElementTransparent fields)
3. Resolve weather           (location → EPW / ExternalConditions)
4. Validate against core schema (engine's CORE_SCHEMA_VALIDATOR)
5. run_project(...)          (engine)
6. Shape Output → structured JSON response (from results_summary + selected series)
  │
  ▼
HTTP response (structured JSON) | 422 with structured errors
```

Steps 4–5 already exist in the engine (`run_project_from_input_file` validates then runs). Steps 1–3
and 6 are the new work.

## 6. Key design decisions

### 6.1 Input strategy — template + typed patch (the crux)

**Decision:** Never build HEM JSON from scratch for a user. Start from a validated **archetype
baseline** and apply a small **typed override patch**. Rationale: the schema is 8,019 lines / ~20
sections / 13 required (C2); the 111 demo inputs are known-good starting points; the glazing use
case only needs to vary a handful of fields.

**Glazing override → engine fields** (verified against `BuildingElementTransparent` in
`schemas/core-input.schema.json` and demo inputs):

| Product-facing knob | Engine field(s) | Notes |
|---------------------|-----------------|-------|
| Window U-value | `u_value` *or* `thermal_resistance_construction` | Schema allows either; archetype uses one. U↔R conversion convention **needs confirmation from the HEM methodology doc** (confidence: low on the exact convention; high that both fields exist). |
| Solar factor (g-value) | `g_value` | e.g. demo = 0.71 |
| Frame fraction | `frame_area_fraction` | |
| Geometry | `height`, `width`, `base_height`, `mid_height`, `orientation360`, `pitch` | Usually fixed by archetype; overridable for design work |
| Openable area / ventilation | `max_window_open_area`, `free_area_height`, `window_part_list[].mid_height_air_flow_path` | Affects natural-ventilation airflow |
| Shading / treatment | `shading[]`, `treatment` | Blinds/curtains/overhangs (Phase 2+) |

**Open decision (D1):** which fields are *user-editable* vs *archetype-fixed* per use case. UC1 exposes
all; UC3 exposes very few. Needs your input (see §10).

### 6.2 Output — structured JSON

**Decision:** Return machine-readable JSON, not the current concatenated CSV-as-text. The engine
already produces a `core__results_summary` file and a full `output` JSON; the `HemResponse` type
(`src/lib.rs`) exists to serialize an arbitrary payload but `hem-lambda` does not yet use it.

- **Phase 1** response = the summary results (annual/aggregate figures) as JSON.
- **Later** optionally include selected per-timestep series and heat-balance data behind a flag,
  mirroring the engine's existing `heat_balance` / `detailed_output_heating_cooling` switches.

**Open decision (D2):** exact result fields to surface for each use case. Depends on the summary
file's contents (to be enumerated in Phase 1).

### 6.3 Weather resolution

**Decision:** Replace the Lambda's single hardcoded `weather.epw` with a location→EPW lookup.
- **Phase 1:** a fixed set of bundled EPW files selectable by name (e.g. the CIBSE London file
  already in `examples/weather_data`).
- **Later:** postcode/region → EPW mapping.

**Open decision (D3):** which weather regions/files to support at launch.

### 6.4 Versioning & reproducibility

Every response embeds the **engine version** and **archetype/template id + version** so a result is
reproducible and, later, migratable when the methodology changes (G5). Cheap now, essential for C1/C3.

## 7. API surface (provisional)

| Method | Path | Purpose | Phase |
|--------|------|---------|-------|
| `POST` | `/simulate` | Run one scenario (archetype + overrides + weather) → structured results | 1 |
| `GET` | `/archetypes` | List available archetype templates | 1 |
| `GET` | `/healthz` | Liveness | 1 |
| `POST` | `/compare` | Baseline vs upgraded glazing → deltas | 2 |
| `GET` | `/weather` | List supported weather locations | 2 |
| `POST` | `/simulate/batch` | Run a matrix of scenarios | 3 |

### 7.1 `POST /simulate` request (illustrative — not final)

```json
{
  "archetype": "detached_1990s",
  "weather": "london_cibse",
  "glazing_overrides": {
    "u_value": 1.2,
    "g_value": 0.63,
    "frame_area_fraction": 0.1
  },
  "options": { "heat_balance": false, "detailed": false }
}
```

Applies the overrides to **all** transparent elements by default. **Open decision (D4):** whether
overrides target all windows, named windows, or windows by orientation.

## 8. Delivery plan

| Phase | Deliverable | Serves | Est. size | Exit criterion |
|-------|-------------|--------|-----------|----------------|
| **1 MVP** | `hem-profiles` (1 archetype) + `hem-api` + `hem-server` Axum `/simulate`, `/archetypes`, `/healthz`; structured JSON summary out; bundled weather | UC1 | Small (days) | A `POST /simulate` with glazing overrides returns correct structured results matching a direct engine run on the patched input |
| **2 Comparison** | `/compare` (baseline vs upgrade, deltas); more archetypes; weather-by-location; shading/treatment overrides | UC2, UC3 | Medium | Deltas verified against two independent engine runs |
| **3 Deploy & scale** | Containerize `hem-server` and/or extend `hem-lambda` onto `hem-api`; auth; batch endpoint; scenario storage | UC4 | Medium | Deployed, authenticated, load-tested |
| **4 Compliance (deferred)** | Compliance-shaped outputs; wrapper alignment | UC5 | Unknown | **Gated on HEM becoming statutory (C1)** — do not start until then |

## 9. Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Upstream schema churn breaks templates (C2) | High | Medium | Isolate in `hem-profiles`; validate templates in CI against the engine's schema; pin engine version per template |
| Users mistake outputs for compliance figures (C1) | Medium | High | Explicit non-compliance labelling in responses/UI contract; version stamping |
| Fork drifts from upstream, rebasing parity fixes gets costly (C4) | Medium | Medium | Keep product code out of the engine crate; periodically rebase; keep parity e2e as the CI gate |
| U-value↔thermal-resistance convention wrong | Medium | High (silent wrong numbers) | Confirm against HEM methodology doc before Phase 1 ships; add a round-trip test |
| Long-running sims block HTTP | Low (single-dwelling is fast) | Medium | Async/job model only if measured runtime warrants it (defer) |

## 10. Decisions needed from you

- **D1** Per use case, which glazing/building fields are user-editable vs archetype-fixed?
- **D2** Which result figures matter most for sales/design (energy demand, running-cost proxy,
  overheating/comfort, carbon)? Drives §6.2.
- **D3** Which weather locations to support at launch?
- **D4** Do overrides apply to all windows, named windows, or by orientation?
- **D5** First archetype to build (e.g. the existing `demo.json` detached dwelling, or a specific
  Patent Glazing target case)?
- **D6** Confirm Phase 4 (compliance) stays out of scope until HEM is statutory.

## 11. Success criteria (Phase 1)

1. `POST /simulate` with a glazing override produces results **bit-identical** to running the engine
   directly on the equivalently-patched input (proves the patch layer is faithful).
2. Invalid input returns a structured `422` (reuse the engine's schema errors).
3. The engine crate has **zero new product code** — all additions are in new crates (G5/C4).
4. e2e parity suite still green (no engine regression).

## 12. Appendix — grounding references (verified 2026-07-07)

- Engine entry points: `run_project_from_input_file`, `run_project` (`src/lib.rs`).
- Existing HTTP transport: `hem-lambda/src/main.rs` (bundles one fixed `weather.epw`; hardcodes
  no-tariff / no-heat-balance).
- Input schema: `schemas/core-input.schema.json` (8,019 lines; 13 required top-level sections).
- Glazing element: `BuildingElementTransparent` (fields enumerated in §6.1).
- Output files: `core__results`, `core__results_static`, `core__results_summary`,
  `core__results_heat_balance_*`, `core__results_heat_source_wet*` (`write_core_output_files`, `src/lib.rs`).
- Demo inputs: 111 files under `examples/input/core/`.
- Licence: MIT / Crown Copyright (`LICENSE.md`).
