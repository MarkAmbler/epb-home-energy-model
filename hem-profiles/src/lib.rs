//! Curated HEM dwelling archetypes.
//!
//! An archetype is a *known-good* full HEM input JSON that the modelling service uses as a
//! baseline. Callers apply a small typed override patch (see the `hem-api` crate) rather than
//! authoring the ~8,000-line schema by hand. Keeping archetypes here — separate from the engine
//! crate and from the `examples/` test fixtures — makes them a curated product artifact whose
//! shape we control, and keeps the engine crate rebaseable on upstream (design doc §5.1).

use serde::Serialize;
use serde_json::Value;

/// Summary metadata for an archetype, suitable for a `GET /archetypes` listing.
#[derive(Debug, Clone, Serialize)]
pub struct ArchetypeInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

/// The minimal single-zone demonstration dwelling (two windows), taken verbatim from the engine's
/// `examples/input/core/short/demo.json`. Only 8 timesteps — a fast fixture for tests, NOT a
/// source of physically meaningful figures.
const DETACHED_DEMO_JSON: &str = include_str!("../archetypes/detached_demo.json");

/// A realistic full-period archetype: a naturally-ventilated deck-access flat with window opening
/// for cooling (from `examples/input/core/long/SAP11_deck_flat_nat_vent_with_window_opening_for_cooling.json`).
/// Two zones, four windows, ~full year — exercises both fabric heat loss (U-value) and solar
/// gain / window-opening ventilation (g-value), which is what makes it useful for glazing studies.
const FLAT_NAT_VENT_JSON: &str = include_str!("../archetypes/flat_nat_vent.json");

/// The [`FLAT_NAT_VENT_JSON`] envelope reparametrised to the current UK new-build glazing standard
/// (whole-window U = 1.4 W/m²K, Approved Document L 2021 England, effective 15 Jun 2023; g = 0.63
/// for representative modern low-e double glazing). The opaque fabric is unchanged — it already
/// meets current Part L limiting U-values. **Illustrative**: a curated fabric preset so a non-expert
/// can pick "current new-build glazing" by name, NOT a surveyed dwelling and NOT a compliance run.
const FLAT_NEW_BUILD_UK_JSON: &str = include_str!("../archetypes/flat_new_build_uk.json");

const ARCHETYPES: &[ArchetypeInfo] = &[
    ArchetypeInfo {
        id: "flat_nat_vent",
        name: "Naturally-ventilated flat",
        description: "Deck-access flat, natural ventilation with window opening for cooling; 2 zones, 4 windows, full-period. Realistic archetype for glazing studies.",
    },
    ArchetypeInfo {
        id: "flat_new_build_uk",
        name: "Flat, current UK new-build glazing (illustrative)",
        description: "The nat-vent flat envelope with glazing at the current UK new-build standard (U=1.4 W/m²K, Approved Document L 2021; g=0.63 modern low-e double glazing). Opaque fabric already meets current Part L. Illustrative preset — not a surveyed dwelling, not a compliance calculation.",
    },
    ArchetypeInfo {
        id: "detached_demo",
        name: "Detached demo dwelling",
        description: "Minimal single-zone demo, two windows, 8 timesteps only. Fast fixture — figures are NOT physically meaningful.",
    },
];

/// List all available archetypes.
pub fn list() -> &'static [ArchetypeInfo] {
    ARCHETYPES
}

/// Whether an archetype id is known.
pub fn exists(id: &str) -> bool {
    ARCHETYPES.iter().any(|a| a.id == id)
}

/// Return the raw baseline HEM input for an archetype, parsed as a JSON value ready to patch.
///
/// Returns `None` for an unknown id. Panics only if a bundled archetype is itself malformed JSON,
/// which the `archetypes_are_valid_json` test rules out at build time.
pub fn baseline(id: &str) -> Option<Value> {
    let raw = match id {
        "flat_nat_vent" => FLAT_NAT_VENT_JSON,
        "flat_new_build_uk" => FLAT_NEW_BUILD_UK_JSON,
        "detached_demo" => DETACHED_DEMO_JSON,
        _ => return None,
    };
    Some(serde_json::from_str(raw).expect("bundled archetype must be valid JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archetypes_are_valid_json_with_zones() {
        for info in list() {
            let baseline = baseline(info.id).expect("listed archetype must have a baseline");
            let obj = baseline.as_object().expect("archetype root must be a JSON object");
            assert!(obj.contains_key("Zone"), "{} must contain a Zone section", info.id);
        }
    }

    #[test]
    fn unknown_archetype_is_none() {
        assert!(baseline("does_not_exist").is_none());
        assert!(!exists("does_not_exist"));
    }

    /// The current-UK new-build preset must carry the documented glazing spec on every window:
    /// U=1.4 W/m²K (with the mutually-exclusive resistance removed) and g=0.63.
    #[test]
    fn new_build_uk_has_current_glazing_on_all_windows() {
        let b = baseline("flat_new_build_uk").expect("archetype exists");
        let mut windows = 0;
        for zone in b["Zone"].as_object().unwrap().values() {
            for el in zone["BuildingElement"].as_object().unwrap().values() {
                if el["type"] == "BuildingElementTransparent" {
                    windows += 1;
                    assert_eq!(el["u_value"], serde_json::json!(1.4), "window must be at U=1.4");
                    assert!(
                        el.get("thermal_resistance_construction").is_none(),
                        "the mutually-exclusive resistance key must be absent"
                    );
                    assert_eq!(el["g_value"], serde_json::json!(0.63));
                }
            }
        }
        assert_eq!(windows, 4, "the flat envelope has four windows");
    }
}
