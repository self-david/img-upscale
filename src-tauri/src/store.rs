//! Biblioteca de resultados: la base SQLite guarda los metadatos y la
//! miniatura, y la imagen completa vive en `resultados/` para que la base no
//! crezca sin control. También guarda la sesión (ajustes y cola).

use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use image::{DynamicImage, GenericImageView, ImageFormat};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub const PROTOCOL: &str = "appimg";
const THUMB_SIDE: u32 = 480;
/// Un RGBA de más de 64 MP no cabe con holgura en el portapapeles.
const MAX_CLIPBOARD_PIXELS: u64 = 64_000_000;
const RESULTS_COLUMNS: &str = "
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    format TEXT NOT NULL,
    level TEXT NOT NULL,
    scale INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    size INTEGER NOT NULL,
    file TEXT NOT NULL,
    thumb BLOB NOT NULL,
    created_at INTEGER NOT NULL";
const QUEUE_COLUMNS: &str = "
    position INTEGER NOT NULL,
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    size INTEGER NOT NULL,
    status TEXT NOT NULL,
    result_id TEXT";

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

/// Ajustes de la última tanda; se restauran al abrir la app.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub level: Option<String>,
    pub scale: Option<u32>,
    pub remove_bg: Option<bool>,
    pub quality: Option<u8>,
    pub chroma: Option<String>,
    pub format: Option<String>,
    pub device: Option<i64>,
}

/// Una imagen de la cola tal como se guarda para poder reanudarla.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QueuedItem {
    pub id: String,
    pub path: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub status: String,
    pub result_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Session {
    pub settings: Settings,
    pub queue: Vec<QueuedItem>,
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

/// Carpeta de las imágenes completas, hermana de la base de datos.
fn files_dir(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("resultados")
}

fn schema() -> String {
    format!(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS results ({RESULTS_COLUMNS});
         CREATE INDEX IF NOT EXISTS results_created ON results (created_at DESC);
         CREATE TABLE IF NOT EXISTS settings (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS queue ({QUEUE_COLUMNS});"
    )
}

fn connect(db: &Path) -> Result<Connection, String> {
    let conn = Connection::open(db)
        .map_err(|error| format!("No se pudo abrir la biblioteca: {error}"))?;
    let _ = conn.busy_timeout(Duration::from_secs(5));
    migrate_legacy(&conn, db)
        .map_err(|error| format!("No se pudo convertir la biblioteca: {error}"))?;
    conn.execute_batch(&schema())
        .map_err(|error| format!("No se pudo preparar la biblioteca: {error}"))?;
    Ok(conn)
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut statement) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let names: Vec<String> = rows.filter_map(Result::ok).collect();
    names.iter().any(|name| name == column)
}

/// Las primeras versiones guardaban la imagen completa como BLOB dentro de la
/// base. Ahora vive en `resultados/` y solo se guarda el nombre del archivo:
/// esta migración saca los blobs antiguos al disco una única vez.
fn migrate_legacy(conn: &Connection, db: &Path) -> Result<(), String> {
    if !has_table(conn, "results") || has_column(conn, "results", "file") {
        return Ok(());
    }
    let dir = files_dir(db);
    fs::create_dir_all(&dir).map_err(|error| format!("carpeta de resultados: {error}"))?;

    type Legacy = (String, String, String, String, i64, i64, i64, Vec<u8>, Vec<u8>, i64);
    let mut old: Vec<Legacy> = Vec::new();
    {
        let mut statement = conn
            .prepare(
                "SELECT id, name, format, level, scale, width, height, bytes, thumb, created_at
                 FROM results",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            old.push(row.map_err(|error| error.to_string())?);
        }
    }

    conn.execute_batch("DROP INDEX IF EXISTS results_created; ALTER TABLE results RENAME TO results_legacy;")
        .map_err(|error| error.to_string())?;
    conn.execute_batch(&schema()).map_err(|error| error.to_string())?;

    for (id, name, format, level, scale, width, height, bytes, thumb, created_at) in old {
        let file = format!("{id}.{format}");
        fs::write(dir.join(&file), &bytes)
            .map_err(|error| format!("no se pudo mover {name}: {error}"))?;
        conn.execute(
            "INSERT INTO results (id, name, format, level, scale, width, height, size, file, thumb, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &id,
                &name,
                &format,
                &level,
                scale,
                width,
                height,
                bytes.len() as i64,
                &file,
                &thumb,
                created_at
            ],
        )
        .map_err(|error| error.to_string())?;
    }

    conn.execute_batch("DROP TABLE results_legacy; VACUUM;")
        .map_err(|error| error.to_string())?;
    Ok(())
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
    let stored_name = format!("{id}.{format}");
    let dir = files_dir(db);
    fs::create_dir_all(&dir)
        .map_err(|_| "No se pudo crear la carpeta de resultados.".to_string())?;
    fs::write(dir.join(&stored_name), &bytes)
        .map_err(|_| "No se pudo guardar la imagen del resultado.".to_string())?;

    let inserted = conn.execute(
        "INSERT INTO results (id, name, format, level, scale, width, height, size, file, thumb, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            &id,
            &name,
            &format,
            level,
            scale,
            width,
            height,
            bytes.len() as i64,
            &stored_name,
            &thumb,
            created_at
        ],
    );
    if let Err(error) = inserted {
        let _ = fs::remove_file(dir.join(&stored_name));
        return Err(format!("No se pudo guardar en la biblioteca: {error}"));
    }

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
            "SELECT id, name, format, level, scale, width, height, size, created_at
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
    if thumb {
        return conn
            .query_row("SELECT format, thumb FROM results WHERE id = ?1", [id], |row| {
                let format: String = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                Ok((bytes, mime_of(&format).to_string()))
            })
            .optional()
            .map_err(|error| format!("No se pudo leer la imagen: {error}"));
    }

    let row: Option<(String, String)> = conn
        .query_row("SELECT format, file FROM results WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()
        .map_err(|error| format!("No se pudo leer la imagen: {error}"))?;
    let Some((format, file)) = row else {
        return Ok(None);
    };
    match fs::read(files_dir(db).join(&file)) {
        Ok(bytes) => Ok(Some((bytes, mime_of(&format).to_string()))),
        // el archivo se borró por fuera: se trata como si ya no existiera
        Err(_) => Ok(None),
    }
}

fn remove_at(db: &Path, id: &str) -> Result<(), String> {
    let conn = connect(db)?;
    let file: Option<String> = conn
        .query_row("SELECT file FROM results WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|error| format!("No se pudo eliminar la imagen: {error}"))?;
    conn.execute("DELETE FROM results WHERE id = ?1", [id])
        .map_err(|error| format!("No se pudo eliminar la imagen: {error}"))?;
    if let Some(file) = file {
        let _ = fs::remove_file(files_dir(db).join(file));
    }
    Ok(())
}

fn bytes_at(db: &Path, id: &str) -> Result<Vec<u8>, String> {
    blob_at(db, id, false)?
        .map(|(bytes, _)| bytes)
        .ok_or_else(|| "Esa imagen ya no está en la biblioteca.".to_string())
}

fn export_at(db: &Path, id: &str, destination: &Path) -> Result<(), String> {
    let source = files_dir(db).join(file_at(db, id)?);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "No se pudo crear la carpeta de destino.".to_string())?;
    }
    fs::copy(&source, destination)
        .map(|_| ())
        .map_err(|_| "No se pudo guardar la imagen; comprueba que la carpeta sea accesible.".to_string())
}

fn file_at(db: &Path, id: &str) -> Result<String, String> {
    let conn = connect(db)?;
    conn.query_row("SELECT file FROM results WHERE id = ?1", [id], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .map_err(|error| format!("No se pudo leer la imagen: {error}"))?
    .ok_or_else(|| "Esa imagen ya no está en la biblioteca.".to_string())
}

fn settings_at(db: &Path) -> Result<Settings, String> {
    let conn = connect(db)?;
    let raw: Option<String> = conn
        .query_row("SELECT json FROM settings WHERE id = 1", [], |row| row.get(0))
        .optional()
        .map_err(|error| format!("No se pudieron leer los ajustes: {error}"))?;
    Ok(raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default())
}

fn settings_set_at(db: &Path, settings: &Settings) -> Result<(), String> {
    let conn = connect(db)?;
    let raw = serde_json::to_string(settings)
        .map_err(|_| "No se pudieron preparar los ajustes.".to_string())?;
    conn.execute(
        "INSERT INTO settings (id, json) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET json = excluded.json",
        [&raw],
    )
    .map_err(|error| format!("No se pudieron guardar los ajustes: {error}"))?;
    Ok(())
}

fn queue_at(db: &Path) -> Result<Vec<QueuedItem>, String> {
    let conn = connect(db)?;
    let mut statement = conn
        .prepare(
            "SELECT id, path, name, width, height, size, status, result_id
             FROM queue ORDER BY position",
        )
        .map_err(|error| format!("No se pudo leer la cola: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(QueuedItem {
                id: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                width: row.get::<_, i64>(3)? as u32,
                height: row.get::<_, i64>(4)? as u32,
                size: row.get::<_, i64>(5)? as u64,
                status: row.get(6)?,
                result_id: row.get(7)?,
            })
        })
        .map_err(|error| format!("No se pudo leer la cola: {error}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|error| format!("No se pudo leer la cola: {error}"))?);
    }
    Ok(out)
}

fn queue_set_at(db: &Path, items: &[QueuedItem]) -> Result<(), String> {
    let mut conn = connect(db)?;
    let transaction = conn
        .transaction()
        .map_err(|error| format!("No se pudo guardar la cola: {error}"))?;
    transaction
        .execute("DELETE FROM queue", [])
        .map_err(|error| format!("No se pudo guardar la cola: {error}"))?;
    for (position, item) in items.iter().enumerate() {
        // lo que estaba en marcha se reanuda como pendiente
        let status = if item.status == "running" {
            "pending"
        } else {
            item.status.as_str()
        };
        transaction
            .execute(
                "INSERT INTO queue (position, id, path, name, width, height, size, status, result_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    position as i64,
                    &item.id,
                    &item.path,
                    &item.name,
                    item.width as i64,
                    item.height as i64,
                    item.size as i64,
                    status,
                    &item.result_id
                ],
            )
            .map_err(|error| format!("No se pudo guardar la cola: {error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("No se pudo guardar la cola: {error}"))
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

/// Guarda varias imágenes en una carpeta, sin pisar archivos existentes.
pub fn export_many(app: &AppHandle, ids: &[String], folder: &Path) -> Result<usize, String> {
    let db = library(app)?;
    fs::create_dir_all(folder).map_err(|_| "No se pudo crear la carpeta de destino.".to_string())?;
    let mut saved = 0;
    for id in ids {
        let stored: Option<String> = {
            let conn = connect(&db)?;
            conn.query_row("SELECT name FROM results WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| format!("No se pudo leer la biblioteca: {error}"))?
        };
        let Some(name) = stored else { continue };
        let mut target = folder.join(&name);
        let (stem, ext) = match name.rsplit_once('.') {
            Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
            None => (name.clone(), String::new()),
        };
        let mut index = 2;
        while target.exists() {
            target = folder.join(format!("{stem}-{index}{ext}"));
            index += 1;
        }
        export_at(&db, id, &target)?;
        saved += 1;
    }
    Ok(saved)
}

pub fn session(app: &AppHandle) -> Result<Session, String> {
    let db = library(app)?;
    Ok(Session {
        settings: settings_at(&db)?,
        queue: queue_at(&db)?,
    })
}

pub fn save_session(app: &AppHandle, settings: &Settings, queue: &[QueuedItem]) -> Result<(), String> {
    let db = library(app)?;
    settings_set_at(&db, settings)?;
    queue_set_at(&db, queue)
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

        // la imagen completa vive en disco; la base solo guarda su nombre
        let on_disk = files_dir(&db).join(format!("{}.png", stored.id));
        assert!(on_disk.is_file());
        assert_eq!(fs::metadata(&on_disk).unwrap().len(), stored.size);

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
        assert!(!on_disk.exists());
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

        // si el archivo desaparece por fuera, se responde 404 y no datos rotos
        fs::remove_file(files_dir(&db).join(format!("{}.png", stored.id))).unwrap();
        assert_eq!(
            respond(&db, &format!("/{}", stored.id)).status(),
            tauri::http::StatusCode::NOT_FOUND
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

    #[test]
    fn migrates_blobs_to_files_once() {
        let dir = std::env::temp_dir().join("img-upscaler-migracion");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("biblioteca.db");

        // base con el esquema viejo: la imagen completa dentro de la base
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE results (
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
                );",
            )
            .unwrap();
            let image = image::RgbImage::from_fn(32, 24, |x, y| image::Rgb([x as u8, y as u8, 10]));
            let mut full = Vec::new();
            image
                .write_to(&mut std::io::Cursor::new(&mut full), ImageFormat::Png)
                .unwrap();
            let mut thumb = Vec::new();
            image
                .write_to(&mut std::io::Cursor::new(&mut thumb), ImageFormat::Png)
                .unwrap();
            conn.execute(
                "INSERT INTO results VALUES ('viejo', 'viejo_2x.png', 'png', 'simple', 2, 32, 24, ?1, ?2, 1)",
                params![&full, &thumb],
            )
            .unwrap();
        }

        let listed = all_at(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].size, fs::metadata(files_dir(&db).join("viejo.png")).unwrap().len());
        assert_eq!(
            blob_at(&db, "viejo", false).unwrap().unwrap().0,
            fs::read(files_dir(&db).join("viejo.png")).unwrap()
        );

        // la segunda apertura no vuelve a migrar ni duplica nada
        assert_eq!(all_at(&db).unwrap().len(), 1);
        let conn = Connection::open(&db).unwrap();
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(results)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(columns.contains(&"file".to_string()));
        assert!(!has_table(&conn, "results_legacy"));
    }

    #[test]
    fn keeps_settings_and_queue_between_sessions() {
        let dir = std::env::temp_dir().join("img-upscaler-sesion");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("biblioteca.db");

        assert!(queue_at(&db).unwrap().is_empty());
        settings_set_at(
            &db,
            &Settings {
                level: Some("simple".into()),
                scale: Some(4),
                quality: Some(80),
                format: Some("webp".into()),
                ..Settings::default()
            },
        )
        .unwrap();
        queue_set_at(
            &db,
            &[QueuedItem {
                id: "uno".into(),
                path: "C:/foto.jpg".into(),
                name: "foto.jpg".into(),
                width: 10,
                height: 8,
                size: 1234,
                status: "running".into(),
                result_id: None,
            }],
        )
        .unwrap();

        let settings = settings_at(&db).unwrap();
        assert_eq!(settings.level.as_deref(), Some("simple"));
        assert_eq!(settings.scale, Some(4));
        assert_eq!(settings.quality, Some(80));
        assert_eq!(settings.format.as_deref(), Some("webp"));

        let queue = queue_at(&db).unwrap();
        assert_eq!(queue.len(), 1);
        // lo que estaba en marcha vuelve como pendiente
        assert_eq!(queue[0].status, "pending");
        assert_eq!(queue[0].path, "C:/foto.jpg");

        // guardar de nuevo reemplaza la cola entera
        queue_set_at(&db, &[]).unwrap();
        assert!(queue_at(&db).unwrap().is_empty());
    }
}
