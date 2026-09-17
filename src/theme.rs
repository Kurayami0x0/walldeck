//! Appearance settings: layout, colors, and motion.
//!
//! These are parsed out of the same file as everything else (see
//! `config.rs`) -- this module just owns the fields and their defaults.

use crate::config::Value;

/// `[r, g, b, a]`. `picker.rs` converts to the BGRA byte order the Argb8888
/// shm buffer wants at draw time -- the config file itself stays in the
/// order people actually think in ("#rrggbb" / "#rrggbbaa").
#[derive(Debug, Clone, Copy)]
pub struct Rgba(pub [u8; 4]);

pub fn parse_hex_color(s: &str) -> Option<Rgba> {
    let s = s.trim().trim_matches('"').trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok();
    match s.len() {
        6 => Some(Rgba([byte(0)?, byte(1)?, byte(2)?, 0xff])),
        8 => Some(Rgba([byte(0)?, byte(1)?, byte(2)?, byte(3)?])),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    // -- layout --
    pub window_width: i32,
    /// Height of the icon panel alone (thumbnails plus their padding); the
    /// search bar and its gap are added on top of this for the real surface.
    pub window_height: i32,
    pub icon_size: i32,
    pub spacing: i32,
    pub padding: i32,
    pub corner_radius: i32,
    /// Corner rounding for the icon panel itself, separate from the per-icon
    /// `corner_radius`.
    pub window_corner_radius: i32,
    pub search_bar_height: i32,
    /// Width of the search bar box. It's centered below the icon panel,
    /// not stretched to `window_width`, so this is independent of it.
    pub search_bar_width: i32,
    /// Vertical gap between the icon panel and the search bar below it.
    pub search_bar_gap: i32,
    /// Corner rounding for the search bar box, separate from
    /// `window_corner_radius`. Defaults to a full pill (half its height).
    pub search_bar_corner_radius: i32,

    // -- colors --
    pub background: Rgba,
    pub search_bar_background: Rgba,
    pub search_text_color: Rgba,
    pub highlight_border: Rgba,
    pub highlight_border_width: i32,
    /// 0-255. Non-highlighted thumbnails are drawn at this alpha (out of
    /// 255) so the highlighted one pops without needing a heavier border.
    pub dim_opacity: u8,

    // -- animation --
    pub open_duration_ms: f64,
    pub close_duration_ms: f64,
    /// The panel scales up from this factor (of its full size) while
    /// opening, and back down to it while closing -- a subtle "pop"
    /// instead of a flat fade. 1.0 disables the scale effect entirely.
    pub open_close_scale_from: f64,
    /// Cubic-bezier control points (x1, y1, x2, y2) for the open/close
    /// easing curve -- same convention as CSS `cubic-bezier()`. Default is
    /// the standard "ease-in-out" curve.
    pub open_close_curve: (f64, f64, f64, f64),
    /// How fast the rendered scroll position chases the logical scroll
    /// target, in 1/sec -- higher is snappier, lower is floatier.
    pub scroll_smoothing: f64,
    /// Same idea, for the sliding highlight ring.
    pub highlight_smoothing: f64,
    /// Momentum decay after touchpad/wheel input stops.
    pub momentum_half_life_ms: f64,
    /// Pixels scrolled per discrete mouse-wheel "click". Touchpad
    /// scrolling uses its own reported delta directly and ignores this.
    pub wheel_step: i32,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            window_width: 1179,
            window_height: 370,
            icon_size: 350,
            spacing: 15,
            padding: 10,
            corner_radius: 12,
            window_corner_radius: 20,
            search_bar_height: 44,
            search_bar_width: 300,
            search_bar_gap: 16,
            search_bar_corner_radius: 22,

            background: Rgba([0x1e, 0x1e, 0x2e, 0xeb]),
            search_bar_background: Rgba([0x18, 0x18, 0x25, 0xeb]),
            search_text_color: Rgba([0xcd, 0xd6, 0xf4, 0xff]),
            highlight_border: Rgba([0xf9, 0xe2, 0xaf, 0xff]),
            highlight_border_width: 3,
            dim_opacity: 190,

            open_duration_ms: 200.0,
            close_duration_ms: 160.0,
            open_close_scale_from: 0.92,
            open_close_curve: (0.42, 0.0, 0.58, 1.0), // CSS "ease-in-out"
            scroll_smoothing: 14.0,
            highlight_smoothing: 20.0,
            momentum_half_life_ms: 120.0,
            wheel_step: 60,
        }
    }
}

impl Theme {
    /// Apply a single key. Returns false if this module doesn't own it.
    pub fn set_key(&mut self, key: &str, v: &Value) -> bool {
        match key {
            "window_width" => { if let Some(n) = v.int() { self.window_width = n.max(1); } }
            "window_height" => { if let Some(n) = v.int() { self.window_height = n.max(1); } }
            "icon_size" => { if let Some(n) = v.int() { self.icon_size = n.max(1); } }
            "spacing" => { if let Some(n) = v.int() { self.spacing = n.max(0); } }
            "padding" => { if let Some(n) = v.int() { self.padding = n.max(0); } }
            "corner_radius" => { if let Some(n) = v.int() { self.corner_radius = n.max(0); } }
            "window_corner_radius" => { if let Some(n) = v.int() { self.window_corner_radius = n.max(0); } }
            "search_bar_height" => { if let Some(n) = v.int() { self.search_bar_height = n.max(0); } }
            "search_bar_width" => { if let Some(n) = v.int() { self.search_bar_width = n.max(1); } }
            "search_bar_gap" => { if let Some(n) = v.int() { self.search_bar_gap = n.max(0); } }
            "search_bar_corner_radius" => { if let Some(n) = v.int() { self.search_bar_corner_radius = n.max(0); } }

            "background" => { if let Some(c) = v.color() { self.background = c; } }
            "search_bar_background" => { if let Some(c) = v.color() { self.search_bar_background = c; } }
            "search_text_color" => { if let Some(c) = v.color() { self.search_text_color = c; } }
            "highlight_border" => { if let Some(c) = v.color() { self.highlight_border = c; } }
            "highlight_border_width" => { if let Some(n) = v.int() { self.highlight_border_width = n.max(0); } }
            "dim_opacity" => { if let Some(n) = v.byte() { self.dim_opacity = n; } }

            "open_duration_ms" => { if let Some(n) = v.float() { self.open_duration_ms = n.max(0.0); } }
            "close_duration_ms" => { if let Some(n) = v.float() { self.close_duration_ms = n.max(0.0); } }
            "open_close_scale_from" => { if let Some(n) = v.float() { self.open_close_scale_from = n.clamp(0.1, 1.0); } }
            "open_close_curve" => { if let Some(c) = v.curve() { self.open_close_curve = c; } }
            "scroll_smoothing" => { if let Some(n) = v.float() { self.scroll_smoothing = n.max(0.1); } }
            "highlight_smoothing" => { if let Some(n) = v.float() { self.highlight_smoothing = n.max(0.1); } }
            "momentum_half_life_ms" => { if let Some(n) = v.float() { self.momentum_half_life_ms = n.max(1.0); } }
            "wheel_step" => { if let Some(n) = v.int() { self.wheel_step = n; } }

            _ => return false,
        }
        true
    }

    /// Clamp radii against the boxes they round, so an oversized value
    /// degrades to "as round as this shape can be" instead of producing an
    /// inverted or garbage mask.
    pub fn clamp(&mut self) {
        self.corner_radius = self.corner_radius.min(self.icon_size / 2);
        self.window_corner_radius = self
            .window_corner_radius
            .min(self.window_width / 2)
            .min(self.window_height / 2);
        self.search_bar_width = self.search_bar_width.min(self.window_width).max(1);
        self.search_bar_corner_radius = self
            .search_bar_corner_radius
            .min(self.search_bar_width / 2)
            .min(self.search_bar_height / 2);
    }

    /// Total rendered surface height: the icon panel, then a gap, then the
    /// search bar sitting below it as its own floating pill.
    pub fn surface_height(&self) -> i32 {
        self.window_height + self.search_bar_gap + self.search_bar_height
    }

    /// Y offset where the icon row starts. The icon panel occupies the top
    /// of the surface on its own, so this is just its own top padding.
    pub fn icon_area_y0(&self) -> i32 {
        self.padding
    }

    /// Y offset where the search bar pill starts, below the panel and gap.
    pub fn search_bar_y0(&self) -> i32 {
        self.window_height + self.search_bar_gap
    }

    /// X offset where the search bar pill starts -- centered under the icon
    /// panel, since `search_bar_width` is independent of `window_width`.
    pub fn search_bar_x0(&self) -> i32 {
        (self.window_width - self.search_bar_width) / 2
    }
}
