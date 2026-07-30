//! Monitor geometry, read from Hyprland.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    #[serde(default = "default_scale")]
    pub scale: f32,
    #[serde(default)]
    pub transform: u8,
}

fn default_scale() -> f32 {
    1.0
}

impl Monitor {
    /// Logical size, accounting for a rotated panel. Hyprland reports the
    /// physical mode, so a 90/270-degree transform swaps the axes.
    pub fn logical_size(&self) -> (u32, u32) {
        if matches!(self.transform, 1 | 3 | 5 | 7) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

pub async fn detect() -> Result<Vec<Monitor>> {
    let output = tokio::process::Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .await
        .context("running `hyprctl monitors -j` (is Hyprland running?)")?;

    if !output.status.success() {
        anyhow::bail!(
            "hyprctl exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let monitors: Vec<Monitor> =
        serde_json::from_slice(&output.stdout).context("parsing hyprctl monitor output")?;

    if monitors.is_empty() {
        anyhow::bail!("hyprctl reported no monitors");
    }

    Ok(monitors)
}

/// Bounding box of the whole layout, for `layout = "span"`.
pub fn bounding_box(monitors: &[Monitor]) -> (i32, i32, u32, u32) {
    let min_x = monitors.iter().map(|m| m.x).min().unwrap_or(0);
    let min_y = monitors.iter().map(|m| m.y).min().unwrap_or(0);
    let max_x = monitors
        .iter()
        .map(|m| m.x + m.logical_size().0 as i32)
        .max()
        .unwrap_or(0);
    let max_y = monitors
        .iter()
        .map(|m| m.y + m.logical_size().1 as i32)
        .max()
        .unwrap_or(0);

    (
        min_x,
        min_y,
        (max_x - min_x).max(1) as u32,
        (max_y - min_y).max(1) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: i32, w: u32, h: u32) -> Monitor {
        Monitor {
            name: name.into(),
            width: w,
            height: h,
            x,
            y: 0,
            scale: 1.0,
            transform: 0,
        }
    }

    #[test]
    fn spans_a_side_by_side_layout() {
        let monitors = vec![
            monitor("DP-2", 0, 2560, 1440),
            monitor("DP-1", 2560, 2560, 1440),
        ];
        assert_eq!(bounding_box(&monitors), (0, 0, 5120, 1440));
    }

    #[test]
    fn rotated_panels_swap_axes() {
        let mut m = monitor("DP-3", 0, 2560, 1440);
        m.transform = 1;
        assert_eq!(m.logical_size(), (1440, 2560));
    }
}
