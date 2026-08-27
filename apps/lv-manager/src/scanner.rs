use nokhwa::{
    pixel_format::RgbFormat,
    utils::{
        CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    },
    Camera,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread,
    time::Duration,
};

pub enum ScannerEvent {
    Frame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Decoded(String),
    Failed(String),
}

pub struct QrScanner {
    events: Receiver<ScannerEvent>,
    stopped: Arc<AtomicBool>,
}

impl QrScanner {
    pub fn start() -> Self {
        let (events, receiver) = mpsc::sync_channel(2);
        let stopped = Arc::new(AtomicBool::new(false));
        start_capture(events, Arc::clone(&stopped));
        Self {
            events: receiver,
            stopped,
        }
    }

    pub fn try_event(&self) -> Option<ScannerEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for QrScanner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "macos")]
fn start_capture(events: SyncSender<ScannerEvent>, stopped: Arc<AtomicBool>) {
    nokhwa::nokhwa_initialize(move |granted| {
        if !granted {
            let _ = events.send(ScannerEvent::Failed(
                "Camera permission was denied. Allow camera access in System Settings, or paste the pairing payload."
                    .to_owned(),
            ));
            return;
        }
        let events = events.clone();
        let stopped = Arc::clone(&stopped);
        let _ = thread::Builder::new()
            .name("lv-manager-qr-scanner".to_owned())
            .spawn(move || capture_loop(&events, &stopped));
    });
}

#[cfg(not(target_os = "macos"))]
fn start_capture(events: SyncSender<ScannerEvent>, stopped: Arc<AtomicBool>) {
    let _ = thread::Builder::new()
        .name("lv-manager-qr-scanner".to_owned())
        .spawn(move || capture_loop(&events, &stopped));
}

fn capture_loop(events: &SyncSender<ScannerEvent>, stopped: &AtomicBool) {
    if let Err(error) = run_capture(events, stopped) {
        let _ = events.send(ScannerEvent::Failed(error));
    }
}

fn run_capture(events: &SyncSender<ScannerEvent>, stopped: &AtomicBool) -> Result<(), String> {
    let mut camera = open_camera()?;
    camera
        .open_stream()
        .map_err(|error| format!("Could not open the camera: {error}"))?;
    while !stopped.load(Ordering::Relaxed) {
        let frame = camera
            .frame()
            .map_err(|error| format!("Could not read from the camera: {error}"))?;
        let decoded = frame
            .decode_image::<RgbFormat>()
            .map_err(|error| format!("Could not decode a camera frame: {error}"))?;
        let (width, height) = decoded.dimensions();
        let rgb = decoded.into_raw();
        if let Some(payload) = decode_qr(width, height, &rgb) {
            let _ = events.send(ScannerEvent::Decoded(payload));
            return Ok(());
        }
        let rgba = rgb
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], u8::MAX])
            .collect();
        let _ = events.try_send(ScannerEvent::Frame {
            width,
            height,
            rgba,
        });
        thread::sleep(Duration::from_millis(75));
    }
    Ok(())
}

fn open_camera() -> Result<Camera, String> {
    let formats = [
        FrameFormat::MJPEG,
        FrameFormat::NV12,
        FrameFormat::YUYV,
        FrameFormat::RAWRGB,
    ];
    let mut errors = Vec::new();
    for format in formats {
        let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(Resolution::new(1280, 720), format, 30),
        ));
        match Camera::new(CameraIndex::Index(0), requested) {
            Ok(camera) => return Ok(camera),
            Err(error) => errors.push(error.to_string()),
        }
    }
    Err(format!(
        "Could not initialize the default camera: {}",
        errors.join("; ")
    ))
}

fn decode_qr(width: u32, height: u32, rgb: &[u8]) -> Option<String> {
    let grayscale = rgb
        .chunks_exact(3)
        .map(|pixel| {
            let value =
                u32::from(pixel[0]) * 77 + u32::from(pixel[1]) * 150 + u32::from(pixel[2]) * 29;
            u8::try_from(value >> 8).expect("weighted RGB value fits in one byte")
        })
        .collect();
    let image = image::GrayImage::from_raw(width, height, grayscale)?;
    let mut prepared = rqrr::PreparedImage::prepare(image);
    prepared
        .detect_grids()
        .into_iter()
        .find_map(|grid| grid.decode().ok().map(|(_, payload)| payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grayscale_conversion_rejects_invalid_dimensions_without_panicking() {
        assert!(decode_qr(10, 10, &[0; 3]).is_none());
    }
}
