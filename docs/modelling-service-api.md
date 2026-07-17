# HEM Modelling Service — API reference

A small HTTP service over the HEM engine that runs simulations from a compact parameter set
(archetype + glazing overrides) instead of a hand-authored ~8,000-line HEM input. Design and scope:
[`design/modelling-service-design.md`](design/modelling-service-design.md); running status:
[`STATUS.md`](STATUS.md).

> **Not a compliance tool.** HEM is not the statutory method (SAP 10.x / RdSAP remain so). Outputs
> are for design, sales, and research — **not** EPC or Part L figures. Cost and carbon use
> illustrative default factors unless you supply your own (see [Economics](#economics)).

## Architecture

Three new workspace crates over the (unmodified) engine crate, so the engine stays rebaseable on
upstream:

- `hem-profiles` — curated archetype templates (known-good HEM inputs).
- `hem-api` — transport-agnostic core: input assembly, overrides, engine run, structured results.
- `hem-server` — thin Axum HTTP transport over `hem-api`.

## Running

```bash
HEM_SERVER_ADDR=127.0.0.1:8080 cargo run -p hem-server
```

`HEM_SERVER_ADDR` defaults to `127.0.0.1:8080`. No weather/tariff files are needed — the service
bundles the CIBSE London weather used by the engine's parity harness.

## Endpoints

| Method | Path          | Purpose |
|--------|---------------|---------|
| GET    | `/healthz`    | Liveness — returns `ok`. |
| GET    | `/archetypes` | List available dwelling archetypes. |
| POST   | `/simulate`   | Run one scenario → structured summary + cost/carbon. |
| POST   | `/compare`    | Baseline vs upgraded glazing → both summaries + headline reductions. |

### GET `/archetypes`

```json
{ "archetypes": [ { "id": "flat_nat_vent", "name": "...", "description": "..." }, ... ] }
```

| id | What it is |
|----|-----------|
| `flat_nat_vent` | Naturally-ventilated deck-access flat; 2 zones, 4 windows, 4380 h (~half year, heating season). Realistic for glazing studies. |
| `flat_new_build_uk` | The same envelope with glazing at the current UK new-build standard (U=1.4 W/m²K, Approved Document L 2021; g=0.63). **Illustrative preset**, not a surveyed dwelling. |
| `detached_demo` | Minimal single-zone demo, 8 timesteps. Fast test fixture — **figures are not physically meaningful.** |

### POST `/simulate`

Request (only `archetype` is required):

```json
{
  "archetype": "flat_nat_vent",
  "glazing_overrides": {
    "u_value": 1.2,
    "g_value": 0.5,
    "frame_area_fraction": 0.1,
    "shading": [ { "type": "overhang", "depth": 0.6, "distance": 0.3 } ],
    "treatment": [ { "type": "curtains", "controls": "manual", "delta_r": 0.1, "trans_red": 0.5, "is_open": false } ]
  },
  "targeted_overrides": [
    { "select": { "names": ["living window W03"] }, "overrides": { "u_value": 0.8 } }
  ],
  "options": { "heat_balance": false, "detailed_output_heating_cooling": false },
  "economics": null
}
```

Response (abridged):

```json
{
  "archetype": "flat_nat_vent",
  "api_version": "0.1.0",
  "transparent_elements_modified": 4,
  "windows": [ { "zone": "zone 1", "name": "living window W03", "orientation360": 90.0, "pitch": 90.0 }, ... ],
  "energy_supply_fuels": { "mains elec": "electricity" },
  "economics_used": { "source": "Illustrative defaults — ...", "fuels": { "electricity": { "price_gbp_per_kwh": 0.2611, "carbon_kg_per_kwh": 0.177 }, ... } },
  "cost_carbon": { "cost_gbp": 416.68, "carbon_kg": 282.47 },
  "summary": { "space_heat_demand_total": 1435.2, "space_cool_demand_total": 0.0, "delivered_energy": { ... }, ... }
}
```

### POST `/compare`

Runs the archetype twice with identical economics and reports the reductions (baseline − upgrade).
Both `*_overrides` are optional; an upgrade can be expressed purely via `upgrade_targeted`.

```json
{
  "archetype": "flat_nat_vent",
  "baseline_overrides": {},
  "baseline_targeted": [],
  "upgrade_overrides": { "u_value": 0.8, "g_value": 0.5 },
  "upgrade_targeted": []
}
```

```json
{
  "archetype": "flat_nat_vent",
  "transparent_elements_modified": 4,
  "windows": [ ... ],
  "energy_supply_fuels": { "mains elec": "electricity" },
  "economics_used": { ... },
  "baseline": { "overrides": {}, "cost_carbon": { ... }, "summary": { ... } },
  "upgrade":  { "overrides": { ... }, "cost_carbon": { ... }, "summary": { ... } },
  "delta": {
    "space_heat_demand_reduction": 574.8,
    "space_cool_demand_reduction": 0.0,
    "delivered_energy_reduction": 574.8,
    "cost_gbp_reduction": 150.08,
    "carbon_kg_reduction": 101.74
  }
}
```

Positive reductions = the upgrade is better (less energy/cost/carbon). Figures are over the
archetype's **simulated period**, which may be shorter than a year — they are **not** annualised.

## Glazing overrides

Applied to `BuildingElementTransparent` elements. All fields optional; only those set are applied.

| Field | Meaning |
|-------|---------|
| `u_value` | Whole-window U (W/m²·K), the datasheet value. |
| `thermal_resistance_construction` | Glazing construction resistance (m²·K/W). **Mutually exclusive with `u_value`** — setting one removes the other; setting *both* is a 422. |
| `g_value` | Solar factor (0–1). |
| `frame_area_fraction` | Fraction of the opening occupied by frame (0–1). |
| `shading` | Replaces the window's external shading list (overhang / sidefinleft / sidefinright / reveal / obstacle). `[]` clears it. |
| `treatment` | Replaces the window's internal treatment list (blinds / curtains). Only control-free (fixed `is_open`) treatments work with the current archetypes. |

`shading`/`treatment` entries pass through to the engine verbatim and are validated by its core
schema — a malformed entry returns a 422.

## Per-window targeting

`glazing_overrides` applies to **all** windows. `targeted_overrides` refine specific windows,
applied after the global overrides, with later rules winning per field. A `select` matches a window
when every non-empty criterion matches (AND); an empty selector matches all windows. Use the
`windows` inventory in any response to discover names and orientations.

```json
{ "select": { "names": ["bed window W02"], "orientations": [90] }, "overrides": { "u_value": 0.8 } }
```

## Economics

Cost and carbon come from per-fuel-type factors. Omit `economics` to use documented UK defaults
(echoed back in `economics_used` so a result is self-documenting). Supply your own to override:

```json
{ "economics": { "fuels": { "electricity": { "price_gbp_per_kwh": 0.27, "carbon_kg_per_kwh": 0.19 } } } }
```

- Keys are engine fuel-type names (`electricity`, `mains_gas`, …).
- Prices are **unit-rate only** — standing charges are excluded (they cancel in an A/B comparison).
- A dwelling using a fuel with no supplied factors is a 422 (never silently priced at zero).
- **Default factors** (illustrative, override for real work): electricity 26.11 p/kWh, mains gas
  7.33 p/kWh (Ofgem price cap, GB average, direct debit incl. VAT, 1 Jul–30 Sep 2026); carbon
  0.177 / 0.18296 kgCO₂e/kWh (DESNZ/DEFRA 2025 conversion factors).
- **Limitation:** cost is based on delivered (gross) consumption per fuel, which equals metered grid
  import only when there is no on-site generation. All current archetypes are generation-free; a PV
  archetype would need net-import + export-credit accounting.

## Errors

| Status | When |
|--------|------|
| 404 | Unknown archetype. |
| 422 | Invalid input — an **unknown/misspelt field** (requests reject unknown fields rather than silently ignoring them), mutually-exclusive glazing keys, a fuel with no factors, or a shape the engine's core schema rejects. |
| 500 | Weather/config failure, or an engine calculation error. |

Error body: `{ "error": "<message>" }`.

**Known limitation:** a *schema-valid* input that fails deeper in the calculation (e.g. a window
`treatment` that references a `$.Control` key the archetype doesn't define) surfaces as a **500**,
even though the client caused it — the engine does not cleanly separate "your input is bad" from
"we broke" once the input passes schema validation.

## Reproducibility

Every response carries `api_version` and the `economics_used` factors, so a result records the code
version and the exact price/carbon assumptions behind its figures.
