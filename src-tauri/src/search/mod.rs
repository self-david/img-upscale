//! Búsqueda de imágenes federada: cada adaptador consulta su proveedor y aquí
//! se fusionan los resultados. Un proveedor caído no afecta a los demás.

pub mod adapters;

use futures::future::join_all;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageHit {
    pub id: String,
    pub thumb: String,
    pub url: String,
    pub page: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub width: u32,
    pub height: u32,
    pub source: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    /// "large" | "medium" | "any"
    #[serde(default)]
    pub size: Option<String>,
    /// "landscape" | "portrait" | "square" | "any"
    #[serde(default)]
    pub orientation: Option<String>,
    #[serde(default)]
    pub open_license: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub page: u32,
    #[serde(default)]
    pub filters: Filters,
    /// vacío = todas las fuentes activas
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub id: String,
    /// "ok" | "empty" | "no-key" | "off" | "error"
    pub state: String,
    pub count: usize,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub hits: Vec<ImageHit>,
    pub statuses: Vec<SourceStatus>,
    pub took_ms: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Need {
    None,
    Brave,
    SerpApi,
    Url,
}

pub struct SourceSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub needs: Need,
    pub signup: &'static str,
    pub note: &'static str,
}

pub const SOURCES: &[SourceSpec] = &[
    SourceSpec {
        id: "openverse",
        name: "Openverse",
        needs: Need::None,
        signup: "https://api.openverse.org/",
        note: "Licencias abiertas · funciona sin clave",
    },
    SourceSpec {
        id: "commons",
        name: "Wikimedia",
        needs: Need::None,
        signup: "https://commons.wikimedia.org/",
        note: "Commons · autor y licencia visibles",
    },
    SourceSpec {
        id: "brave",
        name: "Brave",
        needs: Need::Brave,
        signup: "https://api-dashboard.search.brave.com/",
        note: "Índice propio · 5 $ de crédito al mes",
    },
    SourceSpec {
        id: "google_images",
        name: "Google",
        needs: Need::SerpApi,
        signup: "https://serpapi.com/",
        note: "Google Images a través de SerpAPI",
    },
    SourceSpec {
        id: "yandex_images",
        name: "Yandex",
        needs: Need::SerpApi,
        signup: "https://serpapi.com/",
        note: "Yandex Images a través de SerpAPI",
    },
    SourceSpec {
        id: "searxng",
        name: "SearXNG",
        needs: Need::Url,
        signup: "https://docs.searxng.org/",
        note: "Tu instancia propia con salida JSON",
    },
];

#[derive(Default, Clone)]
pub struct Keys {
    pub brave: String,
    pub serpapi: String,
    pub searxng: String,
    /// fuentes desactivadas a mano; vacío = todas las que tengan credenciales
    pub disabled: Vec<String>,
}

impl Keys {
    pub fn configured(&self, spec: &SourceSpec) -> bool {
        match spec.needs {
            Need::None => true,
            Need::Brave => !self.brave.is_empty(),
            Need::SerpApi => !self.serpapi.is_empty(),
            Need::Url => !self.searxng.is_empty(),
        }
    }

    pub fn enabled(&self, id: &str) -> bool {
        !self.disabled.iter().any(|entry| entry == id)
    }
}

pub fn normalize_license(license: &str, version: Option<&str>) -> Option<String> {
    let license = license.trim();
    if license.is_empty() {
        return None;
    }
    let upper = license.to_ascii_uppercase();
    let label = if upper == "CC0" || upper == "PUBLICDOMAIN" || upper == "PDM" {
        "Dominio público".to_string()
    } else if upper.starts_with("BY") || upper.starts_with("CC") {
        let code = upper.trim_start_matches("CC-").replace('-', " ");
        match version {
            Some(version) if !version.is_empty() => format!("CC {code} {version}"),
            _ => format!("CC {code}"),
        }
    } else {
        license.to_string()
    };
    Some(label)
}

fn dedupe_key(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn passes(hit: &ImageHit, filters: &Filters) -> bool {
    if filters.open_license.unwrap_or(false) && hit.license.is_none() {
        return false;
    }
    let long = hit.width.max(hit.height);
    if hit.width > 0 && hit.height > 0 {
        match filters.size.as_deref() {
            Some("large") if long < 1200 => return false,
            Some("medium") if long < 800 || long >= 1200 => return false,
            _ => {}
        }
        let ratio = hit.width as f32 / hit.height as f32;
        match filters.orientation.as_deref() {
            Some("landscape") if ratio < 1.05 => return false,
            Some("portrait") if ratio > 0.95 => return false,
            Some("square") if !(0.9..=1.1).contains(&ratio) => return false,
            _ => {}
        }
    }
    true
}

/// Fusión con *Reciprocal Rank Fusion* y dedupe por URL de la imagen.
pub fn merge(per_source: Vec<(String, Vec<ImageHit>)>, filters: &Filters) -> Vec<ImageHit> {
    let mut scored: Vec<(f32, ImageHit)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for (_, hits) in per_source {
        for (rank, hit) in hits.into_iter().filter(|hit| passes(hit, filters)).enumerate() {
            let weight = 1.0 / (60.0 + rank as f32);
            let key = dedupe_key(&hit.url);
            match index.get(&key) {
                Some(position) => {
                    let entry = &mut scored[*position];
                    entry.0 += weight;
                    if entry.1.width == 0 && hit.width > 0 {
                        entry.1.width = hit.width;
                        entry.1.height = hit.height;
                    }
                    if entry.1.license.is_none() {
                        entry.1.license = hit.license.clone();
                    }
                    if entry.1.author.is_none() {
                        entry.1.author = hit.author.clone();
                    }
                }
                None => {
                    index.insert(key, scored.len());
                    scored.push((weight, hit));
                }
            }
        }
    }

    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().map(|(_, hit)| hit).collect()
}

pub fn statuses_in_order(statuses: Vec<SourceStatus>) -> Vec<SourceStatus> {
    let mut statuses = statuses;
    statuses.sort_by_key(|status| {
        SOURCES
            .iter()
            .position(|spec| spec.id == status.id)
            .unwrap_or(usize::MAX)
    });
    statuses
}

/// Lanza todos los proveedores activos en paralelo y fusiona lo que llegue.
pub async fn run(request: &SearchRequest, keys: &Keys) -> SearchResponse {
    let started = std::time::Instant::now();
    let limit = 24usize;

    let client = match adapters::client() {
        Ok(client) => client,
        Err(message) => {
            return SearchResponse {
                hits: Vec::new(),
                statuses: SOURCES
                    .iter()
                    .map(|spec| SourceStatus {
                        id: spec.id.to_string(),
                        state: "error".to_string(),
                        count: 0,
                        message: Some(message.clone()),
                    })
                    .collect(),
                took_ms: 0,
            }
        }
    };

    let mut statuses = Vec::new();
    let mut tasks = Vec::new();

    for spec in SOURCES {
        let wanted = if request.sources.is_empty() {
            keys.enabled(spec.id)
        } else {
            request.sources.iter().any(|id| id == spec.id)
        };
        if !wanted {
            statuses.push(SourceStatus {
                id: spec.id.to_string(),
                state: "off".to_string(),
                count: 0,
                message: None,
            });
            continue;
        }
        if !keys.configured(spec) {
            statuses.push(SourceStatus {
                id: spec.id.to_string(),
                state: "no-key".to_string(),
                count: 0,
                message: None,
            });
            continue;
        }

        let query = request.query.clone();
        let page = request.page;
        let keys = keys.clone();
        let client = client.clone();
        tasks.push(async move {
            let result = match spec.id {
                "openverse" => adapters::openverse(&client, &query, page, limit).await,
                "commons" => adapters::wikimedia(&client, &query, page, limit).await,
                "brave" => adapters::brave(&client, &keys.brave, &query, page, limit).await,
                "google_images" => {
                    adapters::serpapi(&client, &keys.serpapi, "google_images", "google_images", &query, page)
                        .await
                }
                "yandex_images" => {
                    adapters::serpapi(&client, &keys.serpapi, "yandex_images", "yandex_images", &query, page)
                        .await
                }
                "searxng" => adapters::searxng(&client, &keys.searxng, &query, page).await,
                _ => Ok(Vec::new()),
            };
            (spec.id, result)
        });
    }

    let mut per_source = Vec::new();
    for (id, result) in join_all(tasks).await {
        match result {
            Ok(hits) => {
                statuses.push(SourceStatus {
                    id: id.to_string(),
                    state: if hits.is_empty() { "empty" } else { "ok" }.to_string(),
                    count: hits.len(),
                    message: None,
                });
                per_source.push((id.to_string(), hits));
            }
            Err(message) => statuses.push(SourceStatus {
                id: id.to_string(),
                state: "error".to_string(),
                count: 0,
                message: Some(message),
            }),
        }
    }

    SearchResponse {
        hits: merge(per_source, &request.filters),
        statuses: statuses_in_order(statuses),
        took_ms: started.elapsed().as_millis() as u64,
    }
}
