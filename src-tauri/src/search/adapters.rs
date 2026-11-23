//! Un adaptador por proveedor. Cada uno devuelve ya `ImageHit` normalizados;
//! cualquier fallo se queda en su proveedor y no afecta a los demás.

use serde_json::Value;

use super::{normalize_license, ImageHit, Keys};

const UA: &str = "img-upscaler/0.3 (app de escritorio personal)";

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent(UA)
        .build()
        .unwrap_or_default()
}

/// Despacha a la fuente pedida; una desconocida simplemente no devuelve nada.
pub async fn fetch(
    id: &str,
    client: &reqwest::Client,
    keys: &Keys,
    query: &str,
    page: u32,
    limit: usize,
) -> Result<Vec<ImageHit>, String> {
    match id {
        "openverse" => openverse(client, query, page, limit).await,
        "commons" => wikimedia(client, query, page, limit).await,
        "brave" => brave(client, &keys.brave, query, page, limit).await,
        "google_images" | "yandex_images" => {
            serpapi(client, &keys.serpapi, id, query, page).await
        }
        "searxng" => searxng(client, &keys.searxng, query, page).await,
        _ => Ok(Vec::new()),
    }
}

fn text(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(|value| value.trim().to_string())
}

fn number(value: &Value, path: &[&str]) -> u32 {
    let mut current = value;
    for key in path {
        match current.get(*key) {
            Some(next) => current = next,
            None => return 0,
        }
    }
    current
        .as_u64()
        .or_else(|| current.as_str().and_then(|raw| raw.parse().ok()))
        .unwrap_or(0) as u32
}

fn strip_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut inside = false;
    for char in raw.chars() {
        match char {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(char),
            _ => {}
        }
    }
    out.trim().to_string()
}

async fn get_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| format!("Respuesta ilegible: {error}"))
}

/// Openverse: imágenes con licencia abierta, sin clave.
pub async fn openverse(
    client: &reqwest::Client,
    query: &str,
    page: u32,
    limit: usize,
) -> Result<Vec<ImageHit>, String> {
    // por encima de ~10 resultados el gateway anónimo de Openverse devuelve 5xx
    let limit = limit.min(10);
    let params = vec![
        ("q", query.to_string()),
        ("page", (page + 1).to_string()),
        ("page_size", limit.to_string()),
    ];
    let response = client
        .get("https://api.openverse.org/v1/images/")
        .query(&params)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(response).await?;

    Ok(body["results"]
        .as_array()
        .map(|items| items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let url = text(item, &["url"])?;
            Some(ImageHit {
                id: format!("openverse:{}", text(item, &["id"]).unwrap_or_else(|| url.clone())),
                thumb: text(item, &["thumbnail"]).unwrap_or_else(|| url.clone()),
                page: text(item, &["foreign_landing_url"]),
                title: text(item, &["title"]).unwrap_or_else(|| "Sin título".into()),
                author: text(item, &["creator"]),
                license: normalize_license(
                    &text(item, &["license"]).unwrap_or_default(),
                    text(item, &["license_version"]).as_deref(),
                ),
                width: number(item, &["width"]),
                height: number(item, &["height"]),
                url,
                source: "openverse".into(),
            })
        })
        .collect())
}

/// Wikimedia Commons: sin clave, licencias y autoría en los metadatos.
pub async fn wikimedia(
    client: &reqwest::Client,
    query: &str,
    page: u32,
    limit: usize,
) -> Result<Vec<ImageHit>, String> {
    let offset = page * limit as u32;
    let search = format!("filetype:bitmap {query}");
    let params = vec![
        ("action", "query".to_string()),
        ("format", "json".to_string()),
        ("generator", "search".to_string()),
        ("gsrnamespace", "6".to_string()),
        ("gsrsearch", search.clone()),
        ("gsrlimit", limit.to_string()),
        ("gsroffset", offset.to_string()),
        ("prop", "imageinfo".to_string()),
        ("iiprop", "url|size|extmetadata".to_string()),
        ("iiurlwidth", "480".to_string()),
    ];
    let response = client
        .get("https://commons.wikimedia.org/w/api.php")
        .query(&params)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(response).await?;

    let mut hits = Vec::new();
    if let Some(pages) = body["query"]["pages"].as_object() {
        for page in pages.values() {
            let info = &page["imageinfo"][0];
            let Some(url) = info["url"].as_str() else {
                continue;
            };
            let title = page["title"]
                .as_str()
                .unwrap_or("Sin título")
                .trim_start_matches("File:")
                .to_string();
            hits.push(ImageHit {
                id: format!("commons:{}", page["pageid"].as_u64().unwrap_or(0)),
                thumb: info["thumburl"].as_str().unwrap_or(url).to_string(),
                page: Some(format!(
                    "https://commons.wikimedia.org/?curid={}",
                    page["pageid"].as_u64().unwrap_or(0)
                )),
                title,
                author: text(&info["extmetadata"], &["Artist", "value"]).map(|raw| strip_html(&raw)),
                license: text(&info["extmetadata"], &["LicenseShortName", "value"])
                    .map(|raw| strip_html(&raw)),
                width: number(info, &["width"]),
                height: number(info, &["height"]),
                url: url.to_string(),
                source: "commons".into(),
            });
        }
    }
    Ok(hits)
}

/// Brave Images: índice propio, clave con crédito mensual.
pub async fn brave(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    page: u32,
    limit: usize,
) -> Result<Vec<ImageHit>, String> {
    let params = vec![
        ("q", query.to_string()),
        ("count", limit.to_string()),
        ("offset", (page * limit as u32).to_string()),
        ("safesearch", "moderate".to_string()),
    ];
    let response = client
        .get("https://api.search.brave.com/res/v1/images/search")
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&params)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(response).await?;

    Ok(body["results"]
        .as_array()
        .map(|items| items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let url = text(item, &["properties", "url"])
                .or_else(|| text(item, &["thumbnail", "src"]))?;
            Some(ImageHit {
                id: format!("brave:{url}"),
                thumb: text(item, &["thumbnail", "src"]).unwrap_or_else(|| url.clone()),
                page: text(item, &["url"]),
                title: text(item, &["title"]).unwrap_or_else(|| "Sin título".into()),
                author: text(item, &["source"]),
                license: None,
                width: 0,
                height: 0,
                url,
                source: "brave".into(),
            })
        })
        .collect())
}

/// SerpAPI: `google_images` o `yandex_images`. La clave es del usuario.
pub async fn serpapi(
    client: &reqwest::Client,
    key: &str,
    engine: &str,
    query: &str,
    page: u32,
) -> Result<Vec<ImageHit>, String> {
    let (page_key, page_value) = if engine == "google_images" {
        ("ijn", page.to_string())
    } else {
        ("p", (page + 1).to_string())
    };
    let query_key = if engine == "google_images" { "q" } else { "text" };

    let params = vec![
        ("engine", engine.to_string()),
        ("api_key", key.to_string()),
        (query_key, query.to_string()),
        (page_key, page_value),
        ("hl", "es".to_string()),
    ];
    let response = client
        .get("https://serpapi.com/search.json")
        .query(&params)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(response).await?;

    Ok(body["images_results"]
        .as_array()
        .map(|items| items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let url = text(item, &["original"]).or_else(|| text(item, &["thumbnail"]))?;
            Some(ImageHit {
                id: format!("{engine}:{url}"),
                thumb: text(item, &["thumbnail"]).unwrap_or_else(|| url.clone()),
                page: text(item, &["link"]),
                title: text(item, &["title"]).unwrap_or_else(|| "Sin título".into()),
                author: text(item, &["source"]),
                license: None,
                width: number(item, &["original_width"]),
                height: number(item, &["original_height"]),
                url,
                source: engine.into(),
            })
        })
        .collect())
}

/// SearXNG propio: agrega varios buscadores bajo una URL configurable.
pub async fn searxng(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    page: u32,
) -> Result<Vec<ImageHit>, String> {
    let endpoint = format!("{}/search", base.trim_end_matches('/'));
    let params = vec![
        ("q", query.to_string()),
        ("format", "json".to_string()),
        ("categories", "images".to_string()),
        ("pageno", (page + 1).to_string()),
    ];
    let response = client
        .get(endpoint)
        .query(&params)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(response).await?;

    Ok(body["results"]
        .as_array()
        .map(|items| items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let url = text(item, &["img_src"])?;
            Some(ImageHit {
                id: format!("searxng:{url}"),
                thumb: text(item, &["thumbnail_src"]).unwrap_or_else(|| url.clone()),
                page: text(item, &["url"]),
                title: text(item, &["title"]).unwrap_or_else(|| "Sin título".into()),
                author: text(item, &["source"]),
                license: text(item, &["license"]),
                width: number(item, &["img_width"]),
                height: number(item, &["img_height"]),
                url,
                source: "searxng".into(),
            })
        })
        .collect())
}

/// Google Lens vía SerpAPI: sube la imagen (≤500 KB) y busca coincidencias visuales.
pub async fn lens(
    client: &reqwest::Client,
    key: &str,
    image_bytes: Vec<u8>,
) -> Result<Vec<ImageHit>, String> {
    let part = reqwest::multipart::Part::bytes(image_bytes)
        .file_name("consulta.jpg")
        .mime_str("image/jpeg")
        .map_err(|error| error.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("api_key", key.to_string())
        .part("image", part);

    let upload = client
        .post("https://serpapi.com/image")
        .multipart(form)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(upload).await?;
    let image_id = body["image_id"]
        .as_str()
        .ok_or_else(|| "SerpAPI no devolvió un identificador de imagen.".to_string())?;

    let params = vec![
        ("engine", "google_lens".to_string()),
        ("api_key", key.to_string()),
        ("image_id", image_id.to_string()),
        ("type", "visual_matches".to_string()),
        ("hl", "es".to_string()),
    ];
    let response = client
        .get("https://serpapi.com/search.json")
        .query(&params)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body = get_json(response).await?;

    Ok(body["visual_matches"]
        .as_array()
        .map(|items| items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let url = text(item, &["image"]).or_else(|| text(item, &["thumbnail"]))?;
            Some(ImageHit {
                id: format!("lens:{url}"),
                thumb: text(item, &["thumbnail"]).unwrap_or_else(|| url.clone()),
                page: text(item, &["link"]),
                title: text(item, &["title"]).unwrap_or_else(|| "Sin título".into()),
                author: text(item, &["source"]),
                license: None,
                width: 0,
                height: 0,
                url,
                source: "lens".into(),
            })
        })
        .collect())
}
