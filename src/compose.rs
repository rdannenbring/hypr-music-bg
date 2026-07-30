//! Turning a square cover into a screen-shaped wallpaper.

use crate::config::{Layout, RenderConfig, RenderStyle};
use crate::monitors::{self, Monitor};
use anyhow::Result;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};

/// One rendered wallpaper and the monitor it belongs to.
pub struct Rendered {
    pub monitor: String,
    pub png: Vec<u8>,
}

/// Render `art` for every monitor.
pub fn render(art_bytes: &[u8], monitors: &[Monitor], cfg: &RenderConfig) -> Result<Vec<Rendered>> {
    let art = image::load_from_memory(art_bytes)?;

    match cfg.layout {
        Layout::PerMonitor => monitors
            .iter()
            .map(|m| {
                let (w, h) = m.logical_size();
                let canvas = compose_one(&art, w, h, cfg);
                Ok(Rendered {
                    monitor: m.name.clone(),
                    png: encode_png(&canvas)?,
                })
            })
            .collect(),

        Layout::Span => {
            // Build one canvas across the whole desktop, then slice each
            // monitor's viewport out of it, so the image reads as continuous
            // across the seam.
            let (min_x, min_y, total_w, total_h) = monitors::bounding_box(monitors);
            let canvas = compose_one(&art, total_w, total_h, cfg);

            monitors
                .iter()
                .map(|m| {
                    let (w, h) = m.logical_size();
                    let slice = image::imageops::crop_imm(
                        &canvas,
                        (m.x - min_x).max(0) as u32,
                        (m.y - min_y).max(0) as u32,
                        w,
                        h,
                    )
                    .to_image();
                    Ok(Rendered {
                        monitor: m.name.clone(),
                        png: encode_png(&slice)?,
                    })
                })
                .collect()
        }
    }
}

fn compose_one(art: &DynamicImage, width: u32, height: u32, cfg: &RenderConfig) -> RgbaImage {
    match cfg.style {
        RenderStyle::Fill => fill(art, width, height),
        RenderStyle::Fit => fit(art, width, height, cfg),
        RenderStyle::Blur => blur_backdrop(art, width, height, cfg),
    }
}

/// Scale to cover the screen and crop the overflow.
fn fill(art: &DynamicImage, width: u32, height: u32) -> RgbaImage {
    let (aw, ah) = art.dimensions();
    let scale = (width as f32 / aw as f32).max(height as f32 / ah as f32);
    let (sw, sh) = (
        (aw as f32 * scale).ceil() as u32,
        (ah as f32 * scale).ceil() as u32,
    );

    let scaled = art.resize_exact(sw.max(1), sh.max(1), FilterType::Lanczos3);
    image::imageops::crop_imm(
        &scaled.to_rgba8(),
        (sw.saturating_sub(width)) / 2,
        (sh.saturating_sub(height)) / 2,
        width,
        height,
    )
    .to_image()
}

/// Letterbox the cover against a flat colour sampled from its own edges.
fn fit(art: &DynamicImage, width: u32, height: u32, cfg: &RenderConfig) -> RgbaImage {
    let mut canvas = RgbaImage::from_pixel(width, height, average_edge_color(art, cfg.darken));
    let scaled = art.resize(width, height, FilterType::Lanczos3).to_rgba8();
    let (sw, sh) = scaled.dimensions();
    image::imageops::overlay(
        &mut canvas,
        &scaled,
        ((width.saturating_sub(sw)) / 2) as i64,
        ((height.saturating_sub(sh)) / 2) as i64,
    );
    canvas
}

/// The good one: a blurred, zoomed copy of the cover as backdrop, with the
/// sharp cover centred on top.
///
/// The backdrop is produced by shrinking hard, blurring the tiny image, then
/// scaling back up. Blurring at full resolution is quadratic in the radius and
/// visibly stalls on a 5120x1440 canvas; doing it at 1/10th scale is
/// indistinguishable and effectively free. This also means low-resolution
/// source art still yields a clean backdrop, since the detail is being thrown
/// away regardless.
fn blur_backdrop(art: &DynamicImage, width: u32, height: u32, cfg: &RenderConfig) -> RgbaImage {
    let small_w = (width / 10).max(16);
    let small_h = (height / 10).max(16);

    let mut backdrop = fill(art, small_w, small_h);
    if cfg.blur_strength > 0.0 {
        backdrop = image::imageops::blur(&backdrop, cfg.blur_strength.max(0.1));
    }

    let mut canvas = DynamicImage::ImageRgba8(backdrop)
        .resize_exact(width, height, FilterType::Triangle)
        .to_rgba8();

    if cfg.darken > 0.0 {
        darken(&mut canvas, cfg.darken.clamp(0.0, 1.0));
    }

    // Sharp cover on top, sized against the screen's shorter edge so it stays
    // fully visible on both ultrawide and portrait displays.
    let target = ((width.min(height) as f32) * cfg.cover_scale.clamp(0.05, 1.0)) as u32;
    if target > 0 {
        let cover = art.resize(target, target, FilterType::Lanczos3).to_rgba8();
        let (cw, ch) = cover.dimensions();
        image::imageops::overlay(
            &mut canvas,
            &cover,
            ((width.saturating_sub(cw)) / 2) as i64,
            ((height.saturating_sub(ch)) / 2) as i64,
        );
    }

    canvas
}

fn darken(image: &mut RgbaImage, amount: f32) {
    let factor = 1.0 - amount;
    for pixel in image.pixels_mut() {
        pixel.0[0] = (pixel.0[0] as f32 * factor) as u8;
        pixel.0[1] = (pixel.0[1] as f32 * factor) as u8;
        pixel.0[2] = (pixel.0[2] as f32 * factor) as u8;
    }
}

/// Mean colour of the cover's border, which reads better behind a letterboxed
/// image than a hardcoded black.
fn average_edge_color(art: &DynamicImage, darken_amount: f32) -> Rgba<u8> {
    let rgba = art.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Rgba([0, 0, 0, 255]);
    }

    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for x in 0..w {
        for y in [0, h - 1] {
            let p = rgba.get_pixel(x, y);
            r += p.0[0] as u64;
            g += p.0[1] as u64;
            b += p.0[2] as u64;
            n += 1;
        }
    }
    for y in 0..h {
        for x in [0, w - 1] {
            let p = rgba.get_pixel(x, y);
            r += p.0[0] as u64;
            g += p.0[1] as u64;
            b += p.0[2] as u64;
            n += 1;
        }
    }

    let n = n.max(1);
    let factor = 1.0 - darken_amount.clamp(0.0, 1.0);
    Rgba([
        ((r / n) as f32 * factor) as u8,
        ((g / n) as f32 * factor) as u8,
        ((b / n) as f32 * factor) as u8,
        255,
    ])
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitors::Monitor;

    fn test_art() -> Vec<u8> {
        let img = RgbaImage::from_fn(300, 300, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn monitors() -> Vec<Monitor> {
        vec![
            Monitor {
                name: "DP-2".into(),
                width: 2560,
                height: 1440,
                x: 0,
                y: 0,
                scale: 1.0,
                transform: 0,
            },
            Monitor {
                name: "DP-1".into(),
                width: 2560,
                height: 1440,
                x: 2560,
                y: 0,
                scale: 1.0,
                transform: 0,
            },
        ]
    }

    /// A 300x300 cover must still produce full-resolution 2560x1440 output —
    /// this is the low-res-source case that motivated the whole design.
    #[test]
    fn renders_each_monitor_at_full_resolution() {
        let cfg = RenderConfig::default();
        let out = render(&test_art(), &monitors(), &cfg).unwrap();

        assert_eq!(out.len(), 2);
        for rendered in &out {
            let decoded = image::load_from_memory(&rendered.png).unwrap();
            assert_eq!(decoded.dimensions(), (2560, 1440));
        }
        assert_eq!(out[0].monitor, "DP-2");
    }

    #[test]
    fn span_layout_slices_the_seam_correctly() {
        let cfg = RenderConfig {
            layout: Layout::Span,
            ..RenderConfig::default()
        };
        let out = render(&test_art(), &monitors(), &cfg).unwrap();

        assert_eq!(out.len(), 2);
        for rendered in &out {
            let decoded = image::load_from_memory(&rendered.png).unwrap();
            assert_eq!(decoded.dimensions(), (2560, 1440));
        }
    }

    #[test]
    fn fill_style_crops_rather_than_distorts() {
        let art = image::load_from_memory(&test_art()).unwrap();
        let out = fill(&art, 2560, 1440);
        assert_eq!(out.dimensions(), (2560, 1440));
    }
}
