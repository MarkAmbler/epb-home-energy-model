//! Modelling-service core logic (transport-agnostic).
//!
//! Pipeline (design doc §5.3): load an archetype baseline → apply a typed glazing override patch →
//! resolve weather → run the HEM engine → return structured results. This crate holds the logic;
//! `hem-server` (Axum) and `hem-lambda` are thin transports over it. It adds **no** code to the
//! engine crate, keeping the fork rebaseable on upstream (design doc §5.1 / Success Criterion 3).

use home_energy_model::errors::HemError;
use home_energy_model::output::OutputSummary;
use home_energy_model::output_writer::SinkOutputWriter;
use home_energy_model::read_weather_file::cibse_weather_data_to_external_conditions;
use home_energy_model::{run_project_from_input_file, RunInput};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Cursor};

/// Bundled Phase 1 weather. The engine's e2e parity harness runs every core demo against exactly
/// this file, so using it here means the archetype is exercised the same way it is validated
/// (design doc §6.3). Location-selectable weather is a Phase 2 concern.
const LONDON_CIBSE_WEATHER: &str =
    include_str!("../../examples/weather_data/London_weather_CIBSE_format.csv");

/// Version of this API crate, surfaced in responses for reproducibility (design doc §6.4).
pub const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Glazing parameters exposed to callers. Every field is optional; only those set are applied.
///
/// All fields are **direct passthroughs** to `BuildingElementTransparent`. The engine accepts a
/// window's thermal performance as *either* a `u_value` *or* a `thermal_resistance_construction`,
/// but not both (see the engine's `UValueInput`). `u_value` is what a glazing product datasheet
/// gives, so it is the primary knob; setting it swaps out whichever form the archetype used. It is
/// an error to set both (see [`GlazingOverrides::validate`]).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GlazingOverrides {
    /// Whole-window thermal transmittance U (W/m²·K) — the value from a glazing datasheet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub u_value: Option<f64>,
    /// Thermal resistance of the glazing construction (m²·K/W). Mutually exclusive with `u_value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal_resistance_construction: Option<f64>,
    /// Solar factor (g-value), 0..1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub g_value: Option<f64>,
    /// Fraction of the opening occupied by frame, 0..1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_area_fraction: Option<f64>,
    /// Replace the window's external shading list — overhangs, side fins, reveals, and obstacles
    /// (design doc §6.1). `Some(list)` replaces the element's entire `shading` array; `Some([])`
    /// clears all shading; `None` leaves it unchanged. Entries are passed through verbatim and
    /// validated by the engine's core schema, so a malformed entry surfaces as a 422 — this
    /// deliberately avoids re-encoding the unstable input schema (constraint C2). Shape:
    /// `{"type":"overhang"|"sidefinleft"|"sidefinright"|"reveal","depth":m,"distance":m}` or
    /// `{"type":"obstacle","height":m,"distance":m,"transparency":0..1}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shading: Option<Vec<Value>>,
    /// Replace the window's internal treatment list — blinds/curtains (design doc §6.1). `Some(list)`
    /// replaces the element's `treatment`; `Some([])` removes all treatments; `None` leaves it
    /// unchanged. Passed through verbatim and schema-validated (malformed ⇒ 422). Minimal entry:
    /// `{"type":"blinds"|"curtains","controls":"manual"|"auto_motorised"|"manual_motorised"|
    /// "combined_light_blind_HVAC","delta_r":m²K/W,"trans_red":0..1,"is_open":bool}`. NOTE: the
    /// `Control_open`/`Control_opening_irrad`/`Control_closing_irrad` fields reference keys in the
    /// archetype's `$.Control`; the current archetypes have none, so only control-free (fixed
    /// `is_open`) treatments are usable today — a reference to a missing control will fail the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treatment: Option<Vec<Value>>,
}

impl GlazingOverrides {
    fn is_empty(&self) -> bool {
        self.u_value.is_none()
            && self.thermal_resistance_construction.is_none()
            && self.g_value.is_none()
            && self.frame_area_fraction.is_none()
            && self.shading.is_none()
            && self.treatment.is_none()
    }

    /// Reject combinations the engine cannot represent. `u_value` and
    /// `thermal_resistance_construction` are mutually exclusive on a transparent element.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.u_value.is_some() && self.thermal_resistance_construction.is_some() {
            return Err(ApiError::InvalidInput(
                "u_value and thermal_resistance_construction are mutually exclusive; set only one"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Selects which transparent elements a [`TargetedOverride`] applies to. An element matches when it
/// satisfies **every non-empty** criterion (AND); an all-empty selector matches every window.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WindowSelector {
    /// BuildingElement names to match, exact (e.g. `"living window W03"`). Empty ⇒ not filtered by name.
    #[serde(default)]
    pub names: Vec<String>,
    /// `orientation360` values to match (degrees). Empty ⇒ not filtered by orientation.
    #[serde(default)]
    pub orientations: Vec<f64>,
}

impl WindowSelector {
    /// True if a window with this `name`/`orientation` satisfies every non-empty criterion.
    fn matches(&self, name: &str, orientation: Option<f64>) -> bool {
        let name_ok = self.names.is_empty() || self.names.iter().any(|n| n == name);
        let orientation_ok = self.orientations.is_empty()
            || orientation
                .is_some_and(|o| self.orientations.iter().any(|t| (t - o).abs() < 1e-9));
        name_ok && orientation_ok
    }
}

/// Apply `overrides` only to the windows picked by `select`. Rules are applied after the global
/// [`SimulateRequest::glazing_overrides`], in order, and later rules win per field — so a global
/// upgrade can be refined for specific windows (e.g. "all windows to U=1.0, but the north face to
/// U=0.8").
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TargetedOverride {
    pub select: WindowSelector,
    pub overrides: GlazingOverrides,
}

/// One transparent element in the assembled dwelling, so a caller knows what names/orientations
/// exist to target with a [`WindowSelector`].
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub zone: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation360: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f64>,
}

/// Engine output-detail switches, mirroring the engine's own flags.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SimulateOptions {
    #[serde(default)]
    pub heat_balance: bool,
    #[serde(default)]
    pub detailed_output_heating_cooling: bool,
}

/// Price and carbon intensity for one fuel type, applied to delivered (metered) energy.
///
/// `price_gbp_per_kwh` is the **unit rate only** — it excludes fixed standing charges, which are
/// per-day, do not depend on the glazing spec, and so cancel in a baseline-vs-upgrade comparison.
/// Factors are applied to the delivered energy over the *simulated period*, which may be shorter
/// than a year (e.g. `flat_nat_vent` simulates 4380 hours), so the resulting cost/carbon are NOT
/// annual figures unless the archetype simulates a full year.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FuelFactors {
    /// Unit price of delivered energy, GBP per kWh. Excludes standing charge.
    pub price_gbp_per_kwh: f64,
    /// Carbon intensity of delivered energy, kgCO₂e per kWh.
    pub carbon_kg_per_kwh: f64,
}

/// Price and carbon factors per fuel type, used to turn delivered energy (kWh) into running cost (£)
/// and carbon (kgCO₂e). Keys are the engine's fuel-type names (snake_case, e.g. `"electricity"`,
/// `"mains_gas"`). Caller-supplied; [`Economics::uk_defaults`] provides a documented default set.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Economics {
    /// Provenance of these factors, echoed in responses so a result is self-documenting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Factors keyed by engine fuel-type name.
    pub fuels: BTreeMap<String, FuelFactors>,
}

impl Economics {
    /// Illustrative UK defaults, used when a request omits `economics`. **Not authoritative** — a
    /// design/sales running-cost/carbon proxy the caller is expected to override with their own
    /// tariff and factors. Values verified 2026-07-17:
    /// - Unit prices: Ofgem energy price cap, GB national average, direct-debit incl. VAT,
    ///   1 Jul–30 Sep 2026 — electricity 26.11 p/kWh, mains gas 7.33 p/kWh.
    /// - Carbon factors: UK DESNZ/DEFRA 2025 GHG conversion factors — grid electricity (generation)
    ///   0.177 kgCO₂e/kWh; natural gas 0.18296 kgCO₂e/kWh (gross CV).
    pub fn uk_defaults() -> Self {
        let mut fuels = BTreeMap::new();
        fuels.insert(
            "electricity".to_string(),
            FuelFactors {
                price_gbp_per_kwh: 0.2611,
                carbon_kg_per_kwh: 0.177,
            },
        );
        fuels.insert(
            "mains_gas".to_string(),
            FuelFactors {
                price_gbp_per_kwh: 0.0733,
                carbon_kg_per_kwh: 0.18296,
            },
        );
        Economics {
            source: Some(
                "Illustrative defaults — Ofgem price cap (GB avg, direct debit incl. VAT, \
                 1 Jul–30 Sep 2026) and DESNZ/DEFRA 2025 GHG conversion factors. \
                 Not authoritative; override with your own tariff/factors."
                    .to_string(),
            ),
            fuels,
        }
    }
}

/// Running cost and carbon for one scenario, over the simulated period (see [`FuelFactors`]).
#[derive(Debug, Clone, Serialize)]
pub struct CostCarbon {
    /// Running (unit-rate) cost of delivered energy, GBP. Excludes standing charges.
    pub cost_gbp: f64,
    /// Carbon of delivered energy, kgCO₂e.
    pub carbon_kg: f64,
}

/// A request to run one scenario.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SimulateRequest {
    /// Archetype id (see `hem_profiles::list`).
    pub archetype: String,
    /// Applied to every transparent element (the default). Refine per-window with `targeted_overrides`.
    #[serde(default)]
    pub glazing_overrides: GlazingOverrides,
    /// Per-window overrides applied after `glazing_overrides`; later rules win per field.
    #[serde(default)]
    pub targeted_overrides: Vec<TargetedOverride>,
    #[serde(default)]
    pub options: SimulateOptions,
    /// Price/carbon factors for the cost & carbon figures. Omitted ⇒ [`Economics::uk_defaults`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economics: Option<Economics>,
}

/// A structured simulation result.
///
/// Phase 1 returns the engine's `summary` (annual/aggregate figures — total space heat/cool demand,
/// per-fuel energy supply and delivered energy, peak electricity, hot-water percentile). The full
/// per-timestep `Output` is deliberately NOT returned: it contains maps keyed by `Option<name>`
/// whose `None` key is not a valid JSON object key, so it cannot serialize as-is. Surfacing selected
/// per-timestep series is a later decision (design doc §6.2 / D2).
#[derive(Debug, Serialize)]
pub struct SimulateResponse {
    pub archetype: String,
    pub api_version: &'static str,
    /// How many distinct transparent elements received at least one override (global or targeted).
    pub transparent_elements_modified: usize,
    /// Every transparent element in the dwelling, so callers know what to target (see [`WindowInfo`]).
    pub windows: Vec<WindowInfo>,
    /// Fuel type of each energy supply in the dwelling (supply name → fuel-type string).
    pub energy_supply_fuels: BTreeMap<String, String>,
    /// The price/carbon factors used to derive [`SimulateResponse::cost_carbon`].
    pub economics_used: Economics,
    /// Running cost and carbon over the simulated period.
    pub cost_carbon: CostCarbon,
    pub summary: OutputSummary,
}

/// Errors from the modelling service, classified so a transport can map to the right status.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Unknown archetype id — a client error (404/400).
    #[error("unknown archetype: '{0}'")]
    UnknownArchetype(String),
    /// The bundled weather data failed to parse — a server/config error (500).
    #[error("failed to load weather data: {0}")]
    Weather(String),
    /// The engine rejected the assembled input — a client error (422).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The engine failed or panicked during calculation — a server error (500).
    #[error("calculation failed: {0}")]
    Calculation(String),
}

impl ApiError {
    /// True if this is the caller's fault (4xx), false if it is ours (5xx).
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            ApiError::UnknownArchetype(_) | ApiError::InvalidInput(_)
        )
    }
}

impl From<HemError> for ApiError {
    fn from(err: HemError) -> Self {
        match err {
            // The only client-attributable engine error: the assembled JSON was rejected.
            HemError::InvalidRequest(_) | HemError::NotImplemented(_) => {
                ApiError::InvalidInput(err.to_string())
            }
            _ => ApiError::Calculation(err.to_string()),
        }
    }
}

/// Set the fields of one set of overrides on a single transparent element, returning whether any
/// field was written. `u_value` and `thermal_resistance_construction` are mutually exclusive on the
/// element, so setting one removes the other to keep the input valid (see the engine's `UValueInput`).
fn apply_overrides_to_element(obj: &mut Map<String, Value>, overrides: &GlazingOverrides) -> bool {
    if overrides.is_empty() {
        return false;
    }
    if let Some(u) = overrides.u_value {
        obj.insert("u_value".into(), Value::from(u));
        obj.remove("thermal_resistance_construction");
    } else if let Some(r) = overrides.thermal_resistance_construction {
        obj.insert("thermal_resistance_construction".into(), Value::from(r));
        obj.remove("u_value");
    }
    if let Some(g) = overrides.g_value {
        obj.insert("g_value".into(), Value::from(g));
    }
    if let Some(f) = overrides.frame_area_fraction {
        obj.insert("frame_area_fraction".into(), Value::from(f));
    }
    if let Some(shading) = &overrides.shading {
        obj.insert("shading".into(), Value::Array(shading.clone()));
    }
    if let Some(treatment) = &overrides.treatment {
        obj.insert("treatment".into(), Value::Array(treatment.clone()));
    }
    true
}

/// Apply the `global` overrides to every `BuildingElementTransparent`, then each `targeted` rule in
/// order (later rules win per field). Returns the number of *distinct* transparent elements that
/// received at least one override. Pure and independently testable — the faithfulness guarantee
/// (design doc Success Criterion 1) rests on this being the *only* transformation applied.
pub fn apply_all_overrides(
    input: &mut Value,
    global: &GlazingOverrides,
    targeted: &[TargetedOverride],
) -> usize {
    let mut modified: BTreeSet<(String, String)> = BTreeSet::new();
    let Some(zones) = input.get_mut("Zone").and_then(Value::as_object_mut) else {
        return 0;
    };
    for (zone_name, zone) in zones.iter_mut() {
        let Some(elements) = zone
            .get_mut("BuildingElement")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for (element_name, element) in elements.iter_mut() {
            let is_transparent = element.get("type").and_then(Value::as_str)
                == Some("BuildingElementTransparent");
            if !is_transparent {
                continue;
            }
            // Copy out before the mutable borrow; f64 is Copy so this doesn't hold the borrow.
            let orientation = element.get("orientation360").and_then(Value::as_f64);
            let Some(obj) = element.as_object_mut() else {
                continue;
            };
            let mut touched = apply_overrides_to_element(obj, global);
            for rule in targeted {
                if rule.select.matches(element_name, orientation) {
                    touched |= apply_overrides_to_element(obj, &rule.overrides);
                }
            }
            if touched {
                modified.insert((zone_name.clone(), element_name.clone()));
            }
        }
    }
    modified.len()
}

/// Backward-compatible helper: apply a single override set to every transparent element.
pub fn apply_glazing_overrides(input: &mut Value, overrides: &GlazingOverrides) -> usize {
    apply_all_overrides(input, overrides, &[])
}

/// List every transparent element in the input, for the response's window inventory.
fn window_inventory(input: &Value) -> Vec<WindowInfo> {
    let mut windows = Vec::new();
    if let Some(zones) = input.get("Zone").and_then(Value::as_object) {
        for (zone_name, zone) in zones {
            let Some(elements) = zone.get("BuildingElement").and_then(Value::as_object) else {
                continue;
            };
            for (element_name, element) in elements {
                if element.get("type").and_then(Value::as_str) != Some("BuildingElementTransparent") {
                    continue;
                }
                windows.push(WindowInfo {
                    zone: zone_name.clone(),
                    name: element_name.clone(),
                    orientation360: element.get("orientation360").and_then(Value::as_f64),
                    pitch: element.get("pitch").and_then(Value::as_f64),
                });
            }
        }
    }
    windows
}

/// Assemble the final HEM input for a request: load the archetype baseline and patch it (global
/// overrides, then targeted). Returns the patched JSON and the count of transparent elements touched.
pub fn assemble_input(req: &SimulateRequest) -> Result<(Value, usize), ApiError> {
    req.glazing_overrides.validate()?;
    for rule in &req.targeted_overrides {
        rule.overrides.validate()?;
    }
    let mut input = hem_profiles::baseline(&req.archetype)
        .ok_or_else(|| ApiError::UnknownArchetype(req.archetype.clone()))?;
    let modified = apply_all_overrides(&mut input, &req.glazing_overrides, &req.targeted_overrides);
    Ok((input, modified))
}

/// Map each `EnergySupply` in the input to its fuel-type string (snake_case, as the engine
/// serialises `FuelType`). The delivered-energy summary is keyed by supply *name*, not fuel type,
/// so this map is needed to apply per-fuel economics.
fn energy_supply_fuels(input: &Value) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(supplies) = input.get("EnergySupply").and_then(Value::as_object) {
        for (name, details) in supplies {
            if let Some(fuel) = details.get("fuel").and_then(Value::as_str) {
                map.insert(name.clone(), fuel.to_string());
            }
        }
    }
    map
}

/// Compute running cost and carbon for a scenario from its delivered-energy summary.
///
/// Iterates the real per-supply rows (skipping the `"total"` aggregate), reads each supply's
/// delivered-energy total (kWh), maps the supply to its fuel type, and applies that fuel's factors.
/// Fails (422) if the input uses a fuel for which no factors were supplied — better to reject the
/// request than to silently price part of the energy at zero and return a wrong headline figure.
///
/// Base quantity: delivered energy is *gross consumption per fuel*, which equals metered grid import
/// only when the dwelling has **no on-site generation** (both current archetypes do — generation and
/// export are zero). With PV or other generation, consumption exceeds net import, so this would
/// overstate cost; a generation-aware archetype must switch the base to `energy_supply` net import
/// plus an export credit. Tracked as a known limitation until such an archetype exists.
fn cost_carbon(
    summary: &OutputSummary,
    supply_fuels: &BTreeMap<String, String>,
    econ: &Economics,
) -> Result<CostCarbon, ApiError> {
    let mut cost_gbp = 0.0;
    let mut carbon_kg = 0.0;
    for (supply, end_uses) in &summary.delivered_energy {
        let supply = supply.as_ref();
        if supply == "total" {
            // The aggregate pseudo-row, not a real fuel supply — summing the real rows reproduces it.
            continue;
        }
        // A real, metered fuel is one declared in the input's EnergySupply. Anything else is an
        // engine-internal pseudo-supply (e.g. "_energy_from_environment", "_unmet_demand") that
        // carries no meter, so it has no running cost or carbon — skip it rather than erroring.
        let Some(fuel) = supply_fuels.get(supply).map(String::as_str) else {
            continue;
        };
        // Free ambient energy harvested by a heat pump, and unmet demand, are not metered fuels
        // even if declared, so they never contribute cost or carbon.
        if fuel == "energy_from_environment" || fuel == "unmet_demand" {
            continue;
        }
        let kwh = end_uses.get("total").copied().unwrap_or(0.0);
        let factors = econ.fuels.get(fuel).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "no economics factors supplied for fuel type '{fuel}' (energy supply '{supply}')"
            ))
        })?;
        cost_gbp += kwh * factors.price_gbp_per_kwh;
        carbon_kg += kwh * factors.carbon_kg_per_kwh;
    }
    Ok(CostCarbon {
        cost_gbp,
        carbon_kg,
    })
}

/// Run one scenario end to end.
pub fn simulate(req: &SimulateRequest) -> Result<SimulateResponse, ApiError> {
    let (input, modified) = assemble_input(req)?;
    // Capture the supply→fuel map and window inventory before `input` is moved into the engine run.
    let energy_supply_fuels = energy_supply_fuels(&input);
    let windows = window_inventory(&input);
    let economics_used = req.economics.clone().unwrap_or_else(Economics::uk_defaults);

    let weather = cibse_weather_data_to_external_conditions(BufReader::new(Cursor::new(
        LONDON_CIBSE_WEATHER,
    )))
    .map_err(|e| ApiError::Weather(format!("{e:?}")))?;

    // SinkOutputWriter is the engine's no-op writer: we take results from the returned
    // CalculationResult in memory rather than writing CSV/JSON files. output_formats = None.
    let result = run_project_from_input_file(
        RunInput::Json(input),
        &SinkOutputWriter,
        Some(weather),
        None,
        None,
        req.options.heat_balance,
        req.options.detailed_output_heating_cooling,
    )?;

    let summary = result.output.summary;
    let cost_carbon = cost_carbon(&summary, &energy_supply_fuels, &economics_used)?;

    Ok(SimulateResponse {
        archetype: req.archetype.clone(),
        api_version: API_VERSION,
        transparent_elements_modified: modified,
        windows,
        energy_supply_fuels,
        economics_used,
        cost_carbon,
        summary,
    })
}

/// Total delivered energy across all fuels and end-uses (kWh), read from the summary's
/// `delivered_energy["total"]["total"]` cell. Returns `None` if absent.
fn delivered_energy_total(summary: &OutputSummary) -> Option<f64> {
    summary
        .delivered_energy
        .get("total")
        .and_then(|by_use| by_use.get("total"))
        .copied()
}

/// A request to compare a baseline glazing spec against an upgraded one on the same archetype.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompareRequest {
    pub archetype: String,
    /// The starting glazing spec (global). Empty = the archetype's as-built windows.
    #[serde(default)]
    pub baseline_overrides: GlazingOverrides,
    /// Per-window refinements to the baseline side.
    #[serde(default)]
    pub baseline_targeted: Vec<TargetedOverride>,
    /// The proposed/upgraded glazing spec (global). May be empty when the upgrade is expressed
    /// entirely through `upgrade_targeted` (upgrading only specific windows).
    #[serde(default)]
    pub upgrade_overrides: GlazingOverrides,
    /// Per-window refinements to the upgrade side (e.g. upgrade only the north-facing windows).
    #[serde(default)]
    pub upgrade_targeted: Vec<TargetedOverride>,
    #[serde(default)]
    pub options: SimulateOptions,
    /// Price/carbon factors, applied identically to both sides. Omitted ⇒ [`Economics::uk_defaults`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economics: Option<Economics>,
}

/// One side of a comparison.
#[derive(Debug, Serialize)]
pub struct Scenario {
    pub overrides: GlazingOverrides,
    /// Running cost and carbon for this side, over the simulated period.
    pub cost_carbon: CostCarbon,
    pub summary: OutputSummary,
}

/// Headline reductions (baseline − upgrade). Positive = the upgrade is better (less energy, lower
/// cost, less carbon). Energy figures are kWh, cost is GBP, carbon is kgCO₂e, all over the
/// simulated period.
#[derive(Debug, Serialize)]
pub struct ComparisonDelta {
    pub space_heat_demand_reduction: f64,
    pub space_cool_demand_reduction: f64,
    /// `null` if either summary lacked a delivered-energy total.
    pub delivered_energy_reduction: Option<f64>,
    /// Running-cost saving (GBP), using `economics_used`. Excludes standing charges.
    pub cost_gbp_reduction: f64,
    /// Carbon saving (kgCO₂e), using `economics_used`.
    pub carbon_kg_reduction: f64,
}

/// The result of a baseline-vs-upgrade comparison.
#[derive(Debug, Serialize)]
pub struct CompareResponse {
    pub archetype: String,
    pub api_version: &'static str,
    pub transparent_elements_modified: usize,
    /// Every transparent element in the dwelling (identical for both sides), for targeting.
    pub windows: Vec<WindowInfo>,
    /// Fuel type of each energy supply (identical for both sides — glazing overrides don't touch it).
    pub energy_supply_fuels: BTreeMap<String, String>,
    /// The price/carbon factors used for both sides and the delta.
    pub economics_used: Economics,
    pub baseline: Scenario,
    pub upgrade: Scenario,
    pub delta: ComparisonDelta,
}

/// Run the archetype twice — once with the baseline glazing, once with the upgrade — and report
/// both summaries plus the headline reductions. This is the core design/sales artifact: "what does
/// switching to this glazing spec do to the dwelling's energy demand?"
pub fn compare(req: &CompareRequest) -> Result<CompareResponse, ApiError> {
    // Resolve economics once so both sides — and thus the delta — use identical factors.
    let economics = req.economics.clone().unwrap_or_else(Economics::uk_defaults);

    let baseline = simulate(&SimulateRequest {
        archetype: req.archetype.clone(),
        glazing_overrides: req.baseline_overrides.clone(),
        targeted_overrides: req.baseline_targeted.clone(),
        options: req.options.clone(),
        economics: Some(economics.clone()),
    })?;
    let upgrade = simulate(&SimulateRequest {
        archetype: req.archetype.clone(),
        glazing_overrides: req.upgrade_overrides.clone(),
        targeted_overrides: req.upgrade_targeted.clone(),
        options: req.options.clone(),
        economics: Some(economics.clone()),
    })?;

    let delta = ComparisonDelta {
        space_heat_demand_reduction: baseline.summary.space_heat_demand_total
            - upgrade.summary.space_heat_demand_total,
        space_cool_demand_reduction: baseline.summary.space_cool_demand_total
            - upgrade.summary.space_cool_demand_total,
        delivered_energy_reduction: match (
            delivered_energy_total(&baseline.summary),
            delivered_energy_total(&upgrade.summary),
        ) {
            (Some(b), Some(u)) => Some(b - u),
            _ => None,
        },
        cost_gbp_reduction: baseline.cost_carbon.cost_gbp - upgrade.cost_carbon.cost_gbp,
        carbon_kg_reduction: baseline.cost_carbon.carbon_kg - upgrade.cost_carbon.carbon_kg,
    };

    Ok(CompareResponse {
        archetype: req.archetype.clone(),
        api_version: API_VERSION,
        transparent_elements_modified: upgrade.transparent_elements_modified,
        windows: upgrade.windows.clone(),
        energy_supply_fuels: upgrade.energy_supply_fuels.clone(),
        economics_used: economics,
        baseline: Scenario {
            overrides: req.baseline_overrides.clone(),
            cost_carbon: baseline.cost_carbon,
            summary: baseline.summary,
        },
        upgrade: Scenario {
            overrides: req.upgrade_overrides.clone(),
            cost_carbon: upgrade.cost_carbon,
            summary: upgrade.summary,
        },
        delta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_request(overrides: GlazingOverrides) -> SimulateRequest {
        SimulateRequest {
            archetype: "detached_demo".into(),
            glazing_overrides: overrides,
            targeted_overrides: Vec::new(),
            options: SimulateOptions::default(),
            economics: None,
        }
    }

    #[test]
    fn overrides_touch_all_transparent_elements_only() {
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let overrides = GlazingOverrides {
            g_value: Some(0.5),
            ..Default::default()
        };
        let n = apply_glazing_overrides(&mut input, &overrides);
        // The demo archetype has two windows.
        assert_eq!(n, 2);

        // Every transparent element now has g_value 0.5; no opaque element gained a g_value.
        let zones = input["Zone"].as_object().unwrap();
        for zone in zones.values() {
            for el in zone["BuildingElement"].as_object().unwrap().values() {
                if el["type"] == "BuildingElementTransparent" {
                    assert_eq!(el["g_value"], serde_json::json!(0.5));
                } else {
                    assert!(el.get("g_value").is_none());
                }
            }
        }
    }

    #[test]
    fn empty_overrides_modify_nothing() {
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let before = input.clone();
        let n = apply_glazing_overrides(&mut input, &GlazingOverrides::default());
        assert_eq!(n, 0);
        assert_eq!(input, before, "no-op overrides must not mutate the input");
    }

    #[test]
    fn unknown_archetype_is_client_error() {
        let e = assemble_input(&SimulateRequest {
            archetype: "nope".into(),
            glazing_overrides: Default::default(),
            targeted_overrides: Vec::new(),
            options: Default::default(),
            economics: None,
        })
        .unwrap_err();
        assert!(e.is_client_error());
        assert!(matches!(e, ApiError::UnknownArchetype(_)));
    }

    fn run_engine_summary(input: Value) -> OutputSummary {
        let weather = cibse_weather_data_to_external_conditions(BufReader::new(Cursor::new(
            LONDON_CIBSE_WEATHER,
        )))
        .unwrap();
        run_project_from_input_file(
            RunInput::Json(input),
            &SinkOutputWriter,
            Some(weather),
            None,
            None,
            false,
            false,
        )
        .expect("direct run")
        .output
        .summary
    }

    /// Success Criterion 1: the API's patch layer must produce an input — and thus engine results —
    /// identical to an independent, manual patch of the same fields. This is what guarantees the
    /// service adds nothing to the calculation beyond the documented overrides.
    #[test]
    fn simulate_matches_direct_engine_run_on_manually_patched_input() {
        let overrides = GlazingOverrides {
            g_value: Some(0.42),
            frame_area_fraction: Some(0.15),
            thermal_resistance_construction: Some(0.30),
            ..Default::default()
        };

        // Path A: assemble via the API's patch layer.
        let (api_input, modified) = assemble_input(&demo_request(overrides)).unwrap();
        assert_eq!(modified, 2);

        // Path B: manually patch the same baseline the "obvious" way.
        let mut manual = hem_profiles::baseline("detached_demo").unwrap();
        {
            let zones = manual["Zone"].as_object_mut().unwrap();
            for zone in zones.values_mut() {
                let els = zone["BuildingElement"].as_object_mut().unwrap();
                for el in els.values_mut() {
                    if el["type"] == "BuildingElementTransparent" {
                        let o = el.as_object_mut().unwrap();
                        o.insert("g_value".into(), serde_json::json!(0.42));
                        o.insert("frame_area_fraction".into(), serde_json::json!(0.15));
                        o.insert(
                            "thermal_resistance_construction".into(),
                            serde_json::json!(0.30),
                        );
                    }
                }
            }
        }

        // The assembled inputs must be equal (order-insensitive: serde_json Map is an IndexMap
        // whose PartialEq ignores order under the preserve_order feature).
        assert_eq!(api_input, manual, "patch layer must match a manual patch of the same fields");

        // And the engine summaries from each must be identical. OutputSummary is fully string-keyed,
        // so to_value succeeds; IndexMap PartialEq makes the comparison order-insensitive.
        let sum_api = serde_json::to_value(run_engine_summary(api_input)).unwrap();
        let sum_manual = serde_json::to_value(run_engine_summary(manual)).unwrap();
        assert_eq!(
            sum_api, sum_manual,
            "API-assembled input must yield the same engine summary as a manual patch"
        );
    }

    /// The response payload must actually serialize to JSON (the raw `Output` does not — see the
    /// `SimulateResponse` docs). Guards against a regression that reintroduces a non-serializable
    /// field on the response.
    #[test]
    fn simulate_response_serializes_to_json() {
        let resp = simulate(&demo_request(GlazingOverrides {
            g_value: Some(0.5),
            ..Default::default()
        }))
        .expect("simulate");
        let json = serde_json::to_string(&resp).expect("response must serialize to JSON");
        assert!(json.contains("\"summary\""));
        assert!(json.contains("\"transparent_elements_modified\":2"));
    }

    #[test]
    fn u_value_override_replaces_thermal_resistance() {
        // The detached_demo windows are specified via thermal_resistance_construction. Supplying a
        // u_value must swap them: u_value present, thermal_resistance_construction gone.
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let n = apply_glazing_overrides(
            &mut input,
            &GlazingOverrides {
                u_value: Some(1.2),
                ..Default::default()
            },
        );
        assert_eq!(n, 2);
        for zone in input["Zone"].as_object().unwrap().values() {
            for el in zone["BuildingElement"].as_object().unwrap().values() {
                if el["type"] == "BuildingElementTransparent" {
                    assert_eq!(el["u_value"], serde_json::json!(1.2));
                    assert!(
                        el.get("thermal_resistance_construction").is_none(),
                        "the mutually-exclusive resistance key must be removed"
                    );
                }
            }
        }
    }

    #[test]
    fn setting_both_u_value_and_resistance_is_rejected() {
        let err = assemble_input(&SimulateRequest {
            archetype: "detached_demo".into(),
            glazing_overrides: GlazingOverrides {
                u_value: Some(1.2),
                thermal_resistance_construction: Some(0.3),
                ..Default::default()
            },
            targeted_overrides: Vec::new(),
            options: Default::default(),
            economics: None,
        })
        .unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)));
        assert!(err.is_client_error());
    }

    #[test]
    fn compare_runs_and_computes_deltas() {
        // Baseline = as-built; upgrade = a much better U-value. The comparison must run, touch both
        // windows, and yield finite deltas with a delivered-energy figure. (Physical direction is
        // asserted against the realistic archetype in the live smoke test, not this 8-step demo.)
        let resp = compare(&CompareRequest {
            archetype: "detached_demo".into(),
            baseline_overrides: GlazingOverrides::default(),
            baseline_targeted: Vec::new(),
            upgrade_overrides: GlazingOverrides {
                u_value: Some(0.8),
                ..Default::default()
            },
            upgrade_targeted: Vec::new(),
            options: Default::default(),
            economics: None,
        })
        .expect("compare");

        assert_eq!(resp.transparent_elements_modified, 2);
        assert!(resp.delta.space_heat_demand_reduction.is_finite());
        assert!(resp.delta.delivered_energy_reduction.is_some());
        // The upgrade changed the glazing, so the two summaries must differ.
        assert_ne!(
            resp.baseline.summary.space_heat_demand_total,
            resp.upgrade.summary.space_heat_demand_total,
            "changing the window U-value must change space-heat demand"
        );
        // The response must serialize (it embeds two OutputSummary values).
        serde_json::to_string(&resp).expect("compare response serializes");
    }

    /// Cost and carbon must equal delivered energy × the supplied factors. Both archetypes are
    /// all-electric, so the whole-dwelling figures reduce to `delivered_energy_total × factor`.
    #[test]
    fn cost_and_carbon_are_delivered_energy_times_factors() {
        let econ = Economics {
            source: None,
            fuels: BTreeMap::from([(
                "electricity".to_string(),
                FuelFactors {
                    price_gbp_per_kwh: 0.30,
                    carbon_kg_per_kwh: 0.20,
                },
            )]),
        };
        let resp = simulate(&SimulateRequest {
            archetype: "detached_demo".into(),
            glazing_overrides: GlazingOverrides::default(),
            targeted_overrides: Vec::new(),
            options: Default::default(),
            economics: Some(econ),
        })
        .expect("simulate");

        assert_eq!(resp.energy_supply_fuels.get("mains elec").map(String::as_str), Some("electricity"));
        let kwh = delivered_energy_total(&resp.summary).expect("delivered energy total");
        assert!(kwh > 0.0, "the demo must consume some electricity");
        assert!((resp.cost_carbon.cost_gbp - kwh * 0.30).abs() < 1e-9);
        assert!((resp.cost_carbon.carbon_kg - kwh * 0.20).abs() < 1e-9);
    }

    /// Omitting `economics` applies the documented UK defaults and echoes them back with provenance.
    #[test]
    fn omitted_economics_uses_documented_uk_defaults() {
        let resp = simulate(&demo_request(GlazingOverrides::default())).expect("simulate");
        assert!(resp.economics_used.source.is_some(), "defaults must carry provenance");
        assert!(resp.economics_used.fuels.contains_key("electricity"));
        assert!(resp.cost_carbon.cost_gbp > 0.0);
        assert!(resp.cost_carbon.carbon_kg > 0.0);
    }

    /// A dwelling whose fuel has no supplied factors is a client error (422), not a silent zero.
    #[test]
    fn missing_fuel_factors_is_client_error() {
        let econ = Economics {
            source: None,
            fuels: BTreeMap::from([(
                // Not the fuel the demo uses (electricity).
                "mains_gas".to_string(),
                FuelFactors {
                    price_gbp_per_kwh: 0.07,
                    carbon_kg_per_kwh: 0.18,
                },
            )]),
        };
        let err = simulate(&SimulateRequest {
            archetype: "detached_demo".into(),
            glazing_overrides: GlazingOverrides::default(),
            targeted_overrides: Vec::new(),
            options: Default::default(),
            economics: Some(econ),
        })
        .unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)));
        assert!(err.is_client_error());
    }

    /// The cost/carbon reductions must be finite and move in the same direction as the energy
    /// reduction (a single-fuel dwelling, so less delivered energy ⇒ lower cost and carbon).
    #[test]
    fn compare_reports_consistent_cost_and_carbon_reductions() {
        let resp = compare(&CompareRequest {
            archetype: "detached_demo".into(),
            baseline_overrides: GlazingOverrides::default(),
            baseline_targeted: Vec::new(),
            upgrade_overrides: GlazingOverrides {
                u_value: Some(0.8),
                ..Default::default()
            },
            upgrade_targeted: Vec::new(),
            options: Default::default(),
            economics: None,
        })
        .expect("compare");

        let energy = resp.delta.delivered_energy_reduction.expect("delivered energy reduction");
        assert!(resp.delta.cost_gbp_reduction.is_finite());
        assert!(resp.delta.carbon_kg_reduction.is_finite());
        assert_eq!(
            resp.delta.cost_gbp_reduction.signum(),
            energy.signum(),
            "cost saving must track the delivered-energy saving for a single-fuel dwelling"
        );
        assert_eq!(resp.delta.carbon_kg_reduction.signum(), energy.signum());
    }

    // ---- per-window / by-orientation targeting (D4) ----

    fn g_of(input: &Value, name: &str) -> Option<f64> {
        input["Zone"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|z| z["BuildingElement"].as_object().unwrap())
            .find(|(n, _)| n.as_str() == name)
            .and_then(|(_, e)| e.get("g_value").and_then(Value::as_f64))
    }

    #[test]
    fn selector_matches_use_and_semantics() {
        let by_name = WindowSelector {
            names: vec!["window 0".into()],
            orientations: vec![],
        };
        assert!(by_name.matches("window 0", Some(180.0)));
        assert!(!by_name.matches("window 1", Some(180.0)));

        let by_orientation = WindowSelector {
            names: vec![],
            orientations: vec![180.0],
        };
        assert!(by_orientation.matches("anything", Some(180.0)));
        assert!(!by_orientation.matches("anything", Some(90.0)));
        assert!(!by_orientation.matches("anything", None));

        // Both criteria present ⇒ AND: the name matches but the orientation does not.
        let both = WindowSelector {
            names: vec!["window 0".into()],
            orientations: vec![90.0],
        };
        assert!(!both.matches("window 0", Some(180.0)));

        // Empty selector matches everything.
        assert!(WindowSelector::default().matches("window 0", Some(180.0)));
    }

    #[test]
    fn targeted_override_hits_only_the_named_window() {
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let n = apply_all_overrides(
            &mut input,
            &GlazingOverrides::default(), // no global change
            &[TargetedOverride {
                select: WindowSelector {
                    names: vec!["window 0".into()],
                    orientations: vec![],
                },
                overrides: GlazingOverrides {
                    g_value: Some(0.3),
                    ..Default::default()
                },
            }],
        );
        assert_eq!(n, 1, "only the one named window is modified");
        assert_eq!(g_of(&input, "window 0"), Some(0.3));
        assert_eq!(g_of(&input, "window 1"), Some(0.71), "the other window is untouched");
    }

    #[test]
    fn targeted_override_wins_over_global_per_field() {
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let n = apply_all_overrides(
            &mut input,
            &GlazingOverrides {
                g_value: Some(0.5), // global: all windows to 0.5
                ..Default::default()
            },
            &[TargetedOverride {
                select: WindowSelector {
                    names: vec!["window 0".into()],
                    orientations: vec![],
                },
                overrides: GlazingOverrides {
                    g_value: Some(0.3), // then refine window 0 to 0.3
                    ..Default::default()
                },
            }],
        );
        assert_eq!(n, 2, "the global override touches both windows");
        assert_eq!(g_of(&input, "window 0"), Some(0.3), "targeted rule wins");
        assert_eq!(g_of(&input, "window 1"), Some(0.5), "global rule applies elsewhere");
    }

    #[test]
    fn targeting_by_orientation_selects_matching_windows() {
        // Both flat_nat_vent windows selected by their shared orientation; a non-matching
        // orientation selects none.
        let mut input = hem_profiles::baseline("flat_nat_vent").unwrap();
        let n = apply_all_overrides(
            &mut input,
            &GlazingOverrides::default(),
            &[TargetedOverride {
                select: WindowSelector {
                    names: vec![],
                    orientations: vec![90.0],
                },
                overrides: GlazingOverrides {
                    u_value: Some(0.8),
                    ..Default::default()
                },
            }],
        );
        assert_eq!(n, 4, "all four east-facing windows match");

        let mut input2 = hem_profiles::baseline("flat_nat_vent").unwrap();
        let none = apply_all_overrides(
            &mut input2,
            &GlazingOverrides::default(),
            &[TargetedOverride {
                select: WindowSelector {
                    names: vec![],
                    orientations: vec![270.0],
                },
                overrides: GlazingOverrides {
                    u_value: Some(0.8),
                    ..Default::default()
                },
            }],
        );
        assert_eq!(none, 0, "no window faces 270°");
    }

    #[test]
    fn global_and_targeted_keep_u_and_resistance_mutually_exclusive() {
        // Global sets u_value (removing the archetype's resistance); a targeted rule then sets a
        // resistance on one window (removing that window's u_value). Each window ends with exactly one.
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        apply_all_overrides(
            &mut input,
            &GlazingOverrides {
                u_value: Some(1.0),
                ..Default::default()
            },
            &[TargetedOverride {
                select: WindowSelector {
                    names: vec!["window 0".into()],
                    orientations: vec![],
                },
                overrides: GlazingOverrides {
                    thermal_resistance_construction: Some(0.2),
                    ..Default::default()
                },
            }],
        );
        let el = |name: &str| {
            input["Zone"]
                .as_object()
                .unwrap()
                .values()
                .flat_map(|z| z["BuildingElement"].as_object().unwrap())
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, e)| e.clone())
                .unwrap()
        };
        let w0 = el("window 0");
        assert_eq!(w0.get("thermal_resistance_construction").and_then(Value::as_f64), Some(0.2));
        assert!(w0.get("u_value").is_none(), "targeted resistance removes u_value on that window");
        let w1 = el("window 1");
        assert_eq!(w1.get("u_value").and_then(Value::as_f64), Some(1.0));
        assert!(w1.get("thermal_resistance_construction").is_none(), "global u_value removes resistance");
    }

    #[test]
    fn window_inventory_lists_every_transparent_element() {
        let input = hem_profiles::baseline("flat_nat_vent").unwrap();
        let windows = window_inventory(&input);
        assert_eq!(windows.len(), 4);
        let names: Vec<&str> = windows.iter().map(|w| w.name.as_str()).collect();
        assert!(names.contains(&"living window W03"));
        assert!(windows.iter().all(|w| w.orientation360 == Some(90.0)));
        assert!(windows.iter().all(|w| w.pitch == Some(90.0)));
    }

    /// A compare request that upgrades only specific windows (via `upgrade_targeted`, with no global
    /// `upgrade_overrides`) must deserialize — the global upgrade fields are optional.
    #[test]
    fn compare_request_deserializes_with_only_targeted_upgrade() {
        let json = r#"{
            "archetype": "flat_nat_vent",
            "upgrade_targeted": [
                { "select": { "names": ["living window W03"] }, "overrides": { "u_value": 0.8 } }
            ]
        }"#;
        let req: CompareRequest = serde_json::from_str(json).expect("must parse without upgrade_overrides");
        assert!(req.upgrade_overrides.is_empty());
        assert_eq!(req.upgrade_targeted.len(), 1);
        assert_eq!(req.upgrade_targeted[0].select.names, vec!["living window W03"]);
    }

    // ---- shading overrides (design doc §6.1) ----

    fn shading_of(input: &Value, name: &str) -> Value {
        input["Zone"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|z| z["BuildingElement"].as_object().unwrap())
            .find(|(n, _)| n.as_str() == name)
            .and_then(|(_, e)| e.get("shading"))
            .cloned()
            .unwrap()
    }

    #[test]
    fn shading_override_replaces_the_array() {
        // detached_demo windows start with an empty shading list.
        let overhang = serde_json::json!({"type": "overhang", "depth": 0.6, "distance": 0.3});
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let n = apply_glazing_overrides(
            &mut input,
            &GlazingOverrides {
                shading: Some(vec![overhang.clone()]),
                ..Default::default()
            },
        );
        assert_eq!(n, 2);
        assert_eq!(shading_of(&input, "window 0"), Value::Array(vec![overhang]));
    }

    #[test]
    fn shading_can_be_cleared_and_targeted() {
        // flat_nat_vent windows carry an overhang + two side fins; clear it on one window only.
        let mut input = hem_profiles::baseline("flat_nat_vent").unwrap();
        assert!(shading_of(&input, "living window W03").as_array().unwrap().len() > 0);
        let n = apply_all_overrides(
            &mut input,
            &GlazingOverrides::default(),
            &[TargetedOverride {
                select: WindowSelector {
                    names: vec!["living window W03".into()],
                    orientations: vec![],
                },
                overrides: GlazingOverrides {
                    shading: Some(vec![]),
                    ..Default::default()
                },
            }],
        );
        assert_eq!(n, 1);
        assert_eq!(shading_of(&input, "living window W03"), serde_json::json!([]));
        // A sibling window is untouched.
        assert!(shading_of(&input, "living window W04").as_array().unwrap().len() > 0);
    }

    #[test]
    fn shading_only_override_is_not_empty() {
        let o = GlazingOverrides {
            shading: Some(vec![]),
            ..Default::default()
        };
        assert!(!o.is_empty(), "a shading-only override must count as a modification");
    }

    /// The engine's core schema must accept an override-supplied shading object and run to completion
    /// with it (proves the passthrough forms valid input). The physical effect on demand is verified
    /// live on the full-period `flat_nat_vent` archetype — the 8-step demo has negligible solar
    /// variation, so it is the wrong fixture for a direction assertion.
    #[test]
    fn shading_override_runs_through_the_engine() {
        let resp = simulate(&demo_request(GlazingOverrides {
            shading: Some(vec![serde_json::json!({"type": "overhang", "depth": 2.0, "distance": 0.05})]),
            ..Default::default()
        }))
        .expect("shaded run must be schema-valid and succeed");
        assert!(resp.summary.space_heat_demand_total.is_finite());
    }

    /// A malformed shading entry is rejected by the engine schema as a client error (422).
    #[test]
    fn malformed_shading_is_client_error() {
        let err = simulate(&demo_request(GlazingOverrides {
            // Missing required `depth`/`distance` for an overhang.
            shading: Some(vec![serde_json::json!({"type": "overhang"})]),
            ..Default::default()
        }))
        .unwrap_err();
        assert!(err.is_client_error(), "bad shading must be a 4xx, got {err:?}");
        assert!(matches!(err, ApiError::InvalidInput(_)));
    }

    // ---- treatment overrides (design doc §6.1) ----

    #[test]
    fn treatment_override_replaces_the_array() {
        let curtain = serde_json::json!({
            "type": "curtains", "controls": "manual",
            "delta_r": 0.1, "trans_red": 0.5, "is_open": false
        });
        let mut input = hem_profiles::baseline("detached_demo").unwrap();
        let n = apply_glazing_overrides(
            &mut input,
            &GlazingOverrides {
                treatment: Some(vec![curtain.clone()]),
                ..Default::default()
            },
        );
        assert_eq!(n, 2);
        let t = input["Zone"]["zone 1"]["BuildingElement"]["window 0"]["treatment"].clone();
        assert_eq!(t, Value::Array(vec![curtain]));
    }

    /// A control-free (fixed `is_open`) treatment must form schema-valid input and run to completion.
    /// (Physical effect is verified live on flat_nat_vent; the 8-step demo is too short to assert it.)
    #[test]
    fn control_free_treatment_runs_through_the_engine() {
        let resp = simulate(&demo_request(GlazingOverrides {
            treatment: Some(vec![serde_json::json!({
                "type": "blinds", "controls": "manual",
                "delta_r": 0.08, "trans_red": 0.4, "is_open": false
            })]),
            ..Default::default()
        }))
        .expect("control-free treatment must be schema-valid and run");
        assert!(resp.summary.space_heat_demand_total.is_finite());
    }

    #[test]
    fn malformed_treatment_is_client_error() {
        let err = simulate(&demo_request(GlazingOverrides {
            // Missing required delta_r/trans_red/controls.
            treatment: Some(vec![serde_json::json!({"type": "blinds"})]),
            ..Default::default()
        }))
        .unwrap_err();
        assert!(err.is_client_error(), "bad treatment must be a 4xx, got {err:?}");
        assert!(matches!(err, ApiError::InvalidInput(_)));
    }

    #[test]
    fn targeted_override_with_both_keys_is_rejected() {
        let err = assemble_input(&SimulateRequest {
            archetype: "detached_demo".into(),
            glazing_overrides: GlazingOverrides::default(),
            targeted_overrides: vec![TargetedOverride {
                select: WindowSelector::default(),
                overrides: GlazingOverrides {
                    u_value: Some(1.2),
                    thermal_resistance_construction: Some(0.3),
                    ..Default::default()
                },
            }],
            options: Default::default(),
            economics: None,
        })
        .unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)));
        assert!(err.is_client_error());
    }
}
