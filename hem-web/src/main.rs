//! Rust->WASM (Yew) frontend for the HEM modelling service.
//!
//! A single compare-focused screen: pick an archetype + weather, describe an upgraded glazing spec,
//! and see baseline-vs-upgrade space-heat demand, running cost, and carbon. It talks to `hem-server`
//! over the same origin (the server serves this bundle), so no CORS is involved and all fetch URLs
//! are relative (`/archetypes`, `/weather`, `/compare`).

use serde::{Deserialize, Serialize};
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, HtmlSelectElement};
use yew::prelude::*;

// ---------------------------------------------------------------------------
// Wire types. These mirror the fields of hem-api's responses that this screen
// renders. serde ignores unknown fields by default, so partial structs are safe
// and we don't have to track every field the server returns.
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Deserialize)]
struct NamedItem {
    id: String,
    name: String,
    description: String,
}

#[derive(Deserialize)]
struct ArchetypesResp {
    archetypes: Vec<NamedItem>,
}

#[derive(Deserialize)]
struct WeatherResp {
    weather: Vec<NamedItem>,
}

#[derive(Default, Serialize)]
struct GlazingOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    u_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    g_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame_area_fraction: Option<f64>,
}

#[derive(Serialize)]
struct CompareRequest {
    archetype: String,
    upgrade_overrides: GlazingOverrides,
    #[serde(skip_serializing_if = "Option::is_none")]
    weather: Option<String>,
}

#[derive(Clone, PartialEq, Deserialize)]
struct CostCarbon {
    cost_gbp: f64,
    carbon_kg: f64,
}

#[derive(Clone, PartialEq, Deserialize)]
struct Summary {
    total_floor_area: f64,
    space_heat_demand_total: f64,
    space_cool_demand_total: f64,
}

#[derive(Clone, PartialEq, Deserialize)]
struct Scenario {
    cost_carbon: CostCarbon,
    summary: Summary,
}

#[derive(Clone, PartialEq, Deserialize)]
struct Delta {
    space_heat_demand_reduction: f64,
    cost_gbp_reduction: f64,
    carbon_kg_reduction: f64,
    delivered_energy_reduction: Option<f64>,
}

#[derive(Clone, PartialEq, Deserialize)]
struct EconomicsUsed {
    source: Option<String>,
}

#[derive(Clone, PartialEq, Deserialize)]
struct WindowInfo {
    zone: String,
    name: String,
    orientation360: Option<f64>,
    pitch: Option<f64>,
}

#[derive(Clone, PartialEq, Deserialize)]
struct CompareResponse {
    archetype: String,
    weather: String,
    windows: Vec<WindowInfo>,
    economics_used: EconomicsUsed,
    baseline: Scenario,
    upgrade: Scenario,
    delta: Delta,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------

async fn fetch_named(url: &str) -> Result<Vec<NamedItem>, String> {
    let resp = gloo_net::http::Request::get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if url.ends_with("archetypes") {
        resp.json::<ArchetypesResp>()
            .await
            .map(|r| r.archetypes)
            .map_err(|e| format!("bad response: {e}"))
    } else {
        resp.json::<WeatherResp>()
            .await
            .map(|r| r.weather)
            .map_err(|e| format!("bad response: {e}"))
    }
}

async fn post_compare(body: &CompareRequest) -> Result<CompareResponse, String> {
    let resp = gloo_net::http::Request::post("/compare")
        .json(body)
        .map_err(|e| format!("could not encode request: {e}"))?
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if resp.ok() {
        resp.json::<CompareResponse>()
            .await
            .map_err(|e| format!("could not parse result: {e}"))
    } else {
        let status = resp.status();
        // Error bodies are `{"error": "..."}`, but be defensive: fall back to raw text.
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ErrorBody>(&text)
            .map(|b| b.error)
            .unwrap_or(text);
        Err(format!("server returned {status}: {msg}"))
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// Parse an optional numeric field: empty ⇒ Ok(None), a number ⇒ Ok(Some), else the field label
/// as an error so the user sees which input is malformed.
fn parse_opt(raw: &str, label: &str) -> Result<Option<f64>, String> {
    let t = raw.trim();
    if t.is_empty() {
        Ok(None)
    } else {
        t.parse::<f64>()
            .map(Some)
            .map_err(|_| format!("{label} must be a number"))
    }
}

fn fmt(x: f64) -> String {
    format!("{x:.1}")
}

#[function_component(App)]
fn app() -> Html {
    let archetypes = use_state(Vec::<NamedItem>::new);
    let weather = use_state(Vec::<NamedItem>::new);
    let load_err = use_state(|| Option::<String>::None);

    let archetype = use_state(String::new);
    let weather_id = use_state(String::new);
    let u_value = use_state(|| "0.8".to_string());
    let g_value = use_state(|| "0.5".to_string());
    let frame = use_state(String::new);

    let result = use_state(|| Option::<CompareResponse>::None);
    let run_err = use_state(|| Option::<String>::None);
    let busy = use_state(|| false);

    // Load archetypes + weather once on mount, and seed the default selections.
    {
        let archetypes = archetypes.clone();
        let weather = weather.clone();
        let archetype = archetype.clone();
        let weather_id = weather_id.clone();
        let load_err = load_err.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                match fetch_named("/archetypes").await {
                    Ok(list) => {
                        if let Some(first) = list.first() {
                            if archetype.is_empty() {
                                archetype.set(first.id.clone());
                            }
                        }
                        archetypes.set(list);
                    }
                    Err(e) => load_err.set(Some(e)),
                }
                match fetch_named("/weather").await {
                    Ok(list) => {
                        if let Some(first) = list.first() {
                            if weather_id.is_empty() {
                                weather_id.set(first.id.clone());
                            }
                        }
                        weather.set(list);
                    }
                    Err(e) => load_err.set(Some(e)),
                }
            });
            || ()
        });
    }

    let on_archetype = {
        let archetype = archetype.clone();
        Callback::from(move |e: Event| {
            let sel: HtmlSelectElement = e.target_unchecked_into();
            archetype.set(sel.value());
        })
    };
    let on_weather = {
        let weather_id = weather_id.clone();
        Callback::from(move |e: Event| {
            let sel: HtmlSelectElement = e.target_unchecked_into();
            weather_id.set(sel.value());
        })
    };
    let text_setter = |state: UseStateHandle<String>| {
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            state.set(input.value());
        })
    };

    let on_run = {
        let archetype = archetype.clone();
        let weather_id = weather_id.clone();
        let u_value = u_value.clone();
        let g_value = g_value.clone();
        let frame = frame.clone();
        let result = result.clone();
        let run_err = run_err.clone();
        let busy = busy.clone();
        Callback::from(move |_: MouseEvent| {
            let u = parse_opt(&u_value, "U-value");
            let g = parse_opt(&g_value, "g-value");
            let f = parse_opt(&frame, "frame fraction");
            let (u, g, f) = match (u, g, f) {
                (Ok(u), Ok(g), Ok(f)) => (u, g, f),
                (a, b, c) => {
                    let msg = [a.err(), b.err(), c.err()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("; ");
                    run_err.set(Some(msg));
                    return;
                }
            };
            if u.is_none() && g.is_none() && f.is_none() {
                run_err.set(Some(
                    "Enter at least one upgraded glazing value to compare against the as-built windows."
                        .to_string(),
                ));
                return;
            }
            let body = CompareRequest {
                archetype: (*archetype).clone(),
                upgrade_overrides: GlazingOverrides {
                    u_value: u,
                    g_value: g,
                    frame_area_fraction: f,
                },
                weather: if weather_id.is_empty() {
                    None
                } else {
                    Some((*weather_id).clone())
                },
            };
            let result = result.clone();
            let run_err = run_err.clone();
            let busy = busy.clone();
            busy.set(true);
            run_err.set(None);
            spawn_local(async move {
                match post_compare(&body).await {
                    Ok(r) => {
                        result.set(Some(r));
                    }
                    Err(e) => {
                        result.set(None);
                        run_err.set(Some(e));
                    }
                }
                busy.set(false);
            });
        })
    };

    html! {
        <div class="wrap">
            <header>
                <h1>{ "HEM glazing study" }</h1>
                <p>{ "Compare an upgraded glazing spec against a dwelling's as-built windows — space-heat demand, running cost, and carbon. Design/research figures, not a compliance calculation." }</p>
            </header>

            if let Some(e) = &*load_err {
                <div class="panel err">{ format!("Could not load options from the server: {e}") }</div>
            }

            <div class="panel">
                <h2>{ "Scenario" }</h2>
                <div class="grid">
                    <div>
                        <label>{ "Archetype" }</label>
                        <select onchange={on_archetype}>
                            { for archetypes.iter().map(|a| html!{
                                <option value={a.id.clone()} selected={a.id == *archetype}>{ &a.name }</option>
                            }) }
                        </select>
                        <div class="hint">
                            { archetypes.iter().find(|a| a.id == *archetype).map(|a| a.description.clone()).unwrap_or_default() }
                        </div>
                    </div>
                    <div>
                        <label>{ "Weather" }</label>
                        <select onchange={on_weather}>
                            { for weather.iter().map(|w| html!{
                                <option value={w.id.clone()} selected={w.id == *weather_id}>{ &w.name }</option>
                            }) }
                        </select>
                    </div>
                </div>
            </div>

            <div class="panel">
                <h2>{ "Upgraded glazing (applied to all windows)" }</h2>
                <div class="grid">
                    <div>
                        <label>{ "U-value (W/m\u{00b2}\u{00b7}K)" }</label>
                        <input type="number" step="0.01" min="0" value={(*u_value).clone()} oninput={text_setter(u_value.clone())} />
                        <div class="hint">{ "Whole-window U from the datasheet. Lower = less heat loss." }</div>
                    </div>
                    <div>
                        <label>{ "g-value (0\u{2013}1)" }</label>
                        <input type="number" step="0.01" min="0" max="1" value={(*g_value).clone()} oninput={text_setter(g_value.clone())} />
                        <div class="hint">{ "Solar factor. Higher = more free solar gain." }</div>
                    </div>
                    <div>
                        <label>{ "Frame area fraction (0\u{2013}1)" }</label>
                        <input type="number" step="0.01" min="0" max="1" value={(*frame).clone()} oninput={text_setter(frame.clone())} placeholder="unchanged" />
                        <div class="hint">{ "Optional. Leave blank to keep as-built." }</div>
                    </div>
                </div>
                <button onclick={on_run} disabled={*busy || archetype.is_empty()}>
                    { if *busy { "Running\u{2026}" } else { "Run comparison" } }
                </button>
            </div>

            if let Some(e) = &*run_err {
                <div class="panel err">{ e }</div>
            }

            if let Some(r) = &*result {
                { results_view(r) }
            }
        </div>
    }
}

fn signed_class(x: f64) -> &'static str {
    if x > 0.0 {
        "pos"
    } else if x < 0.0 {
        "neg"
    } else {
        ""
    }
}

fn results_view(r: &CompareResponse) -> Html {
    let d = &r.delta;
    let de = d
        .delivered_energy_reduction
        .map(|v| format!("{v:.1} kWh"))
        .unwrap_or_else(|| "\u{2014}".to_string());
    html! {
        <>
        <div class="panel">
            <h2>{ "Headline savings (baseline \u{2212} upgrade)" }</h2>
            <div class="cards">
                <div class="card">
                    <div class="k">{ "Space-heat demand" }</div>
                    <div class={classes!("v", signed_class(d.space_heat_demand_reduction))}>{ fmt(d.space_heat_demand_reduction) }</div>
                    <div class="u">{ "kWh saved" }</div>
                </div>
                <div class="card">
                    <div class="k">{ "Running cost" }</div>
                    <div class={classes!("v", signed_class(d.cost_gbp_reduction))}>{ format!("\u{00a3}{:.2}", d.cost_gbp_reduction) }</div>
                    <div class="u">{ "saved (unit-rate)" }</div>
                </div>
                <div class="card">
                    <div class="k">{ "Carbon" }</div>
                    <div class={classes!("v", signed_class(d.carbon_kg_reduction))}>{ fmt(d.carbon_kg_reduction) }</div>
                    <div class="u">{ "kgCO\u{2082}e saved" }</div>
                </div>
                <div class="card">
                    <div class="k">{ "Delivered energy" }</div>
                    <div class="v">{ de }</div>
                    <div class="u">{ "saved" }</div>
                </div>
            </div>

            <table>
                <thead>
                    <tr><th>{ "Metric" }</th><th>{ "Baseline" }</th><th>{ "Upgrade" }</th><th>{ "Reduction" }</th></tr>
                </thead>
                <tbody>
                    <tr>
                        <td>{ "Space-heat demand (kWh)" }</td>
                        <td>{ fmt(r.baseline.summary.space_heat_demand_total) }</td>
                        <td>{ fmt(r.upgrade.summary.space_heat_demand_total) }</td>
                        <td class={classes!(signed_class(d.space_heat_demand_reduction))}>{ fmt(d.space_heat_demand_reduction) }</td>
                    </tr>
                    <tr>
                        <td>{ "Space-cool demand (kWh)" }</td>
                        <td>{ fmt(r.baseline.summary.space_cool_demand_total) }</td>
                        <td>{ fmt(r.upgrade.summary.space_cool_demand_total) }</td>
                        <td>{ "\u{2014}" }</td>
                    </tr>
                    <tr>
                        <td>{ "Running cost (\u{00a3})" }</td>
                        <td>{ format!("{:.2}", r.baseline.cost_carbon.cost_gbp) }</td>
                        <td>{ format!("{:.2}", r.upgrade.cost_carbon.cost_gbp) }</td>
                        <td class={classes!(signed_class(d.cost_gbp_reduction))}>{ format!("{:.2}", d.cost_gbp_reduction) }</td>
                    </tr>
                    <tr>
                        <td>{ "Carbon (kgCO\u{2082}e)" }</td>
                        <td>{ fmt(r.baseline.cost_carbon.carbon_kg) }</td>
                        <td>{ fmt(r.upgrade.cost_carbon.carbon_kg) }</td>
                        <td class={classes!(signed_class(d.carbon_kg_reduction))}>{ fmt(d.carbon_kg_reduction) }</td>
                    </tr>
                </tbody>
            </table>

            <p class="note">
                { format!("Figures are over the simulated period ({} weather), not annualised. Floor area {:.1} m\u{00b2}. ",
                    r.weather, r.upgrade.summary.total_floor_area) }
                { r.economics_used.source.clone().unwrap_or_default() }
            </p>

            <details>
                <summary>{ format!("{} window(s) in this dwelling", r.windows.len()) }</summary>
                <ul class="win-list">
                    { for r.windows.iter().map(|w| html!{
                        <li>{ format!("{} / {} \u{2014} orientation {}\u{00b0}, pitch {}\u{00b0}",
                            w.zone, w.name,
                            w.orientation360.map(|v| format!("{v:.0}")).unwrap_or_else(|| "?".into()),
                            w.pitch.map(|v| format!("{v:.0}")).unwrap_or_else(|| "?".into())) }</li>
                    }) }
                </ul>
            </details>
        </div>
        </>
    }
}

fn main() {
    yew::Renderer::<App>::new().render();
}
