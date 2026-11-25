mod bg;
mod search;
mod sidecar;
mod store;

use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Mutex},
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
/// El codificador JPEG de la librería usa 75 por defecto: para escalados eso
/// tira calidad, así que la app guarda a 90 salvo que el usuario diga otra cosa.
const DEFAULT_JPEG_QUALITY: u8 = 90;

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

/// Submuestreo de color del JPEG. 4:4:4 guarda el color completo; 4:2:0 es el
/// de las cámaras y pesa menos.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chroma {
    Full,
    Sub420,
}

/// Formato del archivo final: el de la imagen original o WebP con pérdida.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Auto,
    Webp,
}

#[derive(Clone, Copy)]
pub struct Output {
    quality: u8,
    chroma: Chroma,
    target: Target,
}

fn parse_chroma(value: Option<String>) -> Result<Chroma, String> {
    match value.as_deref() {
        None | Some("420") => Ok(Chroma::Sub420),
        Some("444") => Ok(Chroma::Full),
        Some(other) => Err(format!("Submuestreo de color desconocido: {other}")),
    }
}

fn parse_target(value: Option<String>) -> Result<Target, String> {
    match value.as_deref() {
        None | Some("auto") => Ok(Target::Auto),
        Some("webp") => Ok(Target::Webp),
        Some(other) => Err(format!("Formato de salida desconocido: {other}")),
    }
}

fn output_format(src: &Path, target: Target) -> (&'static str, image::ImageFormat) {
    if target == Target::Webp {
        return ("webp", image::ImageFormat::WebP);
    }
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

fn read_error() -> String {
    "No se pudo leer la imagen: puede estar dañada o en un formato no soportado.".to_string()
}

/// Decodifica aplicando la orientación EXIF. Las fotos verticales de móvil
/// guardan la rotación en los metadatos: sin esto se escalan tumbadas.
fn open_oriented(path: &Path) -> Result<image::DynamicImage, String> {
    let file = fs::File::open(path).map_err(|_| read_error())?;
    let reader = image::ImageReader::new(std::io::BufReader::new(file))
        .with_guessed_format()
        .map_err(|_| read_error())?;
    let mut decoder = reader.into_decoder().map_err(|_| read_error())?;
    let orientation = image::ImageDecoder::orientation(&mut decoder).map_err(|_| read_error())?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|_| read_error())?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// JPEG progresivo con tablas Huffman optimizadas: mismo aspecto y menos peso
/// que el codificador baseline de `image`.
fn write_jpeg(
    image: &image::DynamicImage,
    path: &Path,
    quality: u8,
    chroma: Chroma,
) -> Result<(), String> {
    let failed = || "No se pudo guardar la imagen resultante.".to_string();
    let converted;
    let rgb = match image {
        image::DynamicImage::ImageRgb8(buffer) => buffer,
        other => {
            converted = other.to_rgb8();
            &converted
        }
    };
    // el codificador progresivo trabaja con dimensiones de 16 bits
    if rgb.width() > u16::MAX as u32 || rgb.height() > u16::MAX as u32 {
        return image
            .save_with_format(path, image::ImageFormat::Jpeg)
            .map_err(|_| failed());
    }
    let mut encoder =
        jpeg_encoder::Encoder::new_file(path, quality).map_err(|_| failed())?;
    encoder.set_progressive(true);
    encoder.set_optimized_huffman_tables(true);
    encoder.set_sampling_factor(match chroma {
        Chroma::Full => jpeg_encoder::SamplingFactor::F_1_1,
        Chroma::Sub420 => jpeg_encoder::SamplingFactor::F_2_2,
    });
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width() as u16,
            rgb.height() as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|_| failed())
}

/// WebP con pérdida (libwebp). Mantiene la resolución y la transparencia, y
/// suele pesar bastante menos que el JPEG o el PNG equivalentes.
fn write_webp(image: &image::DynamicImage, path: &Path, quality: u8) -> Result<(), String> {
    let converted;
    let rgba = match image {
        image::DynamicImage::ImageRgba8(buffer) => buffer,
        other => {
            converted = other.to_rgba8();
            &converted
        }
    };
    let encoder = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height());
    let memory = encoder.encode(quality as f32);
    fs::write(path, &*memory)
        .map_err(|_| "No se pudo guardar la imagen resultante.".to_string())
}

/// Recomprime un PNG sin pérdida (oxipng). Si el archivo es enorme o tarda
/// demasiado se queda como estaba: ahorrar peso no puede costar el archivo.
fn optimize_png(path: &Path) {
    let mut options = oxipng::Options::from_preset(2);
    options.timeout = Some(std::time::Duration::from_secs(10));
    options.max_decompressed_size = Some(200 * 1024 * 1024);
    let Ok(original) = fs::read(path) else {
        return;
    };
    let Ok(optimized) = oxipng::optimize_from_memory(&original, &options) else {
        return;
    };
    if optimized.len() < original.len() && fs::write(path, &optimized).is_err() {
        let _ = fs::write(path, &original);
    }
}

fn write_image(
    image: image::DynamicImage,
    destination: &Path,
    ext: &str,
    format: image::ImageFormat,
    out: Output,
) -> Result<(), String> {
    let temporary = destination.with_extension(format!("{ext}.tmp"));
    let failed = || "No se pudo guardar la imagen resultante.".to_string();
    let prepared = prepare_for_format(image, format);

    let saved = match format {
        image::ImageFormat::Jpeg => {
            write_jpeg(&prepared, &temporary, out.quality, out.chroma).is_ok()
        }
        image::ImageFormat::WebP => write_webp(&prepared, &temporary, out.quality).is_ok(),
        _ => {
            let written = prepared.save_with_format(&temporary, format).is_ok();
            if written && format == image::ImageFormat::Png {
                optimize_png(&temporary);
            }
            written
        }
    };

    if !saved {
        let _ = fs::remove_file(&temporary);
        return Err(failed());
    }
    fs::rename(&temporary, destination).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        "No se pudo finalizar el archivo; comprueba que la carpeta no esté en uso.".to_string()
    })
}

/// Escala una imagen con Lanczos y la guarda. Devuelve la ruta del resultado.
pub fn upscale_file(
    src: &Path,
    target_dir: &Path,
    scale: u32,
    out: Output,
    on_progress: &mut dyn FnMut(u8, &str),
) -> Result<PathBuf, String> {
    on_progress(10, "Leyendo");
    let image = open_oriented(src)?;

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
    let (ext, format) = output_format(src, out.target);
    let destination = unique_output_path(target_dir, &stem, scale, ext);
    write_image(resized, &destination, ext, format, out)?;

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
    out: Output,
    target: &Path,
    reporter: &Reporter,
) -> Result<PathBuf, String> {
    let src = PathBuf::from(&job.path);
    upscale_file(&src, target, scale, out, &mut |percent, stage| {
        reporter.progress(percent, stage)
    })
}

/// Intentos cuando el motor falla por memoria. El primero deja que elija el
/// mosaico según la VRAM libre real; los siguientes lo bajan y reducen los
/// hilos de carga/proceso/guardado, que también gastan memoria.
/// `(mosaico, pocos hilos)`: mosaico 0 = automático.
pub fn ai_attempts() -> Vec<(u32, bool)> {
    vec![(0, false), (128, true), (64, true), (32, true)]
}

/// Pipeline de IA sin depender de la ventana: recibe rutas del motor y un
/// callback de progreso, así se puede probar de extremo a extremo.
#[allow(clippy::too_many_arguments)]
fn ai_pipeline(
    job: &JobRequest,
    scale: u32,
    level: &str,
    out: Output,
    device: Option<i32>,
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
    let decoded = open_oriented(&src)?;
    let (width, height) = (decoded.width(), decoded.height());
    let (out_w, out_h) = (width * scale, height * scale);
    check_limits(width, height, scale)?;

    fs::create_dir_all(target_dir)
        .map_err(|_| "No se pudo crear la carpeta de salida.".to_string())?;

    let stem = src
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "imagen".to_string());
    let (ext, format) = output_format(&src, out.target);
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

    let model = if level == "intermedio" {
        sidecar::MODEL_INTERMEDIO
    } else {
        sidecar::MODEL_POTENTE
    };

    progress(6, "Escalando con IA");
    let attempts = ai_attempts();
    let mut last_error = String::new();
    let mut scaled = false;
    for (index, (tile, low_memory)) in attempts.iter().copied().enumerate() {
        if index > 0 {
            progress(6, &format!("Reintentando (mosaico {tile})"));
        }
        match sidecar::run_with(
            exe,
            models,
            gpu,
            &sidecar::Job {
                src: &engine_input,
                dest: &first,
                model,
                scale: 4,
                tile,
                device,
                threads: if low_memory { Some("1:1:1") } else { None },
            },
            &mut |percent| progress(6 + (percent as u16 * 74 / 100) as u8, "Escalando con IA"),
        ) {
            Ok(_) => {
                scaled = true;
                break;
            }
            // cancelar o una instalación rota no mejoran con otro mosaico
            Err(message) if message == CANCELLED || message.starts_with("No se encontró") => {
                discard();
                return Err(message);
            }
            Err(message) => {
                let _ = fs::remove_file(&first);
                last_error = message;
            }
        }
    }
    if !scaled {
        discard();
        return Err(if attempts.len() > 1 {
            format!(
                "{last_error} Se probó también con mosaicos más pequeños; si se repite, usa un factor menor o el nivel Simple."
            )
        } else {
            last_error
        });
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
    write_image(produced, &destination, ext, format, out)?;

    Ok(destination)
}

/// La sesión de segmentación se carga una vez por proceso: crear una por
/// imagen gastaba memoria y fallaba del segundo trabajo en adelante.
#[derive(Default)]
struct Segmenter(Mutex<Option<bg::Segmenter>>);

impl Segmenter {
    fn with<R>(&self, app: &AppHandle, run: impl FnOnce(&mut bg::Segmenter) -> R) -> Result<R, String> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| "El modelo de fondo quedó bloqueado.".to_string())?;
        if guard.is_none() {
            *guard = Some(bg::Segmenter::load(&sidecar::model_file(
                app,
                "u2netp.onnx",
            ))?);
        }
        let segmenter = guard
            .as_mut()
            .ok_or_else(|| "No se pudo cargar el modelo de fondo.".to_string())?;
        Ok(run(segmenter))
    }
}

/// Paso final opcional: recorta el fondo del resultado ya escalado y se queda
/// solo con el PNG con transparencia.
fn cutout_result(app: &AppHandle, reporter: &Reporter, path: &Path) -> Result<PathBuf, String> {
    reporter.progress(90, "Quitando el fondo");
    let image = image::open(path)
        .map_err(|error| format!("No se pudo leer el resultado para quitar el fondo ({error})."))?
        .to_rgb8();
    let mask = app.state::<Segmenter>().with(app, |model| model.mask(&image))??;
    let target = bg::output_path(path, path.parent().unwrap_or(Path::new(".")));
    bg::cutout(&image, &mask, &target)?;
    optimize_png(&target);
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
    out: Output,
    device: Option<i32>,
    target: &Path,
) -> Result<PathBuf, String> {
    ai_pipeline(
        job,
        scale,
        level,
        out,
        device,
        target,
        gpu,
        &sidecar::exe(app),
        &sidecar::models_dir(app),
        &mut |percent, stage| reporter.progress(percent, stage),
    )
}

/// GPUs detectadas; se sondean una sola vez por ejecución.
#[derive(Default)]
struct Gpus(Mutex<Option<Vec<sidecar::Device>>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceInfo {
    id: i32,
    name: String,
    best: bool,
}

fn probe_gpus(app: &AppHandle) -> Vec<sidecar::Device> {
    let Ok(dir) = workspace_dir(app) else {
        return Vec::new();
    };
    let probe = dir.join("sonda.png");
    if !probe.is_file() {
        let sample = image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 30) as u8, (y * 30) as u8, 90])
        });
        let _ = sample.save(&probe);
    }
    sidecar::devices(&sidecar::exe(app), &sidecar::models_dir(app), &probe)
}

fn detect_gpus(app: &AppHandle) -> Vec<sidecar::Device> {
    let state = app.state::<Gpus>();
    if let Ok(cached) = state.0.lock() {
        if let Some(list) = cached.as_ref() {
            return list.clone();
        }
    }
    let list = probe_gpus(app);
    if let Ok(mut cached) = state.0.lock() {
        *cached = Some(list.clone());
    }
    list
}

#[tauri::command]
fn gpu_devices(app: AppHandle) -> Vec<DeviceInfo> {
    let devices = detect_gpus(&app);
    let best = sidecar::best_device(&devices);
    devices
        .into_iter()
        .map(|device| DeviceInfo {
            best: Some(device.index) == best,
            id: device.index,
            name: device.name,
        })
        .collect()
}

#[tauri::command]
fn start_batch(
    app: AppHandle,
    jobs: Vec<JobRequest>,
    scale: u32,
    level: String,
    remove_bg: Option<bool>,
    quality: Option<u8>,
    device: Option<i32>,
    chroma: Option<String>,
    format: Option<String>,
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
    let device = match device {
        Some(value) => {
            // el motor sólo acepta índices de GPU; no hay modo CPU
            if !(0..=15).contains(&value) {
                return Err("Dispositivo de escalado no válido.".to_string());
            }
            Some(value)
        }
        // sin elección explícita se usa la mejor tarjeta del equipo
        None => sidecar::best_device(&detect_gpus(&app)),
    };
    let out = Output {
        quality: quality.unwrap_or(DEFAULT_JPEG_QUALITY).clamp(1, 100),
        chroma: parse_chroma(chroma)?,
        target: parse_target(format)?,
    };
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
                run_simple_job(&job, scale, out, &target, &reporter)
            } else {
                run_ai_job(
                    &app_handle, &gpu, &reporter, &job, scale, &level, out, device, &target,
                )
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
    let image = open_oriented(path).map_err(|error| format!("No se pudo leer la imagen: {error}"))?;
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

/// Guarda varias imágenes de la biblioteca en una carpeta elegida.
#[tauri::command]
fn export_results(app: AppHandle, ids: Vec<String>, folder: String) -> Result<usize, String> {
    store::export_many(&app, &ids, &PathBuf::from(folder))
}

#[tauri::command]
fn load_session(app: AppHandle) -> Result<store::Session, String> {
    store::session(&app)
}

#[tauri::command]
fn save_session(
    app: AppHandle,
    settings: store::Settings,
    queue: Vec<store::QueuedItem>,
) -> Result<(), String> {
    store::save_session(&app, &settings, &queue)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(sidecar::Gpu::default())
        .manage(Gpus::default())
        .manage(Segmenter::default())
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
            export_results,
            load_session,
            save_session,
            gpu_devices,
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

    fn output_with(quality: u8) -> Output {
        Output {
            quality,
            chroma: Chroma::Sub420,
            target: Target::Auto,
        }
    }

    fn output() -> Output {
        output_with(DEFAULT_JPEG_QUALITY)
    }

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
        let out = upscale_file(&src, &dir, 2, output(), &mut |percent, _| {
            ticks.push(percent)
        })
        .unwrap();

        assert_eq!(image::image_dimensions(&out).unwrap(), (32, 24));
        assert_eq!(out.file_name().unwrap().to_string_lossy(), "entrada_2x.png");
        assert_eq!(ticks.last().copied(), Some(100));
    }

    #[test]
    fn refuses_absurd_scale() {
        let dir = std::env::temp_dir().join("img-upscaler-test-big");
        let src = sample(&dir, "grande.png", 6000, 4000);

        let err = upscale_file(&src, &dir, 8, output(), &mut |_, _| {}).unwrap_err();
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
                device: None,
                threads: None,
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
                device: None,
                threads: None,
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
                output(),
                None,
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
                    output(),
                    None,
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
            output(),
            None,
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
                output(),
                None,
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

    /// Un JPEG con una única etiqueta EXIF (0x0112 Orientation) delante de los
    /// datos del codificador, para comprobar que se aplica la rotación.
    fn jpeg_with_orientation(dir: &Path, value: u16) -> PathBuf {
        let mut buffer = Vec::new();
        image::RgbImage::from_fn(40, 20, |x, y| image::Rgb([(x * 6) as u8, (y * 12) as u8, 70]))
            .write_to(&mut std::io::Cursor::new(&mut buffer), image::ImageFormat::Jpeg)
            .unwrap();

        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(b"Exif\0\0");
        payload.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
        payload.extend_from_slice(&[0x01, 0x00]);
        payload.extend_from_slice(&[0x12, 0x01, 0x03, 0x00]);
        payload.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        payload.extend_from_slice(&value.to_le_bytes());
        payload.extend_from_slice(&[0x00, 0x00]);
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut out = Vec::new();
        out.extend_from_slice(&buffer[..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&buffer[2..]);

        let path = dir.join(format!("orientacion-{value}.jpg"));
        fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn applies_exif_orientation() {
        let dir = std::env::temp_dir().join("img-upscaler-exif");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let normal = open_oriented(&jpeg_with_orientation(&dir, 1)).unwrap();
        assert_eq!((normal.width(), normal.height()), (40, 20));

        // 6 = girar 90° en sentido horario
        let rotated = open_oriented(&jpeg_with_orientation(&dir, 6)).unwrap();
        assert_eq!((rotated.width(), rotated.height()), (20, 40));
    }

    #[test]
    fn jpeg_quality_changes_the_saved_file() {
        let dir = std::env::temp_dir().join("img-upscaler-calidad");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let source = image::RgbImage::from_fn(180, 120, |x, y| {
            image::Rgb([
                ((x * 37 + y * 11) % 256) as u8,
                ((x * 13 + y * 71) % 256) as u8,
                ((x * 5 + y * 3) % 256) as u8,
            ])
        });

        let high = dir.join("alta.jpg");
        let low = dir.join("baja.jpg");
        write_image(
            image::DynamicImage::ImageRgb8(source.clone()),
            &high,
            "jpg",
            image::ImageFormat::Jpeg,
            output_with(95),
        )
        .unwrap();
        write_image(
            image::DynamicImage::ImageRgb8(source),
            &low,
            "jpg",
            image::ImageFormat::Jpeg,
            output_with(50),
        )
        .unwrap();

        let high_size = fs::metadata(&high).unwrap().len();
        let low_size = fs::metadata(&low).unwrap().len();
        assert!(high_size > low_size, "calidad 95 ({high_size}) debería pesar más que 50 ({low_size})");
        assert_eq!(image::image_dimensions(&high).unwrap(), (180, 120));
        assert_eq!(image::image_dimensions(&low).unwrap(), (180, 120));
    }

    /// SOF2 (0xFFC2) es el marcador de un JPEG progresivo.
    fn is_progressive_jpeg(bytes: &[u8]) -> bool {
        bytes.windows(2).any(|pair| pair == [0xFF, 0xC2])
    }

    #[test]
    fn jpeg_output_is_progressive_and_valid() {
        let dir = std::env::temp_dir().join("img-upscaler-progresivo");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("foto.jpg");
        let source = image::RgbImage::from_fn(120, 80, |x, y| {
            image::Rgb([((x * 7) % 256) as u8, ((y * 11) % 256) as u8, 60])
        });

        write_image(
            image::DynamicImage::ImageRgb8(source),
            &target,
            "jpg",
            image::ImageFormat::Jpeg,
            output_with(90),
        )
        .unwrap();

        let bytes = fs::read(&target).unwrap();
        assert!(is_progressive_jpeg(&bytes), "debería ser un JPEG progresivo");
        assert_eq!(image::image_dimensions(&target).unwrap(), (120, 80));
    }

    #[test]
    #[ignore]
    fn mide_ahorro_real() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dir = std::env::temp_dir().join("img-upscaler-ahorro");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let base = image::open(root.join("../samples/foto.jpg")).unwrap();
        let up = base.resize_exact(
            base.width() * 2,
            base.height() * 2,
            image::imageops::FilterType::Lanczos3,
        );

        // misma calidad (90) y mismas dimensiones: sólo cambia el codificador
        let baseline = dir.join("baseline.jpg");
        {
            let file = fs::File::create(&baseline).unwrap();
            let mut writer = std::io::BufWriter::new(file);
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 90);
            encoder.encode_image(&up).unwrap();
            std::io::Write::flush(&mut writer).unwrap();
        }
        let progressive = dir.join("progresivo.jpg");
        write_image(up.clone(), &progressive, "jpg", image::ImageFormat::Jpeg, output_with(90))
            .unwrap();


        let raw_png = dir.join("sin-oxipng.png");
        up.save_with_format(&raw_png, image::ImageFormat::Png).unwrap();
        let optimized = dir.join("con-oxipng.png");
        write_image(up.clone(), &optimized, "png", image::ImageFormat::Png, output_with(90))
            .unwrap();

        let webp = dir.join("calidad-90.webp");
        write_image(
            up.clone(),
            &webp,
            "webp",
            image::ImageFormat::WebP,
            Output {
                quality: 90,
                chroma: Chroma::Sub420,
                target: Target::Webp,
            },
        )
        .unwrap();

        let alta = dir.join("calidad-95.jpg");
        write_image(up, &alta, "jpg", image::ImageFormat::Jpeg, output_with(95)).unwrap();
        println!("jpeg 95: {} bytes", fs::metadata(&alta).unwrap().len());

        for (label, a, b) in [
            ("jpeg", &baseline, &progressive),
            ("png", &raw_png, &optimized),
            ("webp vs jpeg", &progressive, &webp),
        ] {
            let before = fs::metadata(a).unwrap().len();
            let after = fs::metadata(b).unwrap().len();
            println!(
                "{label}: {before} -> {after} bytes ({:.1}% menos)",
                (before as f64 - after as f64) / before as f64 * 100.0
            );
        }
    }

    #[test]
    fn png_output_stays_valid_after_optimizing() {
        let dir = std::env::temp_dir().join("img-upscaler-oxipng");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("icono.png");
        let source = image::RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([(x * 3) as u8, (y * 3) as u8, 40, 255])
        });

        write_image(
            image::DynamicImage::ImageRgba8(source),
            &target,
            "png",
            image::ImageFormat::Png,
            output_with(90),
        )
        .unwrap();

        let decoded = image::open(&target).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
        assert!(fs::metadata(&target).unwrap().len() > 0);
    }

    fn foto_cotidiana() -> image::RgbImage {
        image::RgbImage::from_fn(200, 150, |x, y| {
            image::Rgb([
                ((x * 13 + y * 7) % 256) as u8,
                ((x * 3 + y * 29) % 256) as u8,
                ((x + y) * 5 % 256) as u8,
            ])
        })
    }

    #[test]
    fn chroma_420_weighs_less_than_444() {
        let dir = std::env::temp_dir().join("img-upscaler-chroma");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let full = dir.join("444.jpg");
        let sub = dir.join("420.jpg");
        write_image(
            image::DynamicImage::ImageRgb8(foto_cotidiana()),
            &full,
            "jpg",
            image::ImageFormat::Jpeg,
            Output {
                quality: 90,
                chroma: Chroma::Full,
                target: Target::Auto,
            },
        )
        .unwrap();
        write_image(
            image::DynamicImage::ImageRgb8(foto_cotidiana()),
            &sub,
            "jpg",
            image::ImageFormat::Jpeg,
            Output {
                quality: 90,
                chroma: Chroma::Sub420,
                target: Target::Auto,
            },
        )
        .unwrap();

        let full_size = fs::metadata(&full).unwrap().len();
        let sub_size = fs::metadata(&sub).unwrap().len();
        assert!(sub_size < full_size, "4:2:0 ({sub_size}) debería pesar menos que 4:4:4 ({full_size})");
        assert_eq!(image::image_dimensions(&sub).unwrap(), (200, 150));
    }

    #[test]
    fn webp_output_is_lossy_and_keeps_resolution() {
        let dir = std::env::temp_dir().join("img-upscaler-webp");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let webp = dir.join("foto.webp");
        write_image(
            image::DynamicImage::ImageRgb8(foto_cotidiana()),
            &webp,
            "webp",
            image::ImageFormat::WebP,
            Output {
                quality: 90,
                chroma: Chroma::Sub420,
                target: Target::Webp,
            },
        )
        .unwrap();

        let bytes = fs::read(&webp).unwrap();
        assert_eq!(&bytes[..4], &b"RIFF"[..], "no es un contenedor RIFF");
        assert_eq!(&bytes[8..12], &b"WEBP"[..], "no es un WebP");
        // «VP8 » es el códec con pérdida; «VP8L» sería sin pérdida
        assert_eq!(&bytes[12..16], &b"VP8 "[..], "debería estar comprimido con pérdida");
        assert_eq!(image::image_dimensions(&webp).unwrap(), (200, 150));
    }

    #[test]
    fn output_format_follows_the_request() {
        let jpg = Path::new("foto.jpg");
        assert_eq!(output_format(jpg, Target::Auto).0, "jpg");
        assert_eq!(output_format(jpg, Target::Webp).0, "webp");
        assert!(parse_target(Some("webp".to_string())).unwrap() == Target::Webp);
        assert!(parse_target(None).unwrap() == Target::Auto);
        assert!(parse_target(Some("avif".to_string())).is_err());
        assert!(parse_chroma(Some("444".to_string())).unwrap() == Chroma::Full);
        assert!(parse_chroma(None).unwrap() == Chroma::Sub420);
    }

    #[test]
    fn retries_start_automatic_and_shrink() {
        let attempts = ai_attempts();
        assert_eq!(
            attempts.first(),
            Some(&(0, false)),
            "el primer intento deja el mosaico al motor"
        );
        assert_eq!(
            attempts.last().map(|(tile, low)| (*tile, *low)),
            Some((32, true)),
            "el último intento baja el mosaico y los hilos"
        );
        let tiles: Vec<u32> = attempts.iter().map(|(tile, _)| *tile).collect();
        assert!(tiles.windows(2).all(|pair| pair[1] < pair[0] || pair[0] == 0));
        assert!(attempts.iter().skip(1).all(|(_, low)| *low));
    }
}
