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
use serde_json::Value;
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
}

impl GlazingOverrides {
    fn is_empty(&self) -> bool {
        self.u_value.is_none()
            && self.thermal_resistance_construction.is_none()
            && self.g_value.is_none()
            && self.frame_area_fraction.is_none()
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

/// Engine output-detail switches, mirroring the engine's own flags.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SimulateOptions {
    #[serde(default)]
    pub heat_balance: bool,
    #[serde(default)]
    pub detailed_output_heating_cooling: bool,
}

/// A request to run one scenario.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SimulateRequest {
    /// Archetype id (see `hem_profiles::list`).
    pub archetype: String,
    #[serde(default)]
    pub glazing_overrides: GlazingOverrides,
    #[serde(default)]
    pub options: SimulateOptions,
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
    /// How many transparent building elements the overrides were applied to.
    pub transparent_elements_modified: usize,
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

/// Apply glazing overrides in place to every `BuildingElementTransparent` in the input, returning
/// the number of elements modified. Pure and independently testable — the faithfulness guarantee
/// (design doc Success Criterion 1) rests on this being the *only* transformation applied.
pub fn apply_glazing_overrides(input: &mut Value, overrides: &GlazingOverrides) -> usize {
    if overrides.is_empty() {
        return 0;
    }
    let mut modified = 0;

    let Some(zones) = input.get_mut("Zone").and_then(Value::as_object_mut) else {
        return 0;
    };
    for zone in zones.values_mut() {
        let Some(elements) = zone
            .get_mut("BuildingElement")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for element in elements.values_mut() {
            let is_transparent = element.get("type").and_then(Value::as_str)
                == Some("BuildingElementTransparent");
            if !is_transparent {
                continue;
            }
            let Some(obj) = element.as_object_mut() else {
                continue;
            };
            // u_value and thermal_resistance_construction are mutually exclusive on the element:
            // setting one must remove the other so the input stays valid (see UValueInput).
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
            modified += 1;
        }
    }
    modified
}

/// Assemble the final HEM input for a request: load the archetype baseline and patch it.
/// Returns the patched JSON and the count of transparent elements touched.
pub fn assemble_input(req: &SimulateRequest) -> Result<(Value, usize), ApiError> {
    req.glazing_overrides.validate()?;
    let mut input = hem_profiles::baseline(&req.archetype)
        .ok_or_else(|| ApiError::UnknownArchetype(req.archetype.clone()))?;
    let modified = apply_glazing_overrides(&mut input, &req.glazing_overrides);
    Ok((input, modified))
}

/// Run one scenario end to end.
pub fn simulate(req: &SimulateRequest) -> Result<SimulateResponse, ApiError> {
    let (input, modified) = assemble_input(req)?;

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

    Ok(SimulateResponse {
        archetype: req.archetype.clone(),
        api_version: API_VERSION,
        transparent_elements_modified: modified,
        summary: result.output.summary,
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
    /// The starting glazing spec. Empty = the archetype's as-built windows.
    #[serde(default)]
    pub baseline_overrides: GlazingOverrides,
    /// The proposed/upgraded glazing spec.
    pub upgrade_overrides: GlazingOverrides,
    #[serde(default)]
    pub options: SimulateOptions,
}

/// One side of a comparison.
#[derive(Debug, Serialize)]
pub struct Scenario {
    pub overrides: GlazingOverrides,
    pub summary: OutputSummary,
}

/// Headline reductions (baseline − upgrade), in kWh. Positive = the upgrade uses less energy.
#[derive(Debug, Serialize)]
pub struct ComparisonDelta {
    pub space_heat_demand_reduction: f64,
    pub space_cool_demand_reduction: f64,
    /// `null` if either summary lacked a delivered-energy total.
    pub delivered_energy_reduction: Option<f64>,
}

/// The result of a baseline-vs-upgrade comparison.
#[derive(Debug, Serialize)]
pub struct CompareResponse {
    pub archetype: String,
    pub api_version: &'static str,
    pub transparent_elements_modified: usize,
    pub baseline: Scenario,
    pub upgrade: Scenario,
    pub delta: ComparisonDelta,
}

/// Run the archetype twice — once with the baseline glazing, once with the upgrade — and report
/// both summaries plus the headline reductions. This is the core design/sales artifact: "what does
/// switching to this glazing spec do to the dwelling's energy demand?"
pub fn compare(req: &CompareRequest) -> Result<CompareResponse, ApiError> {
    let baseline = simulate(&SimulateRequest {
        archetype: req.archetype.clone(),
        glazing_overrides: req.baseline_overrides.clone(),
        options: req.options.clone(),
    })?;
    let upgrade = simulate(&SimulateRequest {
        archetype: req.archetype.clone(),
        glazing_overrides: req.upgrade_overrides.clone(),
        options: req.options.clone(),
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
    };

    Ok(CompareResponse {
        archetype: req.archetype.clone(),
        api_version: API_VERSION,
        transparent_elements_modified: upgrade.transparent_elements_modified,
        baseline: Scenario {
            overrides: req.baseline_overrides.clone(),
            summary: baseline.summary,
        },
        upgrade: Scenario {
            overrides: req.upgrade_overrides.clone(),
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
            options: SimulateOptions::default(),
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
            options: Default::default(),
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
            options: Default::default(),
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
            upgrade_overrides: GlazingOverrides {
                u_value: Some(0.8),
                ..Default::default()
            },
            options: Default::default(),
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
}
