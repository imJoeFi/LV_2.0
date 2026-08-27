use iced::widget::image;
use qrcode::{types::QrError, Color, QrCode};
use std::{error::Error, fmt};

const QUIET_ZONE_MODULES: usize = 4;
const RGBA_CHANNELS: usize = 4;
const BLACK: [u8; RGBA_CHANNELS] = [0, 0, 0, u8::MAX];

/// A square, theme-independent QR image with integer-sized modules.
#[derive(Clone)]
pub struct RasterQr {
    handle: image::Handle,
}

impl RasterQr {
    /// Encodes `payload` into a black-on-white image of exactly `side` pixels.
    pub fn new(payload: impl AsRef<[u8]>, side: u16) -> Result<Self, RasterQrError> {
        let pixels = rasterize(payload.as_ref(), side)?;
        let side = u32::from(side);
        Ok(Self {
            handle: image::Handle::from_rgba(side, side, pixels),
        })
    }

    pub fn handle(&self) -> image::Handle {
        self.handle.clone()
    }
}

#[derive(Debug)]
pub enum RasterQrError {
    Encoding(QrError),
    TargetTooSmall { side: u16, required_modules: usize },
}

impl fmt::Display for RasterQrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encoding(error) => write!(formatter, "QR data could not be encoded: {error}"),
            Self::TargetTooSmall {
                side,
                required_modules,
            } => write!(
                formatter,
                "a {side}-pixel image is too small for a {required_modules}-module QR code"
            ),
        }
    }
}

impl Error for RasterQrError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encoding(error) => Some(error),
            Self::TargetTooSmall { .. } => None,
        }
    }
}

impl From<QrError> for RasterQrError {
    fn from(error: QrError) -> Self {
        Self::Encoding(error)
    }
}

fn rasterize(payload: &[u8], side: u16) -> Result<Vec<u8>, RasterQrError> {
    let code = QrCode::new(payload)?;
    let matrix_width = code.width();
    let required_modules = matrix_width + 2 * QUIET_ZONE_MODULES;
    let side = usize::from(side);
    let module_size = side / required_modules;
    if module_size == 0 {
        return Err(RasterQrError::TargetTooSmall {
            side: u16::try_from(side).expect("QR image side originated as u16"),
            required_modules,
        });
    }

    let matrix_pixels = matrix_width * module_size;
    let matrix_offset = (side - matrix_pixels) / 2;
    let mut pixels = vec![u8::MAX; side * side * RGBA_CHANNELS];
    for (index, color) in code.into_colors().into_iter().enumerate() {
        if color == Color::Dark {
            let row = index / matrix_width;
            let column = index % matrix_width;
            fill_module(
                &mut pixels,
                side,
                matrix_offset + column * module_size,
                matrix_offset + row * module_size,
                module_size,
            );
        }
    }
    Ok(pixels)
}

fn fill_module(pixels: &mut [u8], side: usize, x: usize, y: usize, module_size: usize) {
    for row in y..y + module_size {
        let start = (row * side + x) * RGBA_CHANNELS;
        let end = start + module_size * RGBA_CHANNELS;
        for pixel in pixels[start..end].chunks_exact_mut(RGBA_CHANNELS) {
            pixel.copy_from_slice(&BLACK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::GrayImage;

    #[test]
    fn realistic_pairing_payload_round_trips_through_the_rendered_image() {
        let payload = format!("{{\"id\":\"{}\"}}", "x".repeat(368));
        assert_eq!(payload.len(), 377);
        let side = 340_u16;
        let pixels = rasterize(payload.as_bytes(), side).unwrap();
        assert_eq!(pixels.len(), usize::from(side).pow(2) * RGBA_CHANNELS);

        let grayscale = pixels
            .chunks_exact(RGBA_CHANNELS)
            .map(|pixel| pixel[0])
            .collect();
        let image = GrayImage::from_raw(u32::from(side), u32::from(side), grayscale)
            .expect("rendered pixel count matches the square dimensions");
        let mut prepared = rqrr::PreparedImage::prepare(image);
        let decoded = prepared
            .detect_grids()
            .into_iter()
            .find_map(|grid| grid.decode().ok().map(|(_, decoded)| decoded));
        assert_eq!(decoded.as_deref(), Some(payload.as_str()));
    }

    #[test]
    fn raster_is_standard_black_on_white_with_a_quiet_border() {
        let side = 290_u16;
        let pixels = rasterize(b"lnbc-test-invoice", side).unwrap();
        let width = usize::from(side);
        let pixel = |x: usize, y: usize| {
            let start = (y * width + x) * RGBA_CHANNELS;
            &pixels[start..start + RGBA_CHANNELS]
        };
        assert_eq!(pixel(0, 0), [u8::MAX; RGBA_CHANNELS]);
        assert_eq!(pixel(width - 1, width - 1), [u8::MAX; RGBA_CHANNELS]);
        assert!(pixels
            .chunks_exact(RGBA_CHANNELS)
            .any(|pixel| pixel == BLACK));
    }

    #[test]
    fn rejects_a_target_that_cannot_fit_one_pixel_per_module() {
        let error = RasterQr::new([42; 100], 20).err().unwrap();
        assert!(matches!(error, RasterQrError::TargetTooSmall { .. }));
    }
}
