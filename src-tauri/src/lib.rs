mod bg;
mod search;
mod sidecar;
mod store;

use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    thread,
    time::Instant,
};
use tauri::{AppHandle, Emitter, Manager};

const SUPPORTED: [&str; 8] = ["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff", "gif"];
const MAX_OUTPUT_PIXELS: u64 = 120_000_000;
/// El sidecar solo acierta a su escala nativa (4×); el intermedio 4× también
/// tiene que caber en memoria.
const MAX_INTERMEDIATE_PIXELS: u64 = 320_000_000;
const CANCELLED: &str = "CANCELLED";

#[derive(Serialize, Clone, Debug)]
pub struct ImageMeta {
    path: String,
    name: String,
    width: u32,
    height: u32,
    size: u64,
}

#[derive(Deserialize, Clone)]
pub struct JobRequest {
    id: String,
    path: String,
}

#[derive(Serialize, Clone)]
struct JobProgress {
    id: String,
    percent: u8,
    stage: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct JobDone {
    id: String,
    result_id: String,
    width: u32,
    height: u32,
    ms: u64,
}

#[derive(Serialize, Clone)]
struct JobError {
    id: String,
    message: String,
}

#[derive(Serialize, Clone)]
struct JobCancelled {
    id: String,
}

/// Agrupa los eventos de un trabajo para no repetir el `emit` en cada rama.
struct Reporter<'a> {
    app: &'a AppHandle,
    id: String,
}

impl Reporter<'_> {
    fn progress(&self, percent: u8, stage: &str) {
        let _ = self.app.emit(
            "job:progress",
            JobProgress {
                id: self.id.clone(),
                percent,
                stage: stage.to_string(),
            },
        );
    }

    fn done(&self, stored: &store::Stored, elapsed_ms: u128) {
        let _ = self.app.emit(
            "job:done",
            JobDone {
                id: self.id.clone(),
                result_id: stored.id.clone(),
                width: stored.width,
                height: stored.height,
                ms: elapsed_ms as u64,
            },
        );
    }

    fn failed(&self, message: String) {
        let _ = self.app.emit(
            "job:error",
            JobError {
                id: self.id.clone(),
                message,
            },
        );
    }

    fn cancelled(&self) {
        let _ = self.app.emit(
            "job:cancelled",
            JobCancelled {
                id: self.id.clone(),
            },
        );
    }
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| SUPPORTED.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn meta_for(path: &Path) -> Option<ImageMeta> {
    let (width, height) = image::image_dimensions(path).ok()?;
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Some(ImageMeta {
        path: path.to_string_lossy().to_string(),
        name: path.file_name()?.to_string_lossy().to_string(),
        width,
        height,
        size,
    })
}

/// Expande archivos y carpetas. Las carpetas se leen solo en su raíz, sin recursión.
#[tauri::command]
fn add_paths(paths: Vec<String>) -> Vec<ImageMeta> {
    let mut out = Vec::new();
    for raw in paths {
        let path = PathBuf::from(&raw);
        if path.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|child| child.is_file() && is_supported(child))
                    .collect();
                files.sort();
                out.extend(files.iter().filter_map(|file| meta_for(file)));
            }
        } else if path.is_file() && is_supported(&path) {
            if let Some(meta) = meta_for(&path) {
                out.push(meta);
            }
        }
    }
    out
}

fn output_format(src: &Path) -> (&'static str, image::ImageFormat) {
    match src
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => ("jpg", image::ImageFormat::Jpeg),
        Some("webp") => ("webp", image::ImageFormat::WebP),
        _ => ("png", image::ImageFormat::Png),
    }
}

fn unique_output_path(dir: &Path, stem: &str, scale: u32, ext: &str) -> PathBuf {
    let base = format!("{stem}_{scale}x");
    let mut candidate = dir.join(format!("{base}.{ext}"));
    let mut index = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base}-{index}.{ext}"));
        index += 1;
    }
    candidate
}

/// Carpeta de trabajo: el pipeline escribe ahí y la imagen se mueve a la
/// biblioteca SQLite antes de borrar el temporal.
fn workspace_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "No se pudo localizar la carpeta de datos.".to_string())?
        .join("trabajo");
    fs::create_dir_all(&dir).map_err(|_| "No se pudo crear la carpeta de trabajo.".to_string())?;
    Ok(dir)
}

/// JPEG no admite alfa: se compone sobre blanco antes de guardar.
fn prepare_for_format(mut image: image::DynamicImage, format: image::ImageFormat) -> image::DynamicImage {
    if matches!(format, image::ImageFormat::Jpeg) && image.color().has_alpha() {
        let rgba = image.to_rgba8();
        let mut flattened = image::RgbImage::new(rgba.width(), rgba.height());
        for (x, y, pixel) in rgba.enumerate_pixels() {
            let alpha = pixel[3] as u16;
            let blend = |channel: u8| -> u8 {
                ((channel as u16 * alpha + 255 * (255 - alpha)) / 255) as u8
            };
            flattened.put_pixel(
                x,
                y,
                image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])]),
            );
        }
        image = image::DynamicImage::ImageRgb8(flattened);
    }
    image
}

fn write_image(
    image: image::DynamicImage,
    destination: &Path,
    ext: &str,
    format: image::ImageFormat,
) -> Result<(), String> {
    let temporary = destination.with_extension(format!("{ext}.tmp"));
    prepare_for_format(image, format)
        .save_with_format(&temporary, format)
        .map_err(|_| "No se pudo guardar la imagen resultante.".to_string())?;
    fs::rename(&temporary, destination).map_err(|_| {
        "No se pudo finalizar el archivo; comprueba que la carpeta no esté en uso.".to_string()
    })
}

/// Escala una imagen con Lanczos y la guarda. Devuelve la ruta del resultado.
pub fn upscale_file(
    src: &Path,
    target_dir: &Path,
    scale: u32,
    on_progress: &mut dyn FnMut(u8, &str),
) -> Result<PathBuf, String> {
    on_progress(10, "Leyendo");
    let image = image::open(src).map_err(|_| {
        "No se pudo leer la imagen: puede estar dañada o en un formato no soportado.".to_string()
    })?;

    let (out_w, out_h) = (
        image.width() as u64 * scale as u64,
        image.height() as u64 * scale as u64,
    );
    if out_w * out_h > MAX_OUTPUT_PIXELS {
        return Err(too_big(out_w, out_h));
    }

    on_progress(35, "Escalando");
    let resized = image.resize_exact(
        out_w as u32,
        out_h as u32,
        image::imageops::FilterType::Lanczos3,
    );

    on_progress(75, "Guardando");
    fs::create_dir_all(target_dir)
        .map_err(|_| "No se pudo crear la carpeta de salida.".to_string())?;

    let stem = src
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "imagen".to_string());
    let (ext, format) = output_format(src);
    let destination = unique_output_path(target_dir, &stem, scale, ext);
    write_image(resized, &destination, ext, format)?;

    on_progress(100, "Listo");
    Ok(destination)
}

fn check_limits(width: u32, height: u32, scale: u32) -> Result<(), String> {
    let (out_w, out_h) = (width as u64 * scale as u64, height as u64 * scale as u64);
    if out_w * out_h > MAX_OUTPUT_PIXELS {
        return Err(too_big(out_w, out_h));
    }
    let (native_w, native_h) = (width as u64 * 4, height as u64 * 4);
    if native_w * native_h > MAX_INTERMEDIATE_PIXELS {
        return Err(format!(
            "La imagen es demasiado grande para el modo IA: el motor trabajaría a {native_w}×{native_h} px. Prueba un factor menor o el nivel Simple."
        ));
    }
    Ok(())
}

fn too_big(width: u64, height: u64) -> String {
    format!(
        "La imagen resultante sería de {width}×{height} px, demasiado grande para procesarla con seguridad. Prueba un factor menor."
    )
}

fn run_simple_job(
    job: &JobRequest,
    scale: u32,
    target: &Path,
    reporter: &Reporter,
) -> Result<PathBuf, String> {
    let src = PathBuf::from(&job.path);
    upscale_file(&src, target, scale, &mut |percent, stage| {
        reporter.progress(percent, stage)
    })
}

/// Pipeline de IA sin depender de la ventana: recibe rutas del motor y un
/// callback de progreso, así se puede probar de extremo a extremo.
#[allow(clippy::too_many_arguments)]
fn ai_pipeline(
    job: &JobRequest,
    scale: u32,
    level: &str,
    target_dir: &Path,
    gpu: &sidecar::Gpu,
    exe: &Path,
    models: &Path,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<PathBuf, String> {
    let src = PathBuf::from(&job.path);

    // el motor abre la entrada con su propio decodificador: se le entrega un PNG
    // normalizado para que no dependa del formato ni del perfil de color original
    progress(3, "Preparando imagen");
    let decoded = image::open(&src).map_err(|_| {
        "No se pudo leer la imagen: puede estar dañada o en un formato no soportado.".to_string()
    })?;
    let (width, height) = (decoded.width(), decoded.height());
    let (out_w, out_h) = (width * scale, height * scale);
    check_limits(width, height, scale)?;

    fs::create_dir_all(target_dir)
        .map_err(|_| "No se pudo crear la carpeta de salida.".to_string())?;

    let stem = src
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "imagen".to_string());
    let (ext, format) = output_format(&src);
    let destination = unique_output_path(target_dir, &stem, scale, ext);

    let first = target_dir.join(format!(".{stem}-{}-ia.png", job.id));
    let engine_input = target_dir.join(format!(".{stem}-{}-entrada.png", job.id));
    let discard = || {
        let _ = fs::remove_file(&first);
        let _ = fs::remove_file(&engine_input);
    };

    let source = if decoded.color().has_alpha() {
        image::DynamicImage::ImageRgba8(decoded.to_rgba8())
    } else {
        image::DynamicImage::ImageRgb8(decoded.to_rgb8())
    };
    if source.save(&engine_input).is_err() {
        discard();
        return Err("No se pudo preparar la imagen para el motor de IA.".to_string());
    }

    let (model, tile) = if level == "intermedio" {
        (sidecar::MODEL_INTERMEDIO, 128)
    } else {
        (sidecar::MODEL_POTENTE, 256)
    };

    progress(6, "Escalando con IA");
    if let Err(message) = sidecar::run_with(
        exe,
        models,
        gpu,
        &sidecar::Job {
            src: &engine_input,
            dest: &first,
            model,
            scale: 4,
            tile,
        },
        &mut |percent| progress(6 + (percent as u16 * 74 / 100) as u8, "Escalando con IA"),
    ) {
        discard();
        return Err(message);
    }

    progress(84, "Ajustando tamaño");
    let opened = image::open(&first);
    discard();
    let produced = opened
        .map_err(|error| format!("No se pudo leer el resultado del motor de IA ({error})."))?;
    let produced = if produced.width() != out_w || produced.height() != out_h {
        produced.resize_exact(out_w, out_h, image::imageops::FilterType::Lanczos3)
    } else {
        produced
    };

    progress(92, "Guardando");
    write_image(produced, &destination, ext, format)?;

    Ok(destination)
}

/// Paso final opcional: recorta el fondo del resultado ya escalado y se queda
/// solo con el PNG con transparencia.
fn cutout_result(app: &AppHandle, reporter: &Reporter, path: &Path) -> Result<PathBuf, String> {
    reporter.progress(90, "Quitando el fondo");
    let image = image::open(path)
        .map_err(|error| format!("No se pudo leer el resultado para quitar el fondo ({error})."))?
        .to_rgb8();
    let mut segmenter = bg::Segmenter::load(&sidecar::model_file(app, "u2netp.onnx"))?;
    let mask = segmenter.mask(&image)?;
    let target = bg::output_path(path, path.parent().unwrap_or(Path::new(".")));
    bg::cutout(&image, &mask, &target)?;
    let _ = fs::remove_file(path);
    Ok(target)
}

fn run_ai_job(
    app: &AppHandle,
    gpu: &sidecar::Gpu,
    reporter: &Reporter,
    job: &JobRequest,
    scale: u32,
    level: &str,
    target: &Path,
) -> Result<PathBuf, String> {
    ai_pipeline(
        job,
        scale,
        level,
        target,
        gpu,
        &sidecar::exe(app),
        &sidecar::models_dir(app),
        &mut |percent, stage| reporter.progress(percent, stage),
    )
}

#[tauri::command]
fn start_batch(
    app: AppHandle,
    jobs: Vec<JobRequest>,
    scale: u32,
    level: String,
    remove_bg: Option<bool>,
) -> Result<(), String> {
    if jobs.is_empty() {
        return Err("La cola está vacía.".to_string());
    }
    if !matches!(level.as_str(), "simple" | "intermedio" | "potente") {
        return Err(format!("Nivel de escalado desconocido: {level}"));
    }
    if !(2..=8).contains(&scale) {
        return Err("Factor de escala no válido.".to_string());
    }
    let target = workspace_dir(&app)?;
    let remove_bg = remove_bg.unwrap_or(false);
    app.state::<sidecar::Gpu>()
        .cancel
        .store(false, Ordering::SeqCst);

    let app_handle = app.clone();
    thread::spawn(move || {
        for job in jobs {
            let gpu = app_handle.state::<sidecar::Gpu>();
            let reporter = Reporter {
                app: &app_handle,
                id: job.id.clone(),
            };
            if gpu.cancel.load(Ordering::SeqCst) {
                reporter.cancelled();
                continue;
            }
            let started = Instant::now();
            let produced = if level == "simple" {
                run_simple_job(&job, scale, &target, &reporter)
            } else {
                run_ai_job(&app_handle, &gpu, &reporter, &job, scale, &level, &target)
            };

            match produced {
                Ok(scaled) => {
                    let finished = if remove_bg {
                        match cutout_result(&app_handle, &reporter, &scaled) {
                            Ok(path) => path,
                            Err(message) => {
                                reporter.failed(message);
                                continue;
                            }
                        }
                    } else {
                        scaled
                    };
                    let stored = match store::add(&app_handle, &finished, &level, scale) {
                        Ok(stored) => stored,
                        Err(message) => {
                            reporter.failed(message);
                            continue;
                        }
                    };
                    // la biblioteca es la que guarda la imagen: el archivo de
                    // trabajo ya no hace falta
                    let _ = fs::remove_file(&finished);
                    reporter.done(&stored, started.elapsed().as_millis());
                }
                Err(message) if message == CANCELLED => reporter.cancelled(),
                Err(message) => reporter.failed(message),
            }
        }
        let _ = app_handle.emit("batch:done", ());
    });

    Ok(())
}

fn kill_sidecar(gpu: &sidecar::Gpu) {
    gpu.cancel.store(true, Ordering::SeqCst);
    let running = gpu.pid.lock().unwrap().take();
    if let Some(pid) = running {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[tauri::command]
fn cancel_batch(app: AppHandle) {
    kill_sidecar(&app.state::<sidecar::Gpu>());
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SourceSettings {
    #[serde(default)]
    brave: String,
    #[serde(default)]
    serpapi: String,
    #[serde(default)]
    searxng: String,
    #[serde(default)]
    disabled: Vec<String>,
}

fn sources_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("sources.json"))
        .map_err(|_| "No se pudo localizar la carpeta de configuración.".to_string())
}

fn load_sources(app: &AppHandle) -> SourceSettings {
    sources_path(app)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn keys_from(settings: &SourceSettings) -> search::Keys {
    search::Keys {
        brave: settings.brave.clone(),
        serpapi: settings.serpapi.clone(),
        searxng: settings.searxng.clone(),
        disabled: settings.disabled.clone(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceInfo {
    id: String,
    name: String,
    needs_key: Option<String>,
    configured: bool,
    enabled: bool,
    signup: String,
    note: String,
}

fn source_infos(settings: &SourceSettings) -> Vec<SourceInfo> {
    let keys = keys_from(settings);
    search::SOURCES
        .iter()
        .map(|spec| SourceInfo {
            id: spec.id.to_string(),
            name: spec.name.to_string(),
            needs_key: match spec.needs {
                search::Need::None => None,
                search::Need::Brave => Some("brave".to_string()),
                search::Need::SerpApi => Some("serpapi".to_string()),
                search::Need::Url => Some("searxng".to_string()),
            },
            configured: keys.configured(spec),
            enabled: keys.enabled(spec.id),
            signup: spec.signup.to_string(),
            note: spec.note.to_string(),
        })
        .collect()
}

#[tauri::command]
fn search_sources(app: AppHandle) -> Vec<SourceInfo> {
    source_infos(&load_sources(&app))
}

#[tauri::command]
fn save_source(
    app: AppHandle,
    id: String,
    key: Option<String>,
    enabled: Option<bool>,
) -> Result<Vec<SourceInfo>, String> {
    let mut settings = load_sources(&app);
    if let Some(value) = key {
        let value = value.trim().to_string();
        match id.as_str() {
            "brave" => settings.brave = value,
            "serpapi" => settings.serpapi = value,
            "searxng" => settings.searxng = value.trim_end_matches('/').to_string(),
            _ => {}
        }
    }
    if let Some(enabled) = enabled {
        settings.disabled.retain(|entry| entry != &id);
        if !enabled {
            settings.disabled.push(id);
        }
    }
    let path = sources_path(&app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "No se pudo crear la carpeta de configuración.".to_string())?;
    }
    let raw = serde_json::to_string_pretty(&settings)
        .map_err(|_| "No se pudo preparar la configuración.".to_string())?;
    fs::write(path, raw).map_err(|_| "No se pudieron guardar las fuentes.".to_string())?;
    Ok(source_infos(&settings))
}

#[tauri::command]
async fn search_images(app: AppHandle, request: search::SearchRequest) -> search::SearchResponse {
    let keys = keys_from(&load_sources(&app));
    search::run(&request, &keys).await
}

/// La Image API de SerpAPI admite 500 KB: se reescala hasta que quepa.
fn upload_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let image = image::open(path)
        .map_err(|error| format!("No se pudo leer la imagen: {error}"))?;
    for longest in [1024u32, 768, 512] {
        let scale = (longest as f32 / image.width().max(image.height()) as f32).min(1.0);
        let resized = image.resize(
            ((image.width() as f32 * scale) as u32).max(1),
            ((image.height() as f32 * scale) as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        );
        for quality in [82u8, 70, 55] {
            let mut buffer = Vec::new();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
            if encoder.encode_image(&resized).is_err() {
                continue;
            }
            if buffer.len() <= 480_000 {
                return Ok(buffer);
            }
        }
    }
    Err("No se pudo comprimir la imagen para la búsqueda inversa.".to_string())
}

#[tauri::command]
async fn reverse_image_search(
    app: AppHandle,
    path: String,
) -> Result<search::SearchResponse, String> {
    let settings = load_sources(&app);
    if settings.serpapi.is_empty() {
        return Err(
            "La búsqueda inversa necesita una clave de SerpAPI (Ajustes → Fuentes).".to_string(),
        );
    }
    let started = Instant::now();
    let bytes = upload_bytes(&PathBuf::from(&path))?;
    let hits = search::adapters::lens(&search::adapters::client(), &settings.serpapi, bytes).await?;
    let state = if hits.is_empty() { "empty" } else { "ok" };
    Ok(search::SearchResponse {
        statuses: vec![search::SourceStatus::new("lens", state, hits.len(), None)],
        hits,
        took_ms: started.elapsed().as_millis() as u64,
    })
}

const MAX_DOWNLOAD: u64 = 40 * 1024 * 1024;

fn safe_name(url: &str) -> String {
    let raw = url.split('?').next().unwrap_or(url).rsplit('/').next().unwrap_or("imagen");
    let stem = raw.split('.').next().unwrap_or("imagen").replace("%20", "-");
    let cleaned: String = stem
        .chars()
        .filter(|char| char.is_alphanumeric() || matches!(char, '-' | '_'))
        .take(60)
        .collect();
    if cleaned.is_empty() {
        "imagen".to_string()
    } else {
        cleaned
    }
}

/// Valida el contenido descargado y lo guarda con un nombre legible.
fn store_download(dir: &Path, url: &str, bytes: &[u8]) -> Result<ImageMeta, String> {
    if bytes.len() as u64 > MAX_DOWNLOAD {
        return Err("La imagen ocupa más de 40 MB.".to_string());
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "No se reconoce el formato de la imagen.".to_string())?;
    let format = reader
        .format()
        .ok_or_else(|| "No se reconoce el formato de la imagen.".to_string())?;
    let ext = match format {
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Png => "png",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::Bmp => "bmp",
        image::ImageFormat::Tiff => "tiff",
        _ => return Err("Ese formato de imagen no está soportado.".to_string()),
    };
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "La imagen descargada está dañada.".to_string())?;

    fs::create_dir_all(dir).map_err(|_| "No se pudo crear la caché de resultados.".to_string())?;
    let stem = safe_name(url);
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut index = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{index}.{ext}"));
        index += 1;
    }
    fs::write(&path, bytes).map_err(|_| "No se pudo guardar la imagen.".to_string())?;

    Ok(ImageMeta {
        path: path.to_string_lossy().to_string(),
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "imagen".to_string()),
        width,
        height,
        size: bytes.len() as u64,
    })
}

#[tauri::command]
async fn download_image(
    app: AppHandle,
    url: String,
    referer: Option<String>,
) -> Result<ImageMeta, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("Sólo se pueden descargar direcciones http o https.".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .user_agent("img-upscaler/0.3")
        .build()
        .map_err(|error| format!("No se pudo preparar la conexión: {error}"))?;

    let mut request = client.get(&url);
    if let Some(referer) = referer.as_deref() {
        if referer.starts_with("http") {
            request = request.header("Referer", referer);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("No se pudo descargar la imagen: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "El servidor respondió {}. Prueba con otra imagen.",
            response.status().as_u16()
        ));
    }
    if let Some(length) = response.content_length() {
        if length > MAX_DOWNLOAD {
            return Err("La imagen ocupa más de 40 MB.".to_string());
        }
    }
    let is_image = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("image/"))
        .unwrap_or(true);
    if !is_image {
        return Err("Esa dirección no devolvió una imagen.".to_string());
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("Descarga incompleta: {error}"))?;

    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "No se pudo localizar la carpeta de datos.".to_string())?
        .join("resultados");
    store_download(&dir, &url, &bytes)
}

#[tauri::command]
fn list_results(app: AppHandle) -> Result<Vec<store::Stored>, String> {
    store::all(&app)
}

#[tauri::command]
fn delete_result(app: AppHandle, id: String) -> Result<Vec<store::Stored>, String> {
    store::remove(&app, &id)?;
    store::all(&app)
}

#[tauri::command]
fn copy_result(app: AppHandle, id: String) -> Result<(), String> {
    store::copy(&app, &id)
}

#[tauri::command]
fn export_result(app: AppHandle, id: String, path: String) -> Result<String, String> {
    let destination = PathBuf::from(&path);
    store::export(&app, &id, &destination)?;
    Ok(destination.to_string_lossy().to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(sidecar::Gpu::default())
        .register_uri_scheme_protocol(store::PROTOCOL, |ctx, request| {
            store::serve(ctx.app_handle(), &request)
        })
        .setup(|app| {
            // deja rastro de cómo se resolvieron binario y modelos
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = fs::create_dir_all(&dir);
                let _ = fs::write(dir.join("resolve.log"), sidecar::report(app.handle()));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_paths,
            start_batch,
            cancel_batch,
            list_results,
            delete_result,
            copy_result,
            export_result,
            search_sources,
            save_source,
            search_images,
            reverse_image_search,
            download_image
        ])
        .run(tauri::generate_context!())
        .expect("error al iniciar la aplicación");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir).unwrap();
        let src = dir.join(name);
        image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, 90])
        })
        .save(&src)
        .unwrap();
        src
    }

    #[test]
    fn lanczos_doubles_dimensions() {
        let dir = std::env::temp_dir().join("img-upscaler-test");
        let src = sample(&dir, "entrada.png", 16, 12);

        let mut ticks = Vec::new();
        let out = upscale_file(&src, &dir, 2, &mut |percent, _| ticks.push(percent)).unwrap();

        assert_eq!(image::image_dimensions(&out).unwrap(), (32, 24));
        assert_eq!(out.file_name().unwrap().to_string_lossy(), "entrada_2x.png");
        assert_eq!(ticks.last().copied(), Some(100));
    }

    #[test]
    fn refuses_absurd_scale() {
        let dir = std::env::temp_dir().join("img-upscaler-test-big");
        let src = sample(&dir, "grande.png", 6000, 4000);

        let err = upscale_file(&src, &dir, 8, &mut |_, _| {}).unwrap_err();
        assert!(err.contains("demasiado grande"));
    }

    #[test]
    fn sidecar_upscales_small_image() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let exe = root.join("binaries/realesrgan-ncnn-vulkan.exe");
        if !exe.is_file() {
            return;
        }

        let dir = std::env::temp_dir().join("img-upscaler-sidecar");
        let src = sample(&dir, "mini.png", 48, 32);
        let dest = dir.join("mini-ia.png");
        let gpu = sidecar::Gpu::default();

        sidecar::run_with(
            &exe,
            &root.join("models"),
            &gpu,
            &sidecar::Job {
                src: &src,
                dest: &dest,
                model: sidecar::MODEL_INTERMEDIO,
                scale: 4,
                tile: 64,
            },
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(image::image_dimensions(&dest).unwrap(), (192, 128));
        assert_eq!(gpu.pid.lock().unwrap().is_none(), true);

        // regresión: ncnn toma `\\?\C:\...` como ruta relativa y falla
        let prefixed = PathBuf::from(format!(r"\\?\{}", root.join("models").display()));
        let dest_unc = dir.join("mini-ia-unc.png");
        sidecar::run_with(
            &exe,
            &prefixed,
            &gpu,
            &sidecar::Job {
                src: &src,
                dest: &dest_unc,
                model: sidecar::MODEL_INTERMEDIO,
                scale: 4,
                tile: 64,
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(dest_unc.is_file());
    }

    /// Los dos niveles de IA llegan al tamaño pedido (el Simple ya está
    /// cubierto por el test de Lanczos).
    #[test]
    fn both_ai_levels_reach_the_requested_size() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let exe = root.join("binaries/realesrgan-ncnn-vulkan.exe");
        let models = root.join("models");
        if !exe.is_file() {
            return;
        }

        let dir = std::env::temp_dir().join("img-upscaler-niveles");
        let src = sample(&dir, "entrada.png", 32, 24);
        let job = JobRequest {
            id: "niveles".to_string(),
            path: src.to_string_lossy().to_string(),
        };
        let gpu = sidecar::Gpu::default();

        for level in ["intermedio", "potente"] {
            let output = ai_pipeline(
                &job,
                2,
                level,
                &dir,
                &gpu,
                &exe,
                &models,
                &mut |_, _| {},
            )
            .unwrap_or_else(|error| panic!("{level}: {error}"));
            assert_eq!(
                image::image_dimensions(&output).unwrap(),
                (64, 48),
                "nivel {level}"
            );
            let _ = fs::remove_file(output);
        }
    }

    #[test]
    fn cancel_kills_sidecar_and_leaves_no_temporaries() {
        use std::sync::Arc;
        use std::time::Duration;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let exe = root.join("binaries/realesrgan-ncnn-vulkan.exe");
        if !exe.is_file() {
            return;
        }
        let models = root.join("models");

        let dir = std::env::temp_dir().join("img-upscaler-cancel");
        let src = sample(&dir, "entrada.png", 1200, 900);

        let gpu = Arc::new(sidecar::Gpu::default());
        let worker = {
            let gpu = Arc::clone(&gpu);
            let src = src.clone();
            let exe = exe.clone();
            let models = models.clone();
            let target = dir.clone();
            thread::spawn(move || {
                let job = JobRequest {
                    id: "cancel-test".to_string(),
                    path: src.to_string_lossy().to_string(),
                };
                ai_pipeline(
                    &job,
                    2,
                    "potente",
                    &target,
                    &gpu,
                    &exe,
                    &models,
                    &mut |_, _| {},
                )
            })
        };

        let mut waited = 0;
        while gpu.pid.lock().unwrap().is_none() && waited < 400 {
            thread::sleep(Duration::from_millis(50));
            waited += 1;
        }
        assert!(
            gpu.pid.lock().unwrap().is_some(),
            "el sidecar no llegó a arrancar"
        );

        kill_sidecar(&gpu);

        let result = worker.join().unwrap();
        assert_eq!(
            result.unwrap_err(),
            CANCELLED,
            "el pipeline no reportó la cancelación"
        );
        assert!(gpu.pid.lock().unwrap().is_none(), "el PID quedó registrado");

        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "entrada.png")
            .collect();
        assert!(
            leftovers.is_empty(),
            "quedaron temporales en la carpeta de salida: {leftovers:?}"
        );
    }

    /// Toca las APIs reales sin clave: Openverse y Wikimedia Commons.
    #[test]
    #[ignore]
    fn live_open_search_returns_hits() {
        let request = search::SearchRequest {
            query: "stone bridge".to_string(),
            page: 0,
            filters: search::Filters::default(),
        };
        let response = tauri::async_runtime::block_on(search::run(&request, &search::Keys::default()));
        println!(
            "statuses: {:?}",
            response
                .statuses
                .iter()
                .map(|status| (&status.id, &status.state, status.count, &status.message))
                .collect::<Vec<_>>()
        );
        println!("hits: {}", response.hits.len());
        assert!(response.hits.len() > 5, "hits={}", response.hits.len());
    }

    #[test]
    fn stores_downloads_with_safe_names() {
        let dir = std::env::temp_dir().join("img-upscaler-descargas");
        let _ = fs::remove_dir_all(&dir);

        let mut buffer = Vec::new();
        image::RgbImage::from_fn(8, 6, |x, y| image::Rgb([x as u8, y as u8, 10]))
            .write_to(&mut std::io::Cursor::new(&mut buffer), image::ImageFormat::Png)
            .unwrap();

        let meta = store_download(&dir, "https://ejemplo.com/fotos/puente%20viejo.png?w=800", &buffer)
            .unwrap();
        assert_eq!((meta.width, meta.height), (8, 6));
        assert!(meta.name.starts_with("puente-viejo"), "{}", meta.name);
        assert!(meta.name.ends_with(".png"), "{}", meta.name);
        assert!(PathBuf::from(&meta.path).is_file());

        let again = store_download(&dir, "https://ejemplo.com/fotos/puente%20viejo.png", &buffer)
            .unwrap();
        assert_ne!(meta.name, again.name, "debe evitar pisar el archivo");

        let error = store_download(&dir, "https://ejemplo.com/no.png", b"no soy una imagen").unwrap_err();
        assert!(!error.is_empty());
    }

    #[test]
    fn refuses_large_outputs_before_running_the_engine() {
        assert!(check_limits(900, 600, 8).is_ok());
        let error = check_limits(3000, 2000, 8).unwrap_err();
        assert!(error.contains("demasiado grande"), "{error}");
    }

    /// Instalación rota (carpeta de modelos vacía) en un 8×: el fallo llega con
    /// el código del motor y no deja temporales.
    #[test]
    fn broken_engine_install_is_reported() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let exe = root.join("binaries/realesrgan-ncnn-vulkan.exe");
        if !exe.is_file() {
            return;
        }
        let models = std::env::temp_dir().join("img-upscaler-sin-modelos");
        let _ = fs::remove_dir_all(&models);
        fs::create_dir_all(&models).unwrap();

        let dir = std::env::temp_dir().join("img-upscaler-silent");
        let src = sample(&dir, "entrada.png", 1400, 1000);
        let job = JobRequest {
            id: "silent".to_string(),
            path: src.to_string_lossy().to_string(),
        };

        let error = ai_pipeline(
            &job,
            8,
            "intermedio",
            &dir,
            &sidecar::Gpu::default(),
            &exe,
            &models,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("El motor de escalado falló"), "{error}");

        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "quedaron temporales: {leftovers:?}");
    }

    /// Genera la muestra comparativa con el mismo pipeline que usa la app:
    /// misma foto y mismo recorte, Intermedio vs Potente vs Lanczos.
    #[test]
    #[ignore]
    fn make_sample_comparison() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let samples = manifest.join("../samples");
        let exe = manifest.join("binaries/realesrgan-ncnn-vulkan.exe");
        let models = manifest.join("models");
        if !exe.is_file() {
            return;
        }

        let base = image::open(samples.join("foto.jpg")).unwrap();
        let source_crop = base.crop_imm(450, 310, 400, 300);
        let source = samples.join("fuente.png");
        source_crop.save(&source).unwrap();

        let gpu = sidecar::Gpu::default();
        let out_dir = samples.to_string_lossy().to_string();
        let (x, y, w, h) = (210u32, 120u32, 380u32, 260u32);

        for (level, file) in [
            ("intermedio", "muestra-intermedio.png"),
            ("potente", "muestra-potente.png"),
        ] {
            let job = JobRequest {
                id: format!("muestra-{level}"),
                path: source.to_string_lossy().to_string(),
            };
            let output = ai_pipeline(
                &job,
                2,
                level,
                Path::new(&out_dir),
                &gpu,
                &exe,
                &models,
                &mut |_, _| {},
            )
            .unwrap();

            image::open(&output)
                .unwrap()
                .crop_imm(x, y, w, h)
                .save(samples.join(file))
                .unwrap();
            let _ = fs::remove_file(&output);
        }

        let lanczos = source_crop.resize_exact(800, 600, image::imageops::FilterType::Lanczos3);
        lanczos
            .crop_imm(x, y, w, h)
            .save(samples.join("muestra-lanczos.png"))
            .unwrap();
    }

    #[test]
    fn parses_sidecar_progress() {
        assert_eq!(sidecar::parse_percent("0.00%\r25.00%\r"), Some(25));
        assert_eq!(sidecar::parse_percent("no hay nada"), None);
        assert_eq!(sidecar::parse_percent("150.00%"), Some(100));
    }
}
