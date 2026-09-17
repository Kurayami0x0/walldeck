//! Layer-shell wallpaper picker built on smithay-client-toolkit.
//!
//! This module's only job is to show the cached thumbnails and return the
//! filename the user chose (or `None` on Escape/close). Applying that choice
//! is somebody else's problem -- see `wallpaper.rs`.
//!
//! Compositing: the canvas is **premultiplied** BGRA, matching what a
//! `wl_shm` `Argb8888` buffer is defined to hold. Every shape is composited
//! "over" the canvas with its own coverage-scaled alpha, which is what makes
//! antialiased edges blend correctly instead of haloing.
//!
//! Render pacing: every redraw goes through `draw()`, which is the only
//! place that attaches a buffer, requests the next frame callback, and
//! commits. `frame_pending` guarantees at most one outstanding commit at a
//! time -- input handlers never draw directly, they just update state and
//! call `kick()`, which draws immediately *only if nothing is already in
//! flight*. Otherwise the pending frame callback's `tick()` picks up the
//! change on its own.
//!
//! Motion model: nothing snaps. Scroll position and the highlight ring are
//! both `Eased` values -- a logical target set instantly by input, and a
//! rendered value that chases it every frame via exponential smoothing.
//! Touchpad/wheel momentum works by continuing to nudge the *target* after
//! input stops, decaying over time. Opening and closing run a fixed-duration
//! cubic-bezier ease (same convention as CSS `cubic-bezier()`) over both
//! alpha and a scale-from-center "pop", and the picker doesn't actually
//! return a result until the closing animation finishes.

use crate::config::Config;
use crate::font;
use crate::images::thumb_path;
use crate::theme::Theme;

use image::{imageops::FilterType, RgbaImage};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

const MIN_VELOCITY: f64 = 4.0; // px/sec, below this momentum just stops
const MAX_VELOCITY: f64 = 6000.0; // px/sec, clamp for noisy/spiky input timestamps
const EASE_SETTLE_EPS: f64 = 0.05; // px, close enough to target to call it "arrived"

// Keyboard repeat, matching typical desktop defaults (~25 repeats/sec after
// a 400ms initial delay) since sctk doesn't auto-repeat for us.
const KEY_REPEAT_DELAY_MS: u32 = 400;
const KEY_REPEAT_INTERVAL_MS: u32 = 40;

const SEARCH_FONT_SCALE: i32 = 3; // each font pixel drawn as a SCALExSCALE block

/// Standard CSS-style cubic-bezier easing: control points (x1,y1) and
/// (x2,y2), endpoints implicitly fixed at (0,0) and (1,1). `t` is time
/// progress 0..1; returns eased progress 0..1. Solved via a few Newton's
/// method iterations on the x(u)=t curve, same approach browsers use.
fn cubic_bezier(t: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    let component = |u: f64, p1: f64, p2: f64| {
        let mu = 1.0 - u;
        3.0 * mu * mu * u * p1 + 3.0 * mu * u * u * p2 + u * u * u
    };
    let derivative = |u: f64, p1: f64, p2: f64| {
        let mu = 1.0 - u;
        3.0 * mu * mu * p1 + 6.0 * mu * u * (p2 - p1) + 3.0 * u * u * (1.0 - p2)
    };
    let mut u = t;
    for _ in 0..8 {
        let x = component(u, x1, x2) - t;
        let dx = derivative(u, x1, x2);
        if dx.abs() < 1e-6 {
            break;
        }
        u = (u - x / dx).clamp(0.0, 1.0);
    }
    component(u, y1, y2)
}

/// A value that chases a target every frame via exponential smoothing
/// instead of snapping to it -- the same primitive drives both scroll
/// position and the sliding highlight ring.
struct Eased {
    value: f64,
    target: f64,
}

impl Eased {
    fn new(v: f64) -> Self {
        Eased { value: v, target: v }
    }

    fn set_target(&mut self, t: f64) {
        self.target = t;
    }

    /// Advances toward the target; returns `false` once it's close enough
    /// to consider settled (so callers can stop scheduling frames).
    fn advance(&mut self, dt: f64, rate: f64) -> bool {
        let diff = self.target - self.value;
        if diff.abs() < EASE_SETTLE_EPS {
            self.value = self.target;
            return false;
        }
        self.value += diff * (1.0 - (-rate * dt).exp());
        true
    }
}

enum AnimPhase {
    Opening,
    Idle,
    Closing,
}

struct Entry {
    filename: String,
    rgba: RgbaImage, // icon_size x icon_size, decoded once up front
}

/// A directional key held down, for synthesizing repeat while it stays
/// pressed (Wayland only sends a single press event per physical press).
struct HeldKey {
    dir: i32,
    pressed_at: u32,
    last_repeat: u32,
}

pub struct Picker {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    theme: Theme,

    entries: Vec<Entry>,
    search: String,
    /// Indices into `entries` that match the current search -- everything
    /// layout/navigation/hit-testing touches is a *position within this
    /// list*; it's only mapped back to an absolute `entries` index when a
    /// selection is finalized.
    filtered: Vec<usize>,

    scroll: Eased,
    velocity: f64, // px/sec, drives momentum after scrolling input stops
    scroll_settled: bool,
    last_axis_time: Option<u32>,

    highlight: Eased, // absolute content-space x of the selection ring
    highlight_settled: bool,

    hover: Option<usize>, // position within `filtered`
    kbd_index: usize,     // position within `filtered`
    held_key: Option<HeldKey>,

    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_pos: (f64, f64),

    anim_phase: AnimPhase,
    phase_start: Option<u32>,
    alpha: f64,
    scale: f64,
    pending_result: Option<Selection>,

    configured: bool,
    needs_redraw: bool,
    frame_pending: bool,
    last_frame_time: Option<u32>,
    result: Option<Selection>,
}

#[derive(Clone, Copy)]
enum Selection {
    Picked(usize), // absolute index into `entries`
    Cancelled,
}

/// Open the picker and block until the user selects a wallpaper or cancels
/// (including the closing animation). Returns `None` on cancel, matching
/// the script's `exit 0` on empty choice.
pub fn pick(cfg: &Config, images: &[String]) -> Option<String> {
    let theme = cfg.theme.clone();

    let entries: Vec<Entry> = images
        .iter()
        .filter_map(|name| {
            let path = thumb_path(cfg, name);
            let img = image::open(&path).ok()?;
            let rgba = img
                .resize_to_fill(theme.icon_size as u32, theme.icon_size as u32, FilterType::Triangle)
                .to_rgba8();
            Some(Entry {
                filename: name.clone(),
                rgba,
            })
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    let conn = Connection::connect_to_env().expect("no wayland connection");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("registry init failed");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor missing");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell missing");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm missing");

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("walldeck"),
        None,
    );
    let surface_h = theme.surface_height();
    layer.set_anchor(Anchor::empty()); // centered
    layer.set_size(theme.window_width as u32, surface_h as u32);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.commit();

    // Headroom for a few in-flight buffers so scrolling never has to wait
    // on a mid-animation pool growth.
    let pool = SlotPool::new((theme.window_width * surface_h * 4 * 3) as usize, &shm)
        .expect("shm pool alloc failed");

    let filtered: Vec<usize> = (0..entries.len()).collect();
    let start_highlight = theme.padding as f64; // index 0's x

    let mut state = Picker {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        theme,
        entries,
        search: String::new(),
        filtered,
        scroll: Eased::new(0.0),
        velocity: 0.0,
        scroll_settled: true,
        last_axis_time: None,
        highlight: Eased::new(start_highlight),
        highlight_settled: true,
        hover: None,
        kbd_index: 0,
        held_key: None,
        pointer: None,
        keyboard: None,
        pointer_pos: (0.0, 0.0),
        anim_phase: AnimPhase::Opening,
        phase_start: None,
        alpha: 0.0,
        scale: 1.0,
        pending_result: None,
        configured: false,
        needs_redraw: true,
        frame_pending: false,
        last_frame_time: None,
        result: None,
    };
    state.scale = state.theme.open_close_scale_from;

    while state.result.is_none() {
        event_queue.blocking_dispatch(&mut state).ok()?;
        if state.configured {
            state.kick(&qh);
        }
    }

    match state.result {
        Some(Selection::Picked(i)) => Some(state.entries[i].filename.clone()),
        _ => None,
    }
}

impl Picker {
    fn stride(&self) -> i32 {
        self.theme.icon_size + self.theme.spacing
    }

    /// Absolute content-space x for the cell at position `pos` within
    /// `filtered` (not an `entries` index).
    fn cell_x(&self, pos: usize) -> f64 {
        (self.theme.padding + pos as i32 * self.stride()) as f64
    }

    /// Hit-tests against the *rendered* scroll position, not the logical
    /// target, so clicks land on what's actually visible mid-scroll.
    /// Returns a position within `filtered`.
    fn cell_at(&self, x: f64, y: f64) -> Option<usize> {
        let t = &self.theme;
        let icon_y0 = t.icon_area_y0();
        if y < icon_y0 as f64 || y > (icon_y0 + t.icon_size) as f64 {
            return None;
        }
        let rel_x = x + self.scroll.value - t.padding as f64;
        if rel_x < 0.0 {
            return None;
        }
        let stride = self.stride();
        let idx = (rel_x / stride as f64) as i32;
        if rel_x - ((idx * stride) as f64) < t.icon_size as f64 && (idx as usize) < self.filtered.len() {
            Some(idx as usize)
        } else {
            None
        }
    }

    fn max_scroll(&self) -> f64 {
        let t = &self.theme;
        let n = self.filtered.len() as i32;
        let content_w = t.padding * 2 + n * t.icon_size + (n - 1).max(0) * t.spacing;
        (content_w - t.window_width).max(0) as f64
    }

    /// Sets the logical scroll target (clamped); the rendered position
    /// eases toward it on its own every frame. Kills momentum on hitting a
    /// bound so it doesn't keep trying to push past the edge.
    fn set_scroll_target(&mut self, new_target: f64) {
        let max = self.max_scroll();
        let clamped = new_target.clamp(0.0, max);
        if clamped != new_target {
            self.velocity = 0.0;
        }
        self.scroll.set_target(clamped);
    }

    /// Nudge scroll so cell `pos` (within `filtered`) is fully within view
    /// -- used after keyboard navigation, since arrow keys move the
    /// selection, not the viewport directly.
    fn scroll_into_view(&mut self, pos: usize) {
        let t = &self.theme;
        let cell_start = self.cell_x(pos);
        let cell_end = cell_start + t.icon_size as f64;
        if cell_start < self.scroll.target {
            self.set_scroll_target(cell_start - t.padding as f64);
        } else if cell_end > self.scroll.target + t.window_width as f64 {
            self.set_scroll_target(cell_end - t.window_width as f64 + t.padding as f64);
        }
    }

    /// Move the keyboard cursor by `delta`, wrapping past either end --
    /// past the last entry loops to the first, and back past the first
    /// loops to the last, so arrow-key navigation cycles continuously
    /// instead of stopping dead at the boundaries.
    fn move_kbd_cursor(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let new_idx = wrap_index(self.kbd_index, delta, self.filtered.len());
        if new_idx != self.kbd_index {
            self.kbd_index = new_idx;
            self.hover = None; // keyboard nav takes over from mouse hover
            self.scroll_into_view(new_idx);
        }
    }

    /// Move the keyboard cursor to an absolute position, clamped into range
    /// (not wrapped) -- used for Home/End, where "past the end" should mean
    /// "the end", not "wrap around".
    fn set_kbd_cursor(&mut self, index: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let new_idx = index.clamp(0, self.filtered.len() as i32 - 1) as usize;
        if new_idx != self.kbd_index {
            self.kbd_index = new_idx;
            self.hover = None; // keyboard nav takes over from mouse hover
            self.scroll_into_view(new_idx);
        }
    }

    /// Recompute `filtered` from the current search text, then reset
    /// selection/scroll for the new (usually much shorter) list.
    fn apply_search(&mut self) {
        let needle = self.search.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| needle.is_empty() || e.filename.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.kbd_index = 0;
        self.hover = None;
        self.set_scroll_target(0.0);
        if let Some(pos) = self.filtered.first() {
            let _ = pos; // just documents intent: highlight target recalculated in advance()
        }
        self.needs_redraw = true;
    }

    /// Begin the closing animation instead of resolving immediately -- the
    /// real `result` is only set once it finishes, in `advance()`.
    fn begin_close(&mut self, selection: Selection) {
        if matches!(self.anim_phase, AnimPhase::Closing) {
            return; // already closing, ignore further input
        }
        self.pending_result = Some(selection);
        self.anim_phase = AnimPhase::Closing;
        self.phase_start = None;
        self.held_key = None;
    }

    fn is_closing(&self) -> bool {
        matches!(self.anim_phase, AnimPhase::Closing)
    }

    /// True while there's a reason to keep animating without new input:
    /// scroll/highlight still easing, momentum still has speed, a nav key
    /// is held for repeat, or an open/close animation is in progress.
    fn animating(&self) -> bool {
        !self.scroll_settled
            || !self.highlight_settled
            || self.velocity.abs() > MIN_VELOCITY
            || self.held_key.is_some()
            || !matches!(self.anim_phase, AnimPhase::Idle)
    }

    /// Advance all physics/animation by one frame tick. Called from the
    /// frame callback, so `time_ms` is the compositor's own clock -- same
    /// domain as pointer/key event timestamps.
    fn advance(&mut self, time_ms: u32) {
        let dt = match self.last_frame_time {
            Some(last) => ((time_ms.wrapping_sub(last)) as f64 / 1000.0).clamp(0.0, 0.1),
            None => 1.0 / 60.0,
        };
        self.last_frame_time = Some(time_ms);

        // Scroll momentum: keep nudging the *target*; the rendered value
        // trails behind it via its own easing below.
        if self.velocity.abs() > MIN_VELOCITY {
            let decay = 0.5_f64.powf(dt / (self.theme.momentum_half_life_ms / 1000.0));
            self.velocity *= decay;
            self.set_scroll_target(self.scroll.target + self.velocity * dt);
            if self.velocity.abs() <= MIN_VELOCITY {
                self.velocity = 0.0;
            }
        }
        let scroll_moving = self.scroll.advance(dt, self.theme.scroll_smoothing);
        if scroll_moving {
            self.needs_redraw = true;
        }
        self.scroll_settled = !scroll_moving;

        // Sliding highlight ring chases whichever cell is currently
        // selected (mouse hover wins, keyboard cursor otherwise).
        if let Some(&target_pos) = self.hover.as_ref().or(Some(&self.kbd_index)) {
            if !self.filtered.is_empty() {
                self.highlight.set_target(self.cell_x(target_pos));
            }
        }
        let highlight_moving = self.highlight.advance(dt, self.theme.highlight_smoothing);
        if highlight_moving {
            self.needs_redraw = true;
        }
        self.highlight_settled = !highlight_moving;

        // Key repeat.
        if let Some(held) = &mut self.held_key {
            let since_press = time_ms.wrapping_sub(held.pressed_at);
            let since_repeat = time_ms.wrapping_sub(held.last_repeat);
            if since_press >= KEY_REPEAT_DELAY_MS && since_repeat >= KEY_REPEAT_INTERVAL_MS {
                held.last_repeat = time_ms;
                let dir = held.dir;
                self.move_kbd_cursor(dir);
                self.needs_redraw = true;
            }
        }

        // Open/close: cubic-bezier ease over both alpha and a
        // scale-from-center "pop", not a flat opacity fade.
        let (x1, y1, x2, y2) = self.theme.open_close_curve;
        let scale_from = self.theme.open_close_scale_from;
        match self.anim_phase {
            AnimPhase::Opening => {
                let started = *self.phase_start.get_or_insert(time_ms);
                let t = (time_ms.wrapping_sub(started) as f64 / self.theme.open_duration_ms).clamp(0.0, 1.0);
                let eased = cubic_bezier(t, x1, y1, x2, y2);
                self.alpha = eased;
                self.scale = scale_from + (1.0 - scale_from) * eased;
                self.needs_redraw = true;
                if t >= 1.0 {
                    self.anim_phase = AnimPhase::Idle;
                }
            }
            AnimPhase::Closing => {
                let started = *self.phase_start.get_or_insert(time_ms);
                let t = (time_ms.wrapping_sub(started) as f64 / self.theme.close_duration_ms).clamp(0.0, 1.0);
                let eased = cubic_bezier(t, x1, y1, x2, y2);
                self.alpha = 1.0 - eased;
                self.scale = 1.0 - (1.0 - scale_from) * eased;
                self.needs_redraw = true;
                if t >= 1.0 {
                    self.result = self.pending_result.take().or(Some(Selection::Cancelled));
                }
            }
            AnimPhase::Idle => {}
        }
    }

    /// Draw now if nothing's already in flight; otherwise the pending
    /// frame callback's `advance()` will notice `needs_redraw`/
    /// `animating()` and draw when it fires. Call this after any input
    /// that changes state -- never call `draw()` directly from a handler.
    fn kick(&mut self, qh: &QueueHandle<Self>) {
        if !self.frame_pending && (self.needs_redraw || self.animating()) {
            self.draw(qh);
        }
    }

    /// Renders the full panel (search bar, icons, highlight ring) into
    /// `content` at natural size -- no alpha/scale applied here, that
    /// happens in the final composite pass in `draw()`.
    fn render_content(&self, content: &mut [u8]) {
        let t = &self.theme;
        let w = t.window_width;
        let h = t.surface_height();

        // `content` arrives zeroed (fully transparent) from `draw()`, which
        // is exactly what we want for the gap between the icon panel and
        // the search pill below it, and for the margin where the
        // (narrower) pill doesn't reach the panel's full width.

        // Icon panel: its own rounded rect, full window_width, occupying
        // the top of the surface. Corner pixels get fractional coverage
        // rather than a hard in/out mask, which is what makes the curve
        // read as smooth instead of stepped.
        let bg = t.background.0;
        let bg_rgb = [bg[0], bg[1], bg[2]];
        let bg_alpha = bg[3] as f64 / 255.0;
        for y in 0..t.window_height {
            for x in 0..w {
                let cov = rounded_rect_coverage(x, y, w, t.window_height, t.window_corner_radius);
                if cov <= 0.0 {
                    continue;
                }
                let di = ((y * w + x) * 4) as usize;
                blend_over(content, di, bg_rgb, bg_alpha * cov);
            }
        }

        // Search bar: a separate, narrower pill centered below the panel
        // with a gap, its own rounding independent of the panel's.
        let sbar = t.search_bar_background.0;
        let sbar_rgb = [sbar[0], sbar[1], sbar[2]];
        let sbar_alpha = sbar[3] as f64 / 255.0;
        let sbar_x0 = t.search_bar_x0();
        let sbar_y0 = t.search_bar_y0();
        for ly in 0..t.search_bar_height {
            for lx in 0..t.search_bar_width {
                let cov = rounded_rect_coverage(
                    lx,
                    ly,
                    t.search_bar_width,
                    t.search_bar_height,
                    t.search_bar_corner_radius,
                );
                if cov <= 0.0 {
                    continue;
                }
                let (x, y) = (sbar_x0 + lx, sbar_y0 + ly);
                if x < 0 || x >= w || y < 0 || y >= h {
                    continue;
                }
                let di = ((y * w + x) * 4) as usize;
                blend_over(content, di, sbar_rgb, sbar_alpha * cov);
            }
        }

        // Search text, left-aligned within the pill, with a small cursor
        // bar after it.
        let text_color = t.search_text_color.0;
        let text_bgr = [text_color[2], text_color[1], text_color[0]];
        let text_x0 = sbar_x0 + t.padding;
        let text_y0 = sbar_y0 + (t.search_bar_height - font::GLYPH_HEIGHT * SEARCH_FONT_SCALE) / 2;
        let cursor_x = draw_text(content, w, h, &self.search, text_x0, text_y0, SEARCH_FONT_SCALE, text_bgr);
        draw_rect(content, w, h, cursor_x + 2, text_y0, 2, font::GLYPH_HEIGHT * SEARCH_FONT_SCALE, text_bgr);

        // Icons.
        let selected_pos = self.hover.unwrap_or(self.kbd_index);
        let scroll = self.scroll.value.round() as i32;
        let icon_y0 = t.icon_area_y0();
        for (pos, &entry_idx) in self.filtered.iter().enumerate() {
            let entry = &self.entries[entry_idx];
            let x0 = t.padding + pos as i32 * (t.icon_size + t.spacing) - scroll;
            if x0 + t.icon_size < 0 || x0 > w {
                continue; // off-screen, skip compositing
            }
            let dim = if pos == selected_pos { 1.0 } else { t.dim_opacity as f64 / 255.0 };
            blit_rounded(content, w, h, &entry.rgba, x0, icon_y0, t.corner_radius, dim);
        }

        // Sliding highlight ring, drawn once on top of everything so it
        // reads as a single indicator moving between cells rather than a
        // per-icon border popping on and off.
        if !self.filtered.is_empty() {
            let ring_x = (self.highlight.value.round() as i32) - scroll;
            draw_ring(content, w, h, ring_x, icon_y0, t.icon_size, t.corner_radius, t.highlight_border_width, t.highlight_border.0);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        self.needs_redraw = false;
        let t = self.theme.clone();
        let w = t.window_width;
        let h = t.surface_height();

        let mut content = vec![0u8; (w * h * 4) as usize];
        self.render_content(&mut content);

        let (buffer, canvas) = self
            .pool
            .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
            .expect("buffer alloc failed");

        let scale = self.scale;
        let alpha = self.alpha.clamp(0.0, 1.0);

        if (scale - 1.0).abs() < 0.001 && alpha >= 0.999 {
            // `canvas` can be a few bytes larger than `content`: SlotPool
            // pads the underlying slot up to a 64-byte alignment boundary
            // internally but doesn't trim `create_buffer`'s returned slice
            // back down to `width * height * stride` (unlike `Buffer::canvas()`,
            // which does). Slice to `content`'s length rather than
            // `copy_from_slice`, which panics on any length mismatch.
            canvas[..content.len()].copy_from_slice(&content);
        } else {
            for px in canvas.chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 0]);
            }
            let cx0 = w as f64 / 2.0;
            let cy0 = h as f64 / 2.0;
            for dy in 0..h {
                for dx in 0..w {
                    let sx = cx0 + (dx as f64 - cx0) / scale;
                    let sy = cy0 + (dy as f64 - cy0) / scale;
                    if sx < 0.0 || sy < 0.0 || sx >= w as f64 || sy >= h as f64 {
                        continue;
                    }
                    let si = (((sy as i32) * w + sx as i32) * 4) as usize;
                    let di = ((dy * w + dx) * 4) as usize;
                    let a = (content[si + 3] as f64 * alpha).round().clamp(0.0, 255.0) as u8;
                    if a == 0 {
                        continue;
                    }
                    canvas[di] = (content[si] as f64 * alpha).round() as u8;
                    canvas[di + 1] = (content[si + 1] as f64 * alpha).round() as u8;
                    canvas[di + 2] = (content[si + 2] as f64 * alpha).round() as u8;
                    canvas[di + 3] = a;
                }
            }
        }

        self.layer.wl_surface().attach(Some(buffer.wl_buffer()), 0, 0);
        self.layer.wl_surface().damage_buffer(0, 0, w, h);
        self.layer.wl_surface().frame(qh, self.layer.wl_surface().clone());
        self.frame_pending = true;
        self.layer.commit();
    }
}

/// Draws `text` (folded to uppercase glyphs; unsupported characters are
/// skipped but still ignored gracefully) left-to-right starting at
/// (x0, y0), each font pixel drawn as a `scale`x`scale` block. Returns the
/// x position right after the last character, for placing a cursor.
fn draw_text(dst: &mut [u8], dst_w: i32, dst_h: i32, text: &str, x0: i32, y0: i32, scale: i32, color_bgr: [u8; 3]) -> i32 {
    let advance = (font::GLYPH_WIDTH + 1) * scale;
    let mut x = x0;
    for ch in text.chars() {
        if let Some(rows) = font::glyph(ch) {
            for (row_i, row_bits) in rows.iter().enumerate() {
                for col in 0..font::GLYPH_WIDTH {
                    if row_bits & (1 << (font::GLYPH_WIDTH - 1 - col)) != 0 {
                        draw_rect(dst, dst_w, dst_h, x + col * scale, y0 + row_i as i32 * scale, scale, scale, color_bgr);
                    }
                }
            }
        }
        x += advance;
    }
    x
}

fn draw_rect(dst: &mut [u8], dst_w: i32, dst_h: i32, x0: i32, y0: i32, w: i32, h: i32, color_bgr: [u8; 3]) {
    for y in y0..y0 + h {
        if y < 0 || y >= dst_h {
            continue;
        }
        for x in x0..x0 + w {
            if x < 0 || x >= dst_w {
                continue;
            }
            let di = ((y * dst_w + x) * 4) as usize;
            dst[di] = color_bgr[0];
            dst[di + 1] = color_bgr[1];
            dst[di + 2] = color_bgr[2];
            dst[di + 3] = 255;
        }
    }
}

/// Alpha-blend `src` (icon_size square RGBA) onto `dst` (BGRA) at (x0, y0),
/// rounding the corners to `radius` px. `dim` (0.0-1.0) scales the icon's
/// effective alpha uniformly, used to visually recede non-selected
/// thumbnails instead of relying solely on a border to mark selection.
fn blit_rounded(dst: &mut [u8], dst_w: i32, dst_h: i32, src: &RgbaImage, x0: i32, y0: i32, radius: i32, dim: f64) {
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    for sy in 0..sh {
        let dy = y0 + sy;
        if dy < 0 || dy >= dst_h {
            continue;
        }
        for sx in 0..sw {
            let dx = x0 + sx;
            if dx < 0 || dx >= dst_w {
                continue;
            }
            let cov = rounded_rect_coverage(sx, sy, sw, sh, radius);
            if cov <= 0.0 {
                continue; // outside the rounded rect, leave background showing
            }
            let sp = src.get_pixel(sx as u32, sy as u32).0;
            let alpha = (sp[3] as f64 / 255.0) * dim * cov;
            let di = ((dy * dst_w + dx) * 4) as usize;
            blend_over(dst, di, [sp[0], sp[1], sp[2]], alpha);
        }
    }
}

/// Draws a rounded-rect stroke (the selection ring) of `width` px around a
/// `size`x`size` box at (x0, y0) -- an outline, alpha-blended on top of
/// whatever's already there, rather than overwriting icon pixels outright.
#[allow(clippy::too_many_arguments)]
fn draw_ring(dst: &mut [u8], dst_w: i32, dst_h: i32, x0: i32, y0: i32, size: i32, radius: i32, width: i32, rgba: [u8; 4]) {
    if width <= 0 {
        return;
    }
    let rgb = [rgba[0], rgba[1], rgba[2]];
    let base_alpha = rgba[3] as f64 / 255.0;
    let outer_r = radius + width;
    for ly in -width..size + width {
        let dy = y0 + ly;
        if dy < 0 || dy >= dst_h {
            continue;
        }
        for lx in -width..size + width {
            let dx = x0 + lx;
            if dx < 0 || dx >= dst_w {
                continue;
            }
            // The ring is the annulus between two rounded rects, so its
            // coverage is the outer shape's minus whatever the inner shape
            // already covers. Both edges antialias for free this way.
            let outer = rounded_rect_coverage(
                lx + width,
                ly + width,
                size + 2 * width,
                size + 2 * width,
                outer_r,
            );
            if outer <= 0.0 {
                continue;
            }
            let inner = rounded_rect_coverage(lx, ly, size, size, radius);
            let cov = outer * (1.0 - inner);
            if cov <= 0.0 {
                continue; // interior belongs to the thumbnail, not the ring
            }
            let di = ((dy * dst_w + dx) * 4) as usize;
            blend_over(dst, di, rgb, base_alpha * cov);
        }
    }
}

/// Wrap `index + delta` into `[0, len)`, looping past either end. `len` must
/// be > 0 -- callers already guard on `filtered` being non-empty.
fn wrap_index(index: usize, delta: i32, len: usize) -> usize {
    let len = len as i32;
    (((index as i32 + delta) % len + len) % len) as usize
}

/// How much of the pixel at (x, y) is covered by a `w`x`h` rounded rect whose
/// top-left corner is the origin -- 0.0 outside, 1.0 inside, fractional along
/// the edge. That fractional band is the antialiasing: a corner pixel that's
/// 40% inside the curve contributes 40% alpha instead of being a hard in/out
/// decision, which is what removes the staircase jaggies on rounded corners.
///
/// Implemented as the standard signed distance field for a rounded box:
/// `d` is the distance from the pixel center to the shape's edge (negative
/// inside), and coverage is that distance mapped across a one-pixel band.
/// One function handles corners, straight edges, inside and outside alike,
/// so callers never need to special-case the quadrants.
fn rounded_rect_coverage(x: i32, y: i32, w: i32, h: i32, radius: i32) -> f64 {
    // Cheap reject well outside the box, and the interior fast path -- only
    // pixels near an edge or corner need the distance computation, which
    // keeps this off the hot path for the bulk of a large fill.
    if x < -1 || y < -1 || x > w || y > h {
        return 0.0;
    }
    let inside_x = x >= 0 && x < w;
    let inside_y = y >= 0 && y < h;
    if radius <= 0 {
        return if inside_x && inside_y { 1.0 } else { 0.0 };
    }
    if (inside_y && x >= radius && x < w - radius) || (inside_x && y >= radius && y < h - radius) {
        return 1.0;
    }

    let (wf, hf) = (w as f64, h as f64);
    let r = (radius as f64).min(wf / 2.0).min(hf / 2.0).max(0.0);
    // Pixel center, relative to the box's center.
    let px = x as f64 + 0.5 - wf / 2.0;
    let py = y as f64 + 0.5 - hf / 2.0;
    let qx = px.abs() - (wf / 2.0 - r);
    let qy = py.abs() - (hf / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    let d = outside + inside - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// Composite a source color over one premultiplied BGRA pixel of the canvas.
/// `rgb` is the straight (non-premultiplied) source color, `alpha` its
/// coverage-scaled opacity in 0.0-1.0 -- this does the premultiplication, so
/// callers can keep thinking in plain colors.
///
/// This runs for every covered pixel of the panel, the search pill, every
/// thumbnail, and the ring -- once per frame while the panel is open, since
/// eased scroll/highlight motion means a full repaint every frame, not just
/// on the pixels that changed. That makes it the actual hot path, so it's
/// fixed-point integer math with an opaque fast path rather than the
/// straightforward per-channel `f64` version: converting `alpha` to a u32
/// once and reusing it avoids repeating float `round()`/`clamp()` four times
/// per pixel, and the `a >= 255` case (an opaque interior pixel, the common
/// case for most of the canvas) skips the blend entirely and just writes.
fn blend_over(dst: &mut [u8], di: usize, rgb: [u8; 3], alpha: f64) {
    let a = (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u32; // 0..=255
    if a == 0 {
        return;
    }
    if a >= 255 {
        dst[di] = rgb[2];
        dst[di + 1] = rgb[1];
        dst[di + 2] = rgb[0];
        dst[di + 3] = 255;
        return;
    }
    let inv = 255 - a;
    for (c_dst, c_src) in [(0usize, 2usize), (1, 1), (2, 0)] {
        let src = rgb[c_src] as u32 * a;
        let bg = dst[di + c_dst] as u32 * inv;
        dst[di + c_dst] = ((src + bg + 127) / 255) as u8;
    }
    let out_a = (a * 255 + dst[di + 3] as u32 * inv) / 255;
    dst[di + 3] = out_a.min(255) as u8;
}

// ---------------------------------------------------------------- handlers

impl CompositorHandler for Picker {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wayland_client::protocol::wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, time: u32) {
        self.frame_pending = false;
        self.advance(time);
        self.kick(qh);
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Picker {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Picker {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // The compositor is tearing us down -- no time for an animation,
        // just resolve as cancelled.
        self.result = Some(Selection::Cancelled);
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        self.configured = true;
        self.needs_redraw = true;
    }
}

impl SeatHandler for Picker {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Picker {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        if self.is_closing() {
            return; // ignore input during the closing animation
        }
        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } => {
                    self.pointer_pos = event.position;
                    let new_hover = self.cell_at(event.position.0, event.position.1);
                    if new_hover != self.hover {
                        self.hover = new_hover;
                        if let Some(pos) = new_hover {
                            self.kbd_index = pos; // keep keyboard nav in sync with mouse
                        }
                        self.needs_redraw = true;
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.hover = None;
                    self.needs_redraw = true;
                }
                PointerEventKind::Press { button, .. } if button == 0x110 /* BTN_LEFT */ => {
                    if let Some(pos) = self.cell_at(self.pointer_pos.0, self.pointer_pos.1) {
                        self.begin_close(Selection::Picked(self.filtered[pos]));
                    }
                }
                PointerEventKind::Axis { horizontal, vertical, source, time, .. } => {
                    // Touchpads report a continuous stream of small deltas;
                    // mouse wheels report discrete "clicks" -- scale those
                    // up to feel like an actual scroll step.
                    let raw = if horizontal.absolute != 0.0 { horizontal.absolute } else { vertical.absolute };
                    let delta = if source == Some(wl_pointer::AxisSource::Wheel) {
                        raw.signum() * self.theme.wheel_step as f64
                    } else {
                        raw
                    };

                    // Track velocity from consecutive event timestamps so
                    // scrolling can coast a little after the input stops,
                    // like a browser/trackpad does, instead of hard-stopping
                    // the instant events stop arriving.
                    let inst_velocity = match self.last_axis_time {
                        Some(last) => {
                            let dt_ms = time.wrapping_sub(last).max(1);
                            (delta / (dt_ms as f64 / 1000.0)).clamp(-MAX_VELOCITY, MAX_VELOCITY)
                        }
                        None => 0.0,
                    };
                    self.velocity = match self.last_axis_time {
                        // Big gap since the last event -- this is a fresh
                        // gesture, don't blend with stale momentum.
                        Some(last) if time.wrapping_sub(last) > 150 => inst_velocity,
                        _ => self.velocity * 0.3 + inst_velocity * 0.7,
                    };
                    self.last_axis_time = Some(time);

                    self.set_scroll_target(self.scroll.target + delta);
                }
                _ => {}
            }
        }
        self.kick(qh);
    }
}

impl KeyboardHandler for Picker {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
        self.held_key = None;
    }
    fn press_key(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if self.is_closing() {
            return;
        }
        match event.keysym {
            Keysym::Escape => {
                if !self.search.is_empty() {
                    self.search.clear();
                    self.apply_search();
                } else {
                    self.begin_close(Selection::Cancelled);
                }
            }
            Keysym::Return | Keysym::KP_Enter => {
                if let Some(&entry_idx) = self.filtered.get(self.kbd_index) {
                    self.begin_close(Selection::Picked(entry_idx));
                }
            }
            Keysym::BackSpace => {
                if self.search.pop().is_some() {
                    self.apply_search();
                }
            }
            Keysym::Left => {
                self.move_kbd_cursor(-1);
                self.held_key = Some(HeldKey { dir: -1, pressed_at: event.time, last_repeat: event.time });
            }
            Keysym::Right => {
                self.move_kbd_cursor(1);
                self.held_key = Some(HeldKey { dir: 1, pressed_at: event.time, last_repeat: event.time });
            }
            Keysym::Home | Keysym::KP_Home => self.set_kbd_cursor(0),
            Keysym::End | Keysym::KP_End => self.set_kbd_cursor(i32::MAX),
            _ => {
                if let Some(text) = event.utf8.as_deref() {
                    let mut typed = false;
                    for ch in text.chars() {
                        if font::is_searchable(ch) {
                            self.search.push(ch);
                            typed = true;
                        }
                    }
                    if typed {
                        self.apply_search();
                    }
                }
            }
        }
        self.needs_redraw = true;
        self.kick(qh);
    }
    fn release_key(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if matches!(event.keysym, Keysym::Left | Keysym::Right) {
            self.held_key = None;
        }
        self.kick(qh);
    }
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _layout: u32) {}
}

impl ShmHandler for Picker {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Picker {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Picker);
delegate_output!(Picker);
delegate_shm!(Picker);
delegate_seat!(Picker);
delegate_pointer!(Picker);
delegate_keyboard!(Picker);
delegate_layer!(Picker);
delegate_registry!(Picker);

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force ground truth: supersample the pixel against the exact shape.
    fn supersampled(x: i32, y: i32, w: i32, h: i32, radius: i32) -> f64 {
        const N: i32 = 16;
        let (wf, hf, r) = (w as f64, h as f64, radius as f64);
        let mut hits = 0;
        for sy in 0..N {
            for sx in 0..N {
                let px = x as f64 + (sx as f64 + 0.5) / N as f64;
                let py = y as f64 + (sy as f64 + 0.5) / N as f64;
                if px < 0.0 || py < 0.0 || px > wf || py > hf {
                    continue;
                }
                let cx = px.clamp(r, wf - r);
                let cy = py.clamp(r, hf - r);
                let (dx, dy) = (px - cx, py - cy);
                if dx * dx + dy * dy <= r * r {
                    hits += 1;
                }
            }
        }
        hits as f64 / (N * N) as f64
    }

    const SHAPES: &[(i32, i32, i32)] = &[(20, 20, 8), (64, 40, 12), (1179, 370, 20), (300, 44, 22)];

    #[test]
    fn corners_are_antialiased_not_stepped() {
        let fractional = (0..20)
            .flat_map(|y| (0..20).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let c = rounded_rect_coverage(x, y, 20, 20, 8);
                c > 0.001 && c < 0.999
            })
            .count();
        assert!(fractional > 20, "expected a soft edge, got {fractional} partial pixels");
    }

    #[test]
    fn coverage_tracks_the_true_shape() {
        for &(w, h, r) in SHAPES {
            for y in -1..h + 1 {
                for x in -1..w + 1 {
                    let got = rounded_rect_coverage(x, y, w, h, r);
                    let want = supersampled(x, y, w, h, r);
                    assert!(
                        (got - want).abs() < 0.06,
                        "{w}x{h} r{r} at ({x},{y}): {got} vs {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn interior_is_solid_and_exterior_is_empty() {
        for &(w, h, r) in SHAPES {
            for y in -1..h + 1 {
                for x in -1..w + 1 {
                    let c = rounded_rect_coverage(x, y, w, h, r);
                    if x >= r && x < w - r && y >= r && y < h - r {
                        assert_eq!(c, 1.0, "haze inside {w}x{h} r{r} at ({x},{y})");
                    }
                    if x < -1 || y < -1 || x > w || y > h {
                        assert_eq!(c, 0.0, "bleed outside {w}x{h} r{r} at ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn zero_radius_is_a_plain_rect() {
        assert_eq!(rounded_rect_coverage(0, 0, 10, 10, 0), 1.0);
        assert_eq!(rounded_rect_coverage(9, 9, 10, 10, 0), 1.0);
        assert_eq!(rounded_rect_coverage(-1, 0, 10, 10, 0), 0.0);
        assert_eq!(rounded_rect_coverage(10, 0, 10, 10, 0), 0.0);
    }

    #[test]
    fn opaque_blend_replaces_and_transparent_blend_preserves() {
        let mut px = [0u8; 4];
        blend_over(&mut px, 0, [255, 128, 64], 1.0);
        assert_eq!(px, [64, 128, 255, 255], "BGRA byte order, fully opaque");

        let mut px = [10u8, 20, 30, 255];
        blend_over(&mut px, 0, [255, 255, 255], 0.0);
        assert_eq!(px, [10, 20, 30, 255], "zero alpha must be a no-op");
    }

    #[test]
    fn half_coverage_is_premultiplied() {
        // Over a transparent canvas, a 50%-covered white pixel must come out
        // at half intensity *and* half alpha -- that pairing is what makes an
        // antialiased edge blend correctly instead of haloing.
        let mut px = [0u8; 4];
        blend_over(&mut px, 0, [255, 255, 255], 0.5);
        assert_eq!(px, [128, 128, 128, 128]);
    }

    #[test]
    fn wrap_index_loops_past_either_end() {
        assert_eq!(wrap_index(4, 1, 5), 0, "past the last entry loops to the first");
        assert_eq!(wrap_index(0, -1, 5), 4, "before the first entry loops to the last");
        assert_eq!(wrap_index(2, 1, 5), 3, "ordinary step is unaffected");
        assert_eq!(wrap_index(0, 0, 5), 0);
        assert_eq!(wrap_index(0, -7, 5), 3, "wraps correctly even past one full loop");
        assert_eq!(wrap_index(0, 0, 1), 0, "single-entry list never moves");
    }
}
