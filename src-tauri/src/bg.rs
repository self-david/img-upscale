//! Quitar fondo con u2netp (ONNX) ejecutado en CPU, sin salir del equipo.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use image::imageops::FilterType;
use image::{GrayImage, RgbImage, RgbaImage};
use ort::session::Session;
use ort::value::Tensor;

const SIZE: usize = 320;

/// ONNX Runtime admite un único entorno por proceso: si se crea otro, la carga
/// falla con «only one environment is allowed per process». Se crea una vez y
/// todas las sesiones lo comparten.
/// ONNX Runtime admite un único entorno por proceso: si se crea otro, la carga
/// falla con «only one environment is allowed per process». Se crea una vez y
/// todas las sesiones lo comparten.
fn environment() -> Result<&'static ort::environment::Environment, String> {
    static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        ort::init()
            .build()
            .map_err(|error| format!("No se pudo iniciar ONNX Runtime: {error}"))
    })
    .as_ref()
    .map_err(|error| error.clone())
}

pub struct Segmenter {
    session: Session,
}

impl Segmenter {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.is_file() {
            return Err(format!(
                "No se encontró el modelo de fondo (u2netp.onnx) en {}.",
                path.display()
            ));
        }
        let env = environment()?;
        let session = Session::builder(env)
            .and_then(|mut builder| builder.commit_from_file(path))
            .map_err(|error| format!("No se pudo cargar el modelo de fondo: {error}"))?;
        Ok(Segmenter { session })
    }

    /// Máscara de 0 a 255 con el tamaño de la imagen original.
    pub fn mask(&mut self, image: &RgbImage) -> Result<GrayImage, String> {
        let small = image::imageops::resize(image, SIZE as u32, SIZE as u32, FilterType::Triangle);
        let plane = SIZE * SIZE;
        let mut data = vec![0f32; 3 * plane];
        let mean = [0.485f32, 0.456, 0.406];
        let std = [0.229f32, 0.224, 0.225];
        for (index, pixel) in small.pixels().enumerate() {
            for channel in 0..3 {
                let value = pixel[channel] as f32 / 255.0;
                data[channel * plane + index] = (value - mean[channel]) / std[channel];
            }
        }

        let input = Tensor::from_array(([1usize, 3, SIZE, SIZE], data.into_boxed_slice()))
            .map_err(|error| format!("Entrada inválida para el modelo de fondo: {error}"))?;
        let outputs = self
            .session
            .run(ort::inputs![input])
            .map_err(|error| format!("El modelo de fondo falló: {error}"))?;
        let (_, values) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|error| format!("Salida inesperada del modelo de fondo: {error}"))?;

        if values.len() < plane {
            return Err(format!(
                "El modelo de fondo devolvió {} valores en vez de {plane}.",
                values.len()
            ));
        }
        let raw = &values[..plane];
        let (mut low, mut high) = (f32::MAX, f32::MIN);
        for value in raw {
            low = low.min(*value);
            high = high.max(*value);
        }
        let span = (high - low).max(1e-6);

        let bytes: Vec<u8> = raw
            .iter()
            .map(|value| (((value - low) / span) * 255.0).clamp(0.0, 255.0) as u8)
            .collect();
        let mask = GrayImage::from_raw(SIZE as u32, SIZE as u32, bytes)
            .ok_or_else(|| "No se pudo construir la máscara de fondo.".to_string())?;

        Ok(image::imageops::resize(
            &mask,
            image.width(),
            image.height(),
            FilterType::Triangle,
        ))
    }
}

pub fn output_path(src: &Path, target_dir: &Path) -> PathBuf {
    let stem = src
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "imagen".to_string());
    let mut candidate = target_dir.join(format!("{stem}-sin-fondo.png"));
    let mut index = 2;
    while candidate.exists() {
        candidate = target_dir.join(format!("{stem}-sin-fondo-{index}.png"));
        index += 1;
    }
    candidate
}

/// Aplica la máscara como transparencia y guarda un PNG.
pub fn cutout(image: &RgbImage, mask: &GrayImage, destination: &Path) -> Result<(), String> {
    let cut = RgbaImage::from_fn(image.width(), image.height(), |x, y| {
        let pixel = image.get_pixel(x, y);
        let alpha = mask.get_pixel(x, y)[0];
        image::Rgba([pixel[0], pixel[1], pixel[2], alpha])
    });
    image::DynamicImage::ImageRgba8(cut)
        .save(destination)
        .map_err(|_| "No se pudo guardar la imagen sin fondo.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_cutout_next_to_the_upscaled_file() {
        let dir = std::env::temp_dir().join("img-upscaler-nombres");
        let _ = std::fs::create_dir_all(&dir);
        let scaled = dir.join("foto-2x.png");
        assert_eq!(output_path(&scaled, &dir), dir.join("foto-2x-sin-fondo.png"));
    }

    /// Diagnóstico: la app carga una sesión por imagen, así que esto reproduce
    /// lo que pasa en una tanda con "Quitar el fondo" activado.
    #[test]
    fn loads_and_runs_more_than_once() {
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/u2netp.onnx");
        if !model.is_file() {
            return;
        }
        for attempt in 0..3 {
            let mut segmenter = Segmenter::load(&model)
                .unwrap_or_else(|error| panic!("carga {attempt}: {error}"));
            for (width, height) in [(64u32, 48u32), (1, 1), (640, 480)] {
                let image = RgbImage::from_fn(width, height, |x, y| {
                    image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
                });
                segmenter
                    .mask(&image)
                    .unwrap_or_else(|error| panic!("máscara {attempt} {width}x{height}: {error}"));
            }
        }
    }

    #[test]
    #[ignore]
    fn cutout_sample() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let model = root.join("models/u2netp.onnx");
        if !model.is_file() {
            return;
        }
        let mut segmenter = Segmenter::load(&model).unwrap();
        for name in ["foto.jpg", "perro.webp"] {
            let image = match image::open(root.join("../samples").join(name)) {
                Ok(image) => image.to_rgb8(),
                Err(_) => continue,
            };
            let mask = segmenter.mask(&image).unwrap();
            let out = root
                .join("../samples")
                .join(format!("muestra-sin-fondo-{name}.png"));
            cutout(&image, &mask, &out).unwrap();
        }
    }
}
