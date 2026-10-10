//! Head-unit configuration: identity strings and video geometry.
//!
//! The phone always encodes one of a few fixed resolutions (800x480,
//! 1280x720, 1920x1080). To fill a panel of another aspect ratio the head unit
//! declares `width_margin` / `height_margin` in its VideoConfiguration: the
//! phone then lays its UI out only in the centred `(W - wm) x (H - hm)`
//! rectangle and leaves the margins black (openauto
//! `VideoMediaSinkService::fillFeatures`). The page crops the margins away and
//! scales the content rectangle to the panel, so nothing is distorted.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    R800x480,
    R1280x720,
    R1920x1080,
}

impl Resolution {
    pub fn size(self) -> (u32, u32) {
        match self {
            Resolution::R800x480 => (800, 480),
            Resolution::R1280x720 => (1280, 720),
            Resolution::R1920x1080 => (1920, 1080),
        }
    }

    /// `VideoCodecResolutionType` value.
    pub fn proto(self) -> i32 {
        use crate::aa::proto::resolution::*;
        match self {
            Resolution::R800x480 => R800X480,
            Resolution::R1280x720 => R1280X720,
            Resolution::R1920x1080 => R1920X1080,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "800x480" => Some(Resolution::R800x480),
            "1280x720" => Some(Resolution::R1280x720),
            "1920x1080" => Some(Resolution::R1920x1080),
            _ => None,
        }
    }
}

/// Where the phone's UI sits inside the encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone)]
pub struct HuConfig {
    pub resolution: Resolution,
    pub fps60: bool,
    /// Size (CSS px) of the page area the video is shown in.
    pub panel: (u32, u32),
    /// UI density the phone lays out with (lower = more content, smaller text).
    pub density: u32,
    pub display_name: String,
    pub make: String,
    pub model: String,
    pub year: String,
    pub vehicle_id: String,
    pub hu_make: String,
    pub hu_model: String,
    pub sw_build: String,
    pub sw_version: String,
    pub driver_left: bool,
    /// Send VideoFocusNotification(PROJECTED) right after answering the video
    /// Setup. The original f1xpl openauto did; the current opencardev fork
    /// only answers the phone's VideoFocusRequest. Harmless either way
    /// (hypothesis — to confirm with the phone).
    pub video_focus_after_setup: bool,
}

impl Default for HuConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::R1280x720,
            fps60: false,
            // Dashboard app area on the 1200x512 kiosk: 1200 - 92 (dock) wide,
            // min(512 - 44 (status bar), 460) tall (app/frontend/css/dashboard.css .app-frame).
            panel: (1108, 460),
            density: 160,
            display_name: "RasCar".into(),
            make: "Jaguar".into(),
            model: "RasCar".into(),
            year: "2026".into(),
            vehicle_id: "RASCAR-0001".into(),
            hu_make: "OrangeCar".into(),
            hu_model: "RasCar AA".into(),
            sw_build: "1".into(),
            sw_version: "0.1".into(),
            driver_left: true,
            video_focus_after_setup: true,
        }
    }
}

impl HuConfig {
    /// `(width_margin, height_margin)` making the phone's UI match the panel's
    /// aspect ratio. Margins are kept even so the content is pixel-centred.
    pub fn margins(&self) -> (u32, u32) {
        let (w, h) = self.resolution.size();
        let (pw, ph) = (self.panel.0.max(1) as u64, self.panel.1.max(1) as u64);
        let even = |m: u32| if m % 2 == 1 { m + 1 } else { m };
        if pw * h as u64 >= ph * w as u64 {
            // Panel is wider than the frame: trim height.
            let content_h = ((w as u64 * ph + pw / 2) / pw) as u32;
            (0, even(h.saturating_sub(content_h)))
        } else {
            let content_w = ((h as u64 * pw + ph / 2) / ph) as u32;
            (even(w.saturating_sub(content_w)), 0)
        }
    }

    /// The rectangle of the frame the phone draws into.
    pub fn content_rect(&self) -> Rect {
        let (w, h) = self.resolution.size();
        let (wm, hm) = self.margins();
        Rect { x: wm / 2, y: hm / 2, w: w - wm, h: h - hm }
    }

    /// Normalised page coordinates (0..1 over the shown content) to
    /// touchscreen pixels, relative to the CONTENT the phone draws (margins
    /// excluded). Measured 2026-10-05 on an Android 16 phone with height margin 190:
    /// adding the 95 px margin back put every tap ~95 px too low — the search
    /// bar tap landed on the map and the launcher (lowest button) fell outside.
    pub fn touch_point(&self, u: f64, v: f64) -> (u32, u32) {
        let r = self.content_rect();
        let u = if u.is_finite() { u.clamp(0.0, 1.0) } else { 0.0 };
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let x = u * (r.w.saturating_sub(1)) as f64;
        let y = v * (r.h.saturating_sub(1)) as f64;
        (x.round() as u32, y.round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_panel_margins() {
        let c = HuConfig::default();
        // 1280 * 460 / 1108 = 531.4 → 531 → margin 189 → even 190
        assert_eq!(c.margins(), (0, 190));
        assert_eq!(c.content_rect(), Rect { x: 0, y: 95, w: 1280, h: 530 });
    }

    #[test]
    fn narrow_panel_trims_width() {
        let c = HuConfig { panel: (480, 480), ..HuConfig::default() };
        assert_eq!(c.margins(), (560, 0));
        assert_eq!(c.content_rect(), Rect { x: 280, y: 0, w: 720, h: 720 });
    }

    #[test]
    fn exact_aspect_has_no_margin() {
        let c = HuConfig { panel: (1280, 720), ..HuConfig::default() };
        assert_eq!(c.margins(), (0, 0));
    }

    #[test]
    fn touch_maps_into_content_rect() {
        let c = HuConfig::default();
        assert_eq!(c.touch_point(0.0, 0.0), (0, 0));
        assert_eq!(c.touch_point(1.0, 1.0), (1279, 529));
        assert_eq!(c.touch_point(0.5, 0.5), (640, 265));
        // Out-of-range and NaN are clamped, never wrap.
        assert_eq!(c.touch_point(-3.0, f64::NAN), (0, 0));
        assert_eq!(c.touch_point(9.0, 9.0), (1279, 529));
    }
}
