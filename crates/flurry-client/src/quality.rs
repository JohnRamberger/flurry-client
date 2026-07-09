//! Client-side, no-reference image quality metrics for the benchmark.
//!
//! A true reference comparison (PSNR/SSIM) is impossible here: the 3DS
//! never sends a pristine frame, and the content moves. Instead we compute
//! relative metrics on the decoded screen buffer — comparable across
//! configurations showing the same scene:
//!
//! - **sharpness**: mean absolute luma gradient. Interlace line-doubling
//!   and quarter-res 2x2 upscaling measurably reduce high-frequency energy.
//! - **blockiness**: luma discontinuity at 8-px grid boundaries relative to
//!   the interior average — tracks JPEG quantization artifacts. (>1 means
//!   boundaries are more discontinuous than the interior.)

/// Returns (sharpness, blockiness) for a decoded screen image.
pub fn measure(img: &egui::ColorImage) -> (f32, f32) {
    let w = img.size[0];
    let h = img.size[1];
    if w < 16 || h < 16 {
        return (0.0, 1.0);
    }

    // Luma plane (integer approximation of BT.601).
    let mut y = vec![0i32; w * h];
    for (i, px) in img.pixels.iter().enumerate() {
        y[i] = (77 * px.r() as i32 + 150 * px.g() as i32 + 29 * px.b() as i32) >> 8;
    }

    let mut grad_sum = 0i64;
    let mut grad_n = 0i64;
    let mut edge_sum = 0i64; // discontinuity at 8-px boundaries
    let mut edge_n = 0i64;
    let mut inner_sum = 0i64; // discontinuity elsewhere
    let mut inner_n = 0i64;

    for row in 0..h {
        let base = row * w;
        for col in 1..w {
            let d = (y[base + col] - y[base + col - 1]).abs() as i64;
            grad_sum += d;
            grad_n += 1;
            if col % 8 == 0 {
                edge_sum += d;
                edge_n += 1;
            } else {
                inner_sum += d;
                inner_n += 1;
            }
        }
    }
    for row in 1..h {
        for col in 0..w {
            let d = (y[row * w + col] - y[(row - 1) * w + col]).abs() as i64;
            grad_sum += d;
            grad_n += 1;
            if row % 8 == 0 {
                edge_sum += d;
                edge_n += 1;
            } else {
                inner_sum += d;
                inner_n += 1;
            }
        }
    }

    let sharpness = grad_sum as f32 / grad_n.max(1) as f32;
    let edge = edge_sum as f32 / edge_n.max(1) as f32;
    let inner = inner_sum as f32 / inner_n.max(1) as f32;
    let blockiness = if inner > 0.01 { edge / inner } else { 1.0 };
    (sharpness, blockiness)
}

/// Save a decoded screen image as PNG. Best-effort; errors are returned as
/// strings for the log.
pub fn save_png(img: &egui::ColorImage, path: &std::path::Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        img.size[0] as u32,
        img.size[1] as u32,
    );
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    let mut data = Vec::with_capacity(img.pixels.len() * 4);
    for px in &img.pixels {
        data.extend_from_slice(&[px.r(), px.g(), px.b(), 255]);
    }
    writer.write_image_data(&data).map_err(|e| e.to_string())
}
