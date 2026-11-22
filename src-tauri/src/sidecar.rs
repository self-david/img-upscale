use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::Duration,
};
use tauri::{AppHandle, Manager};

pub const EXE_NAME: &str = "realesrgan-ncnn-vulkan.exe";

/// Modelos disponibles en `models/`.
pub const MODEL_INTERMEDIO: &str = "realesr-general-x4v3";
pub const MODEL_POTENTE: &str = "realesrgan-x4plus";

/// Estado del trabajo en GPU: permite cancelar matando el proceso.
#[derive(Default)]
pub struct Gpu {
    pub cancel: AtomicBool,
    pub pid: Mutex<Option<u32>>,
}

pub struct Job<'a> {
    pub src: &'a Path,
    pub dest: &'a Path,
    pub model: &'a str,
    pub scale: u32,
    pub tile: u32,
}

/// ncnn trata `\\?\C:\...` como ruta relativa y la cuelga del directorio del exe.
/// Hay que pasarle rutas absolutas normales.
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let stripped = text
        .strip_prefix(r"\\?\")
        .or_else(|| text.strip_prefix("//?/"))
        .unwrap_or(&text);
    PathBuf::from(stripped)
}

/// Carpetas donde pueden vivir los recursos, en orden de preferencia.
fn roots(app: &AppHandle) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = app.path().resource_dir() {
        dirs.push(plain(&dir));
    }
    dirs.push(plain(&PathBuf::from(env!("CARGO_MANIFEST_DIR"))));
    dirs
}

fn locate(app: &AppHandle, relative: &str, probe: &str) -> PathBuf {
    let dirs = roots(app);
    for root in &dirs {
        let candidate = root.join(relative);
        if candidate.join(probe).is_file() {
            return candidate;
        }
    }
    dirs.first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(relative)
}

/// Resumen de la resolución de recursos, para diagnosticar instalaciones raras.
pub fn report(app: &AppHandle) -> String {
    let exe = exe(app);
    let models = models_dir(app);
    format!(
        "exe={} (existe={})\nmodels={} (existe={})\n",
        exe.display(),
        exe.is_file(),
        models.display(),
        models.is_dir()
    )
}

pub fn exe(app: &AppHandle) -> PathBuf {
    locate(app, "binaries", EXE_NAME).join(EXE_NAME)
}

pub fn models_dir(app: &AppHandle) -> PathBuf {
    locate(app, "models", "realesr-general-x4v3.param")
}

/// Ruta de un modelo concreto, buscando en cada carpeta candidata.
pub fn model_file(app: &AppHandle, name: &str) -> PathBuf {
    locate(app, "models", name).join(name)
}

/// El binario imprime el avance por tiles como `  37.50%` separado por CR.
pub fn parse_percent(chunk: &str) -> Option<u8> {
    chunk
        .split(['\r', '\n'])
        .filter_map(|line| line.trim().strip_suffix('%'))
        .filter_map(|value| value.trim().parse::<f32>().ok())
        .last()
        .map(|value| value.clamp(0.0, 100.0).round() as u8)
}

/// Lanza una pasada del sidecar y traduce su progreso. El proceso se guarda
/// por PID para poder matarlo desde `cancel_batch`.
pub fn run_with(
    exe: &Path,
    models: &Path,
    gpu: &Gpu,
    job: &Job,
    on_progress: &mut dyn FnMut(u8),
) -> Result<String, String> {
    let exe = &plain(exe);
    let models = &plain(models);
    let src = &plain(job.src);
    let dest = &plain(job.dest);
    if !exe.is_file() {
        return Err(format!(
            "No se encontró el motor de escalado con IA (realesrgan-ncnn-vulkan) en {}.",
            exe.display()
        ));
    }
    if !models.is_dir() {
        return Err("No se encontró la carpeta de modelos de IA.".into());
    }

    let log_path = std::env::temp_dir().join(format!(
        "upscaler-sidecar-{}.log",
        std::process::id()
    ));
    let stdout = fs::File::create(&log_path).map_err(|_| "No se pudo preparar la salida del motor.".to_string())?;
    let stderr = stdout
        .try_clone()
        .map_err(|_| "No se pudo preparar la salida del motor.".to_string())?;

    let mut child = Command::new(exe)
        .arg("-i")
        .arg(src)
        .arg("-o")
        .arg(dest)
        .arg("-n")
        .arg(job.model)
        .arg("-m")
        .arg(models)
        .arg("-s")
        .arg(job.scale.to_string())
        .arg("-t")
        .arg(job.tile.to_string())
        .arg("-f")
        .arg("png")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|_| "No se pudo iniciar el motor de escalado con IA.".to_string())?;

    *gpu.pid.lock().unwrap() = Some(child.id());

    let mut offset = 0u64;
    let status = loop {
        thread::sleep(Duration::from_millis(120));

        if let Ok(mut file) = fs::File::open(&log_path) {
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let mut chunk = String::new();
                if file.read_to_string(&mut chunk).is_ok() && !chunk.is_empty() {
                    offset += chunk.len() as u64;
                    if let Some(percent) = parse_percent(&chunk) {
                        on_progress(percent);
                    }
                }
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => continue,
            Err(_) => {
                let _ = child.kill();
                let _ = fs::remove_file(&log_path);
                return Err("El motor de escalado dejó de responder.".into());
            }
        }
    };

    *gpu.pid.lock().unwrap() = None;

    // última lectura: si el proceso terminó entre dos sondeos, quedan líneas sin leer
    if let Ok(mut file) = fs::File::open(&log_path) {
        if file.seek(SeekFrom::Start(offset)).is_ok() {
            let mut chunk = String::new();
            if file.read_to_string(&mut chunk).is_ok() {
                if let Some(percent) = parse_percent(&chunk) {
                    on_progress(percent);
                }
            }
        }
    }

    let payload = fs::read_to_string(&log_path).unwrap_or_default();
    let _ = fs::remove_file(&log_path);

    if gpu.cancel.load(Ordering::SeqCst) {
        return Err("CANCELLED".to_string());
    }
    verdict(status, &payload, dest)
}

/// Código 0 no basta: el motor también sale así cuando no escribe nada.
fn verdict(status: std::process::ExitStatus, payload: &str, dest: &Path) -> Result<String, String> {
    if !status.success() {
        return Err(format!(
            "El motor de escalado falló (código {}): {}",
            status.code().unwrap_or(-1),
            last_message(payload)
        ));
    }
    if fs::metadata(dest).map(|meta| meta.len()).unwrap_or(0) == 0 {
        return Err(format!(
            "El motor de IA no generó la imagen ({}). Prueba un factor menor o el nivel Simple.",
            last_message(payload)
        ));
    }
    Ok(payload.to_string())
}

/// Última línea útil del motor (las de progreso son solo porcentajes).
pub fn last_message(payload: &str) -> &str {
    payload
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.ends_with('%'))
        .last()
        .unwrap_or("sin detalle")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_windows_extended_prefix() {
        assert_eq!(
            plain(Path::new(r"\\?\C:\fotos\models")),
            PathBuf::from(r"C:\fotos\models")
        );
        assert_eq!(
            plain(Path::new("//?/C:/fotos/models")),
            PathBuf::from("C:/fotos/models")
        );
        let normal = Path::new(r"C:\fotos\models");
        assert_eq!(plain(normal), normal);
    }

    /// El caso que producía «No se pudo leer el resultado»: el motor sale con
    /// código 0 y no escribe nada (`_wfopen ... failed`).
    #[cfg(windows)]
    #[test]
    fn success_without_output_is_an_error() {
        use std::os::windows::process::ExitStatusExt;

        let missing = std::env::temp_dir().join("upscaler-no-generado.png");
        let _ = fs::remove_file(&missing);
        let error = verdict(
            std::process::ExitStatus::from_raw(0),
            "0.00%\n_wfopen C:\\fotos\\models/x.param failed",
            &missing,
        )
        .unwrap_err();
        assert!(error.contains("no generó la imagen"), "{error}");
        assert!(error.contains("_wfopen"), "falta el detalle del motor: {error}");

        let written = std::env::temp_dir().join("upscaler-si-generado.png");
        fs::write(&written, b"png").unwrap();
        assert!(verdict(std::process::ExitStatus::from_raw(0), "", &written).is_ok());
        let _ = fs::remove_file(written);
    }
}
