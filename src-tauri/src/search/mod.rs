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

impl SourceStatus {
    pub fn new(id: &str, state: &str, count: usize, message: Option<String>) -> Self {
        Self {
            id: id.to_string(),
            state: state.to_string(),
            count,
            message,
        }
    }
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

const LIMIT: usize = 24;

/// Lanza todos los proveedores activos en paralelo y fusiona lo que llegue.
pub async fn run(request: &SearchRequest, keys: &Keys) -> SearchResponse {
    let started = std::time::Instant::now();
    let client = adapters::client();

    let mut statuses = Vec::new();
    let mut active = Vec::new();
    for spec in SOURCES {
        if !keys.configured(spec) {
            statuses.push(SourceStatus::new(spec.id, "no-key", 0, None));
        } else if !keys.enabled(spec.id) {
            statuses.push(SourceStatus::new(spec.id, "off", 0, None));
        } else {
            active.push(spec);
        }
    }

    let tasks = active.into_iter().map(|spec| {
        let client = client.clone();
        async move {
            let hits = adapters::fetch(spec.id, &client, keys, &request.query, request.page, LIMIT).await;
            (spec.id, hits)
        }
    });

    let mut per_source = Vec::new();
    for (id, result) in join_all(tasks).await {
        match result {
            Ok(hits) => {
                let state = if hits.is_empty() { "empty" } else { "ok" };
                statuses.push(SourceStatus::new(id, state, hits.len(), None));
                per_source.push((id.to_string(), hits));
            }
            Err(message) => statuses.push(SourceStatus::new(id, "error", 0, Some(message))),
        }
    }

    SearchResponse {
        hits: merge(per_source, &request.filters),
        statuses: statuses_in_order(statuses),
        took_ms: started.elapsed().as_millis() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, url: &str, width: u32, height: u32, license: Option<&str>) -> ImageHit {
        ImageHit {
            id: id.to_string(),
            thumb: url.to_string(),
            url: url.to_string(),
            page: None,
            title: id.to_string(),
            author: None,
            license: license.map(|value| value.to_string()),
            width,
            height,
            source: "test".to_string(),
        }
    }

    #[test]
    fn merge_dedupes_urls_ignoring_query_and_case() {
        let merged = merge(
            vec![
                ("a".to_string(), vec![hit("uno", "https://cdn.test/foto.jpg?w=200", 2000, 1000, None)]),
                ("b".to_string(), vec![hit("dos", "https://CDN.test/foto.jpg", 2000, 1000, Some("CC0"))]),
            ],
            &Filters::default(),
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].license.as_deref(), Some("CC0"), "hereda la licencia de la otra fuente");
    }

    #[test]
    fn merge_rewards_hits_found_by_several_sources() {
        let merged = merge(
            vec![
                ("a".to_string(), vec![hit("solo-a", "https://x/solo.jpg", 0, 0, None), hit("comun", "https://x/comun.jpg", 0, 0, None)]),
                ("b".to_string(), vec![hit("comun-b", "https://x/comun.jpg", 0, 0, None)]),
            ],
            &Filters::default(),
        );

        let order: Vec<&str> = merged.iter().map(|entry| entry.title.as_str()).collect();
        assert_eq!(order, vec!["comun", "solo-a"]);
    }

    #[test]
    fn filters_drop_small_or_misaligned_hits() {
        let hits = vec![
            hit("grande", "https://x/1.jpg", 2400, 1600, Some("CC BY 4.0")),
            hit("mediana", "https://x/2.jpg", 1000, 900, Some("CC BY 4.0")),
            hit("vertical", "https://x/3.jpg", 1400, 2400, None),
        ];

        let large = merge(
            vec![("a".to_string(), hits.clone())],
            &Filters { size: Some("large".to_string()), ..Filters::default() },
        );
        assert_eq!(large.len(), 2);

        let open = merge(
            vec![("a".to_string(), hits.clone())],
            &Filters { open_license: Some(true), ..Filters::default() },
        );
        assert!(open.iter().all(|entry| entry.license.is_some()));

        let portrait = merge(
            vec![("a".to_string(), hits)],
            &Filters { orientation: Some("portrait".to_string()), ..Filters::default() },
        );
        assert_eq!(portrait.len(), 1);
        assert_eq!(portrait[0].title, "vertical");
    }

    #[test]
    fn license_labels_are_readable() {
        assert_eq!(normalize_license("cc0", None).as_deref(), Some("Dominio público"));
        assert_eq!(normalize_license("by-sa", Some("4.0")).as_deref(), Some("CC BY SA 4.0"));
        assert_eq!(normalize_license("  ", None), None);
    }
}
