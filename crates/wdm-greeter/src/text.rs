//! Software text rasterisation.
//!
//! The greeter draws into a `wl_shm` buffer with no toolkit, so it rasterises
//! glyphs itself. That saves depending on GTK or Qt to draw two text fields.
//!
//! Keystrokes *are* the hot path on a login screen — every one repaints the
//! form — so rasterised glyphs are cached. Without the cache every character
//! typed re-rasterises every glyph on screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle};
use fontdue::{Font, FontSettings};

/// Fonts to try, in order. A minimal system may have only one of these.
const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
];

/// An ARGB8888 canvas in native byte order, which is what `wl_shm`'s
/// `Argb8888` format expects on a little-endian machine.
pub struct Canvas {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

impl Canvas {
    pub fn new(width: i32, height: i32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self {
            width,
            height,
            data: vec![0; (width * height * 4) as usize],
        }
    }

    pub fn fill(&mut self, color: u32) {
        for pixel in self.data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&color.to_ne_bytes());
        }
    }

    /// Fill an axis-aligned rectangle, clipped to the canvas.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);

        for row in y0..y1 {
            for col in x0..x1 {
                let offset = ((row * self.width + col) * 4) as usize;
                self.data[offset..offset + 4].copy_from_slice(&color.to_ne_bytes());
            }
        }
    }

    /// Blend one pixel of `color` at `coverage` over what is already there.
    fn blend(&mut self, x: i32, y: i32, color: u32, coverage: u8) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height || coverage == 0 {
            return;
        }

        let offset = ((y * self.width + x) * 4) as usize;
        let src = color.to_ne_bytes();
        let a = u32::from(coverage);

        for (dst, &src) in self.data[offset..offset + 3].iter_mut().zip(&src[..3]) {
            *dst = ((u32::from(src) * a + u32::from(*dst) * (255 - a)) / 255) as u8;
        }
        // Opaque wherever anything was drawn: a translucent login form over an
        // undefined framebuffer is unreadable.
        self.data[offset + 3] = 0xff;
    }
}

thread_local! {
    /// Rasterised glyphs, keyed by character and size.
    ///
    /// Bounded in practice by the handful of sizes the UI uses and the
    /// characters actually typed, so it needs no eviction policy: a login
    /// screen's lifetime is one login.
    static GLYPHS: RefCell<HashMap<fontdue::layout::GlyphRasterConfig, (fontdue::Metrics, Vec<u8>)>> =
        RefCell::new(HashMap::new());
}

/// Rasterise a glyph, reusing the cached coverage map when there is one.
fn rasterize<T>(
    font: &Font,
    key: fontdue::layout::GlyphRasterConfig,
    f: impl FnOnce(&fontdue::Metrics, &[u8]) -> T,
) -> T {
    GLYPHS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (metrics, coverage) = cache
            .entry(key)
            .or_insert_with(|| font.rasterize_config(key));
        f(metrics, coverage)
    })
}

fn font() -> Option<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();

    FONT.get_or_init(|| {
        for path in CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                return Some(font);
            }
        }
        None
    })
    .as_ref()
}

/// Whether any font was found.
///
/// A greeter with no font can still draw its boxes, but nobody could read it, so
/// the caller warns loudly rather than presenting a mystery.
pub fn have_font() -> bool {
    font().is_some()
}

/// Width of `text` at `size`, for centring and for placing a cursor.
pub fn width(text: &str, size: f32) -> f32 {
    let Some(font) = font() else {
        return 0.0;
    };

    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings::default());
    layout.append(&[font], &TextStyle::new(text, size, 0));

    layout
        .glyphs()
        .iter()
        .map(|g| g.x + g.width as f32)
        .fold(0.0, f32::max)
}

/// Draw `text` with its left edge at `x` and its top at `y`.
pub fn draw(canvas: &mut Canvas, x: f32, y: f32, size: f32, color: u32, text: &str) {
    let Some(font) = font() else {
        return;
    };

    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings::default());
    layout.append(&[font], &TextStyle::new(text, size, 0));

    for glyph in layout.glyphs() {
        // Copied out of the cache rather than blended under its borrow, because
        // blend takes &mut Canvas and the cache borrow would still be live.
        let Some((metrics, coverage)) = rasterize(font, glyph.key, |metrics, coverage| {
            (metrics.width > 0 && metrics.height > 0).then(|| (*metrics, coverage.to_vec()))
        }) else {
            continue;
        };

        for row in 0..metrics.height {
            for col in 0..metrics.width {
                canvas.blend(
                    (x + glyph.x) as i32 + col as i32,
                    (y + glyph.y) as i32 + row as i32,
                    color,
                    coverage[row * metrics.width + col],
                );
            }
        }
    }
}

/// Draw `text` horizontally centred within the canvas, top at `y`.
pub fn draw_centered(canvas: &mut Canvas, y: f32, size: f32, color: u32, text: &str) {
    let x = (canvas.width as f32 - width(text, size)) / 2.0;
    draw(canvas, x.max(0.0), y, size, color, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_is_the_requested_size() {
        let canvas = Canvas::new(64, 32);
        assert_eq!(canvas.data.len(), 64 * 32 * 4);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(0, 0), (-1, 5)] {
            let canvas = Canvas::new(w, h);
            assert!(canvas.width >= 1 && canvas.height >= 1);
        }
    }

    #[test]
    fn fill_sets_every_pixel() {
        let mut canvas = Canvas::new(4, 4);
        canvas.fill(0xff102030);
        for pixel in canvas.data.chunks_exact(4) {
            assert_eq!(u32::from_ne_bytes(pixel.try_into().unwrap()), 0xff102030);
        }
    }

    #[test]
    fn rect_is_clipped_to_the_canvas() {
        let mut canvas = Canvas::new(4, 4);
        // Entirely outside, straddling, and negative origin: none may panic.
        canvas.rect(10, 10, 5, 5, 0xffffffff);
        canvas.rect(2, 2, 100, 100, 0xffffffff);
        canvas.rect(-5, -5, 7, 7, 0xffffffff);
        assert_eq!(canvas.data.len(), 4 * 4 * 4);
    }

    #[test]
    fn rect_fills_the_right_pixels() {
        let mut canvas = Canvas::new(4, 4);
        canvas.rect(1, 1, 2, 2, 0xffabcdef);
        let at = |x: i32, y: i32| {
            let o = ((y * 4 + x) * 4) as usize;
            u32::from_ne_bytes(canvas.data[o..o + 4].try_into().unwrap())
        };
        assert_eq!(at(1, 1), 0xffabcdef);
        assert_eq!(at(2, 2), 0xffabcdef);
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(3, 3), 0);
    }

    #[test]
    fn drawing_outside_the_canvas_does_not_panic() {
        let mut canvas = Canvas::new(8, 8);
        draw(&mut canvas, -100.0, -100.0, 20.0, 0xffffffff, "offscreen");
        draw(&mut canvas, 1000.0, 1000.0, 20.0, 0xffffffff, "offscreen");
    }

    #[test]
    fn text_marks_pixels_opaque() {
        if !have_font() {
            return;
        }
        let mut canvas = Canvas::new(200, 60);
        draw(&mut canvas, 4.0, 4.0, 24.0, 0xffffffff, "wdm");
        // Something must have been drawn, and it must be opaque or it would be
        // invisible over an undefined background.
        assert!(canvas.data.chunks_exact(4).any(|p| p[3] == 0xff));
    }

    #[test]
    fn width_grows_with_text() {
        if !have_font() {
            return;
        }
        assert!(width("mm", 20.0) > width("m", 20.0));
        assert_eq!(width("", 20.0), 0.0);
    }

    #[test]
    fn centering_stays_on_canvas() {
        if !have_font() {
            return;
        }
        // A string wider than the canvas must not be placed at a negative x.
        let mut canvas = Canvas::new(20, 40);
        draw_centered(&mut canvas, 0.0, 20.0, 0xffffffff, "far too wide to fit");
    }
}
