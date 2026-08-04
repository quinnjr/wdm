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
use std::rc::Rc;
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
        // A word-at-a-time fill, because this runs once per repaint and a
        // repaint is once per keystroke: the byte loop this replaces was 8.3
        // million four-byte stores per keypress at 3840x2160. `slice::fill` on
        // `u32` lowers to a memset; `chunks_exact_mut` did not.
        //
        // SAFETY: `align_to_mut` is only ever asked to reinterpret, and the
        // result is used only when it produced no unaligned edges — the global
        // allocator hands out memory aligned well past 4 bytes, so that is the
        // real path. Anything else falls back to the byte loop, which is
        // correct at any alignment; a partial word at either end is not a whole
        // pixel and must not be written as one.
        let (prefix, words, suffix) = unsafe { self.data.align_to_mut::<u32>() };
        if prefix.is_empty() && suffix.is_empty() {
            words.fill(color);
            return;
        }

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
        if x0 >= x1 {
            return;
        }

        // One memcpy per row from a row built once, rather than a four-byte
        // store per pixel: `rect` paints the panel and the drop-down on every
        // repaint, and a repaint is every keystroke. The clip above already
        // established the range is in bounds.
        let row: Vec<u8> = color.to_ne_bytes().repeat((x1 - x0) as usize);
        let stride = self.width as usize * 4;
        for y in y0..y1 {
            let start = y as usize * stride + x0 as usize * 4;
            self.data[start..start + row.len()].copy_from_slice(&row);
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

/// What the cache holds for one glyph: its metrics and its coverage map.
type CachedGlyph = (fontdue::Metrics, Rc<[u8]>);

thread_local! {
    /// Rasterised glyphs, keyed by character and size.
    ///
    /// Bounded in practice by the handful of sizes the UI uses and the
    /// characters actually typed, so it needs no eviction policy: a login
    /// screen's lifetime is one login. Coverage maps are held behind `Rc` so a
    /// draw can take a handle out of the cache — ending the cache borrow before
    /// the canvas borrow starts — without copying the bitmap.
    static GLYPHS: RefCell<HashMap<fontdue::layout::GlyphRasterConfig, CachedGlyph>> =
        RefCell::new(HashMap::new());
}

/// Rasterise a glyph, reusing the cached coverage map when there is one.
fn rasterize(font: &Font, key: fontdue::layout::GlyphRasterConfig) -> CachedGlyph {
    GLYPHS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (metrics, coverage) = cache.entry(key).or_insert_with(|| {
            let (metrics, coverage) = font.rasterize_config(key);
            (metrics, Rc::from(coverage))
        });
        (*metrics, Rc::clone(coverage))
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

/// A half-open box in canvas pixels: `x0..x1`, `y0..y1`.
#[derive(Clone, Copy)]
pub struct Clip {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// A string laid out once.
///
/// Layout is the expensive half of drawing text, and several callers both
/// measure a string and draw it. Shaping it once here means such a caller pays
/// a single layout pass instead of one per operation.
pub struct Shaped {
    /// `None` when no font could be loaded; every operation is then a no-op.
    layout: Option<Layout>,
}

impl Shaped {
    pub fn new(text: &str, size: f32) -> Self {
        let Some(font) = font() else {
            return Self { layout: None };
        };

        let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
        layout.reset(&LayoutSettings::default());
        layout.append(&[font], &TextStyle::new(text, size, 0));
        Self {
            layout: Some(layout),
        }
    }

    /// The advance width, for centring and for placing a cursor.
    ///
    /// Advance rather than ink extent: trailing whitespace occupies space even
    /// though it rasterises nothing, so a caret or a marker placed after
    /// "Session: X " must land a space's width past the X, not flush against it.
    ///
    /// The per-glyph `ceil` is not a rounding choice made here, it is fontdue's:
    /// `Layout::append` advances its pen by `ceil(advance_width)` per glyph. So
    /// this sum is exactly the pen position [`Self::draw`] lays glyphs out
    /// against, and rounding once at the end instead would introduce the drift
    /// it looks like it avoids.
    pub fn width(&self) -> f32 {
        let (Some(font), Some(layout)) = (font(), &self.layout) else {
            return 0.0;
        };

        layout
            .glyphs()
            .iter()
            // Control characters are laid out with zero metrics, not with
            // their glyph's; asking the font would count a notdef box.
            .filter(|g| !g.char_data.is_control())
            .map(|g| {
                font.metrics_indexed(g.key.glyph_index, g.key.px)
                    .advance_width
                    .ceil()
            })
            .sum()
    }

    /// Draw with the left edge at `x` and the top at `y`.
    pub fn draw(&self, canvas: &mut Canvas, x: f32, y: f32, color: u32) {
        self.draw_clipped(canvas, x, y, color, None);
    }

    /// Draw, dropping any ink outside `clip`.
    ///
    /// The canvas is the only bound [`Self::draw`] has, which is the wrong one
    /// for text inside a widget: an answer longer than the field drew straight
    /// out of it and across the screen. A caller that owns a box passes it.
    pub fn draw_clipped(
        &self,
        canvas: &mut Canvas,
        x: f32,
        y: f32,
        color: u32,
        clip: Option<Clip>,
    ) {
        let (Some(font), Some(layout)) = (font(), &self.layout) else {
            return;
        };

        for glyph in layout
            .glyphs()
            .iter()
            // The same filter [`Self::width`] applies, and for the same reason
            // it must: a control character is laid out with zero metrics, so
            // measuring it as nothing while drawing whatever the font returns
            // for it would put ink where the caret was never told about.
            .filter(|g| !g.char_data.is_control())
        {
            // The Rc handle is cloned out of the cache rather than blended
            // under its borrow, because blend takes &mut Canvas and the cache
            // borrow would still be live. Cloning the handle copies nothing.
            let (metrics, coverage) = rasterize(font, glyph.key);
            if metrics.width == 0 || metrics.height == 0 {
                continue;
            }

            let left = (x + glyph.x) as i32;
            let top = (y + glyph.y) as i32;
            // Whole glyphs that fall outside are skipped rather than blended
            // pixel by pixel and rejected: the case this exists for is a string
            // far longer than its box, where most glyphs are entirely out.
            if let Some(clip) = clip
                && (left >= clip.x1
                    || left + metrics.width as i32 <= clip.x0
                    || top >= clip.y1
                    || top + metrics.height as i32 <= clip.y0)
            {
                continue;
            }

            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let px = left + col as i32;
                    let py = top + row as i32;
                    if let Some(clip) = clip
                        && (px < clip.x0 || px >= clip.x1 || py < clip.y0 || py >= clip.y1)
                    {
                        continue;
                    }
                    canvas.blend(px, py, color, coverage[row * metrics.width + col]);
                }
            }
        }
    }
}

/// Draw `text` with its left edge at `x` and its top at `y`.
pub fn draw(canvas: &mut Canvas, x: f32, y: f32, size: f32, color: u32, text: &str) {
    Shaped::new(text, size).draw(canvas, x, y, color);
}

/// Draw `text` horizontally centred within the canvas, top at `y`.
pub fn draw_centered(canvas: &mut Canvas, y: f32, size: f32, color: u32, text: &str) {
    let shaped = Shaped::new(text, size);
    let x = (canvas.width as f32 - shaped.width()) / 2.0;
    shaped.draw(canvas, x.max(0.0), y, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measure without drawing. Production callers do both, so they hold a
    /// [`Shaped`]; this is the same call they make, spelled shorter.
    fn width(text: &str, size: f32) -> f32 {
        Shaped::new(text, size).width()
    }

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
    fn trailing_whitespace_has_width() {
        if !have_font() {
            // The only guard on measuring advance rather than ink, so a host
            // without fonts must say it proved nothing rather than pass quietly.
            eprintln!("skipped: no font available");
            return;
        }
        // Advance, not ink: a marker placed after "label " must clear the
        // trailing space, and a caret after one must not sit on the text.
        assert!(width("a ", 20.0) > width("a", 20.0));

        // And each space is worth the same, whichever font this host has:
        // ink extent would stop growing after the first, while an advance sum
        // grows by one space every time. The oracle is the *shape* of the
        // series, so it does not depend on any font's metrics.
        let one = width("a ", 20.0) - width("a", 20.0);
        let two = width("a  ", 20.0) - width("a ", 20.0);
        assert!(
            (one - two).abs() < 0.001,
            "each space must advance the pen equally: {one} then {two}"
        );
    }

    #[test]
    fn control_characters_measure_and_draw_as_nothing() {
        if !have_font() {
            eprintln!("skipped: no font available");
            return;
        }

        // `width` has always skipped control characters — they are laid out
        // with zero metrics, so counting the font's notdef box for one would
        // put the caret past ink that is not there. `draw` did not skip them,
        // so if the font returns a glyph for U+0009 the ink and the measurement
        // disagree: the caret sits inside the drawn text, and centring is off
        // by whatever was drawn. Both sides must agree, which means both must
        // ignore it.
        assert_eq!(width("a\tb", 20.0), width("ab", 20.0));

        let render = |text: &str| {
            let mut canvas = Canvas::new(120, 40);
            draw(&mut canvas, 4.0, 4.0, 20.0, 0xffffffff, text);
            canvas.data
        };
        assert_eq!(
            render("a\tb"),
            render("ab"),
            "a control character put ink on the canvas"
        );
    }

    #[test]
    fn clipping_keeps_ink_inside_the_box() {
        if !have_font() {
            eprintln!("skipped: no font available");
            return;
        }

        // The guard behind the answer field: a string far wider than its widget
        // must not write outside it, whatever the canvas allows.
        let clip = Clip {
            x0: 10,
            y0: 5,
            x1: 40,
            y1: 30,
        };
        let mut canvas = Canvas::new(200, 60);
        Shaped::new(&"W".repeat(200), 20.0).draw_clipped(
            &mut canvas,
            4.0,
            8.0,
            0xffffffff,
            Some(clip),
        );

        for y in 0..canvas.height {
            for x in 0..canvas.width {
                let o = ((y * canvas.width + x) * 4) as usize;
                let inside = x >= clip.x0 && x < clip.x1 && y >= clip.y0 && y < clip.y1;
                if !inside {
                    assert_eq!(canvas.data[o + 3], 0, "ink escaped the clip at {x},{y}");
                }
            }
        }
        assert!(
            canvas.data.chunks_exact(4).any(|p| p[3] == 0xff),
            "nothing was drawn, so the clip proves nothing"
        );
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
