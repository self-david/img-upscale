//! Biblioteca de resultados en SQLite: las imágenes escaladas se guardan aquí
//! y se sirven al frontend con el protocolo `appimg`.

use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use image::{DynamicImage, GenericImageView, ImageFormat};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Manager};

pub const PROTOCOL: &str = "appimg";
const THUMB_SIDE: u32 = 480;
/// Un RGBA de más de 64 MP no cabe con holgura en el portapapeles.
const MAX_CLIPBOARD_PIXELS: u64 = 64_000_000;
const SCHEMA: &str = "
    PRAGMA journal_mode=WAL;
    CREATE TABLE IF NOT EXISTS results (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        format TEXT NOT NULL,
        level TEXT NOT NULL,
        scale INTEGER NOT NULL,
        width INTEGER NOT NULL,
        height INTEGER NOT NULL,
        bytes BLOB NOT NULL,
        thumb BLOB NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS results_created ON results (created_at DESC);
";

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Stored {
    pub id: String,
    pub name: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub level: String,
    pub size: u64,
    pub created_at: i64,
}

/// Ruta de la base de datos, creando la carpeta de datos si hace falta.
pub fn library(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "No se pudo localizar la carpeta de datos.".to_string())?;
    fs::create_dir_all(&dir).map_err(|_| "No se pudo crear la carpeta de datos.".to_string())?;
    Ok(dir.join("biblioteca.db"))
}

fn connect(db: &Path) -> Result<Connection, String> {
    let conn = Connection::open(db)
        .map_err(|error| format!("No se pudo abrir la biblioteca: {error}"))?;
    let _ = conn.busy_timeout(Duration::from_secs(5));
    conn.execute_batch(SCHEMA)
        .map_err(|error| format!("No se pudo preparar la biblioteca: {error}"))?;
    Ok(conn)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn new_id() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{stamp:x}{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn format_of(path: &Path) -> String {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "jpg",
        Some("webp") => "webp",
        Some("gif") => "gif",
        Some("bmp") => "bmp",
        Some("tif") | Some("tiff") => "tiff",
        _ => "png",
    }
    .to_string()
}

fn mime_of(format: &str) -> &'static str {
    match format {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tiff" => "image/tiff",
        _ => "image/png",
    }
}

fn thumb_bytes(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let small = image.thumbnail(THUMB_SIDE, THUMB_SIDE);
    let mut out = Vec::new();
    small
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|_| "No se pudo preparar la miniatura.".to_string())?;
    Ok(out)
}

/// Evita que dos resultados compartan nombre en la biblioteca.
fn unique_name(conn: &Connection, path: &Path) -> Result<String, String> {
    let base = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "imagen.png".to_string());
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (base.clone(), String::new()),
    };

    let mut candidate = base;
    let mut index = 2;
    loop {
        let taken: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM results WHERE name = ?1",
                [&candidate],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("No se pudo comprobar la biblioteca: {error}"))?;
        if taken.is_none() {
            return Ok(candidate);
        }
        candidate = format!("{stem}-{index}{ext}");
        index += 1;
    }
}

fn add_at(db: &Path, file: &Path, level: &str, scale: u32) -> Result<Stored, String> {
    let bytes = fs::read(file)
        .map_err(|_| "No se pudo leer el resultado para guardarlo en la biblioteca.".to_string())?;
    let image = image::load_from_memory(&bytes)
        .map_err(|_| "El resultado del escalado no es una imagen válida.".to_string())?;
    let (width, height) = image.dimensions();
    let thumb = thumb_bytes(&image)?;
    let format = format_of(file);

    let conn = connect(db)?;
    let name = unique_name(&conn, file)?;
    let id = new_id();
    let created_at = now_ms();
    conn.execute(
        "INSERT INTO results (id, name, format, level, scale, width, height, bytes, thumb, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            &id, &name, &format, level, scale, width, height, &bytes, &thumb, created_at
        ],
    )
    .map_err(|error| format!("No se pudo guardar en la biblioteca: {error}"))?;

    Ok(Stored {
        id,
        name,
        format,
        width,
        height,
        scale,
        level: level.to_string(),
        size: bytes.len() as u64,
        created_at,
    })
}

fn all_at(db: &Path) -> Result<Vec<Stored>, String> {
    let conn = connect(db)?;
    let mut statement = conn
        .prepare(
            "SELECT id, name, format, level, scale, width, height, length(bytes), created_at
             FROM results ORDER BY created_at DESC, rowid DESC",
        )
        .map_err(|error| format!("No se pudo leer la biblioteca: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(Stored {
                id: row.get(0)?,
                name: row.get(1)?,
                format: row.get(2)?,
                level: row.get(3)?,
                scale: row.get::<_, i64>(4)? as u32,
                width: row.get::<_, i64>(5)? as u32,
                height: row.get::<_, i64>(6)? as u32,
                size: row.get::<_, i64>(7)? as u64,
                created_at: row.get(8)?,
            })
        })
        .map_err(|error| format!("No se pudo leer la biblioteca: {error}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|error| format!("No se pudo leer la biblioteca: {error}"))?);
    }
    Ok(out)
}

fn blob_at(db: &Path, id: &str, thumb: bool) -> Result<Option<(Vec<u8>, String)>, String> {
    let conn = connect(db)?;
    let column = if thumb { "thumb" } else { "bytes" };
    let query = format!("SELECT format, {column} FROM results WHERE id = ?1");
    conn.query_row(&query, [id], |row| {
        let format: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        Ok((bytes, mime_of(&format).to_string()))
    })
    .optional()
    .map_err(|error| format!("No se pudo leer la imagen: {error}"))
}

fn remove_at(db: &Path, id: &str) -> Result<(), String> {
    let conn = connect(db)?;
    conn.execute("DELETE FROM results WHERE id = ?1", [id])
        .map_err(|error| format!("No se pudo eliminar la imagen: {error}"))?;
    Ok(())
}

fn bytes_at(db: &Path, id: &str) -> Result<Vec<u8>, String> {
    blob_at(db, id, false)?
        .map(|(bytes, _)| bytes)
        .ok_or_else(|| "Esa imagen ya no está en la biblioteca.".to_string())
}

fn export_at(db: &Path, id: &str, destination: &Path) -> Result<(), String> {
    let bytes = bytes_at(db, id)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "No se pudo crear la carpeta de destino.".to_string())?;
    }
    fs::write(destination, bytes)
        .map_err(|_| "No se pudo guardar la imagen; comprueba que la carpeta sea accesible.".to_string())
}

pub fn add(app: &AppHandle, file: &Path, level: &str, scale: u32) -> Result<Stored, String> {
    add_at(&library(app)?, file, level, scale)
}

pub fn all(app: &AppHandle) -> Result<Vec<Stored>, String> {
    all_at(&library(app)?)
}

pub fn remove(app: &AppHandle, id: &str) -> Result<(), String> {
    remove_at(&library(app)?, id)
}

pub fn export(app: &AppHandle, id: &str, destination: &Path) -> Result<(), String> {
    export_at(&library(app)?, id, destination)
}

/// Copia la imagen al portapapeles del sistema como mapa de bits.
pub fn copy(app: &AppHandle, id: &str) -> Result<(), String> {
    let bytes = bytes_at(&library(app)?, id)?;
    let image = image::load_from_memory(&bytes)
        .map_err(|_| "No se pudo leer la imagen para copiarla.".to_string())?
        .to_rgba8();
    let pixels = image.width() as u64 * image.height() as u64;
    if pixels > MAX_CLIPBOARD_PIXELS {
        return Err(
            "La imagen es demasiado grande para el portapapeles (más de 64 MP). Usa «Descargar»."
                .to_string(),
        );
    }

    let mut clipboard = arboard::Clipboard::new()
        .map_err(|_| "No se pudo acceder al portapapeles.".to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: image.width() as usize,
            height: image.height() as usize,
            bytes: Cow::Owned(image.into_raw()),
        })
        .map_err(|_| "El portapapeles está ocupado por otra aplicación. Inténtalo otra vez.".to_string())
}

fn reply(status: tauri::http::StatusCode, content_type: &str, body: Vec<u8>) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .header(tauri::http::header::CONTENT_TYPE, content_type)
        .header(tauri::http::header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
}

/// `/<id>` devuelve la imagen completa y `/thumb/<id>` la miniatura.
fn respond(db: &Path, path: &str) -> tauri::http::Response<Vec<u8>> {
    let path = path.trim_start_matches('/');
    let (thumb, id) = match path.split_once('/') {
        Some(("thumb", id)) => (true, id),
        _ => (false, path),
    };
    if id.is_empty() {
        return reply(tauri::http::StatusCode::BAD_REQUEST, "text/plain", b"Falta el id".to_vec());
    }

    match blob_at(db, id, thumb) {
        Ok(Some((bytes, mime))) => reply(tauri::http::StatusCode::OK, &mime, bytes),
        Ok(None) => reply(tauri::http::StatusCode::NOT_FOUND, "text/plain", b"No encontrada".to_vec()),
        Err(message) => reply(
            tauri::http::StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            message.into_bytes(),
        ),
    }
}

/// `appimg://localhost/<id>` y `appimg://localhost/thumb/<id>`.
/// En Windows Tauri lo expone como `http://appimg.localhost/<id>`.
pub fn serve(app: &AppHandle, request: &tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    match library(app) {
        Ok(db) => respond(&db, request.uri().path()),
        Err(message) => reply(
            tauri::http::StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            message.into_bytes(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        image::RgbImage::from_fn(width, height, |x, y| image::Rgb([x as u8, y as u8, 40]))
            .save(&path)
            .unwrap();
        path
    }

    #[test]
    fn stores_lists_exports_and_deletes() {
        let dir = std::env::temp_dir().join("img-upscaler-biblioteca");
        let _ = fs::remove_dir_all(&dir);
        let db = dir.join("biblioteca.db");
        let file = sample(&dir, "foto_2x.png", 64, 48);

        let stored = add_at(&db, &file, "potente", 2).unwrap();
        assert_eq!((stored.width, stored.height), (64, 48));
        assert_eq!(stored.name, "foto_2x.png");
        assert_eq!(stored.scale, 2);
        assert_eq!(stored.level, "potente");

        let listed = all_at(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, stored.id);

        let (full, mime) = blob_at(&db, &stored.id, false).unwrap().unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(
            image::load_from_memory(&full).unwrap().dimensions(),
            (64, 48)
        );
        assert!(blob_at(&db, &stored.id, true).unwrap().is_some());

        let copy = dir.join("copia.png");
        export_at(&db, &stored.id, &copy).unwrap();
        assert_eq!(image::image_dimensions(&copy).unwrap(), (64, 48));

        // dos resultados con el mismo nombre no se pisan
        let again = add_at(&db, &file, "potente", 2).unwrap();
        assert_eq!(again.name, "foto_2x-2.png");

        remove_at(&db, &stored.id).unwrap();
        assert!(blob_at(&db, &stored.id, false).unwrap().is_none());
        assert_eq!(all_at(&db).unwrap().len(), 1);
    }

    #[test]
    fn serves_full_images_and_thumbnails_by_id() {
        let dir = std::env::temp_dir().join("img-upscaler-protocolo");
        let _ = fs::remove_dir_all(&dir);
        let db = dir.join("biblioteca.db");
        let file = sample(&dir, "foto_4x.png", 800, 600);
        let stored = add_at(&db, &file, "intermedio", 4).unwrap();

        let full = respond(&db, &format!("/{}", stored.id));
        assert_eq!(full.status(), tauri::http::StatusCode::OK);
        assert_eq!(
            full.headers().get(tauri::http::header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(
            image::load_from_memory(full.body()).unwrap().dimensions(),
            (800, 600)
        );

        let thumb = respond(&db, &format!("/thumb/{}", stored.id));
        assert_eq!(thumb.status(), tauri::http::StatusCode::OK);
        assert_eq!(
            image::load_from_memory(thumb.body()).unwrap().dimensions(),
            (THUMB_SIDE, THUMB_SIDE * 3 / 4)
        );
        assert!(thumb.body().len() < full.body().len());

        assert_eq!(
            respond(&db, "/no-existe").status(),
            tauri::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            respond(&db, "/").status(),
            tauri::http::StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn rejects_files_that_are_not_images() {
        let dir = std::env::temp_dir().join("img-upscaler-biblioteca-rota");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("biblioteca.db");
        let file = dir.join("no.png");
        fs::write(&file, b"no soy una imagen").unwrap();
        assert!(add_at(&db, &file, "simple", 2).is_err());
        assert!(all_at(&db).unwrap().is_empty());
    }
}
