/// App icon loading from embedded PNG asset.
///
/// Uses assets/icon.png (ICONO-02_2: dark blue circle with cyan AGB).

const ICON_BYTES: &[u8] = include_bytes!("../../assets/icon.png");

/// Decode the embedded PNG and resize to the requested dimensions.
/// Returns (rgba_bytes, width, height).
fn load_icon(size: u32) -> (Vec<u8>, u32, u32) {
    let img = image::load_from_memory(ICON_BYTES).expect("embedded icon.png is valid");
    let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
    let rgba = resized.to_rgba8().into_raw();
    (rgba, size, size)
}

/// Convert icon data to `egui::IconData` for window icons.
pub fn app_icon_data() -> egui::IconData {
    let (rgba, w, h) = load_icon(64);
    egui::IconData { rgba, width: w, height: h }
}

/// Convert icon data to `tray_icon::Icon` for the system tray.
pub fn tray_icon() -> tray_icon::Icon {
    let (rgba, w, h) = load_icon(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("valid icon data")
}
