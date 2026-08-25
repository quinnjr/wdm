//! Layout and painting of the login form.
//!
//! Deliberately plain. This greeter exists to be the shipped default and to prove
//! `wdm_greeter_v1` is implementable by something that is not wdm; anyone wanting
//! a themed login screen writes their own client against the same protocol.

use std::path::Path;

use crate::config::{Background, ColorScheme, Config};
use crate::text::{self, Canvas};

/// Every colour the form is drawn in. Two fixed sets, selected by
/// `color-scheme` in `/etc/wdm/greeter.toml`; anyone wanting more than a
/// light and a dark look writes their own greeter, which is this crate's
/// whole thesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: u32,
    pub panel: u32,
    pub panel_edge: u32,
    pub field: u32,
    pub text: u32,
    pub dim: u32,
    pub accent: u32,
    pub error: u32,
    pub selected: u32,
}

impl Palette {
    /// The colours this greeter has always painted.
    pub const fn dark() -> Self {
        Palette {
            background: 0xff12131a,
            panel: 0xff1c1e28,
            panel_edge: 0xff2c2f3d,
            field: 0xff0d0e13,
            text: 0xffe8e8ef,
            dim: 0xff8b8fa3,
            accent: 0xff6f9dff,
            error: 0xffff7b72,
            selected: 0xff2a3350,
        }
    }

    /// The dark palette's roles, re-cast in light greys. The error red and
    /// accent blue are darkened rather than reused, because the dark set's
    /// values were chosen against a near-black ground and wash out on white.
    pub const fn light() -> Self {
        Palette {
            background: 0xffe9eaf0,
            panel: 0xfff7f7fa,
            panel_edge: 0xffc9ccd8,
            field: 0xffffffff,
            text: 0xff191b24,
            dim: 0xff5c6072,
            accent: 0xff2757b8,
            error: 0xffb3261e,
            selected: 0xffd4ddf5,
        }
    }
}

/// What the screen is filled with before the panel is drawn on top.
#[derive(Debug)]
pub enum Backdrop {
    /// The palette's own background colour — today's behaviour.
    Palette,
    Color(u32),
    Image(Image),
}

/// A decoded background image, 0xAARRGGBB per pixel like [`Canvas`].
#[derive(Debug)]
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u32>,
    /// The cover-scaled frame for the last canvas size drawn. `draw_cover`
    /// runs on every repaint and a repaint is every keystroke, so without
    /// this the scaling loop — 8.3M samples at 4K — sat on the one path a
    /// login screen actually exercises; with it, a repaint is a memcpy and
    /// the loop runs only on resize. RefCell because the painter takes the
    /// style immutably, and this process is single-threaded.
    cover: std::cell::RefCell<Option<(i32, i32, Vec<u8>)>>,
}

/// Everything `/etc/wdm/greeter.toml` decides about how the form looks.
#[derive(Debug)]
pub struct Style {
    pub palette: Palette,
    pub backdrop: Backdrop,
}

impl Style {
    /// Build the style, decoding the background image if there is one.
    ///
    /// Decoding happens here, once at startup, because a file that cannot be
    /// decoded is a configuration error and configuration errors are startup
    /// errors — not something to discover on the first frame.
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let palette = match config.color_scheme {
            ColorScheme::Dark => Palette::dark(),
            ColorScheme::Light => Palette::light(),
        };
        let backdrop = match &config.background {
            None => Backdrop::Palette,
            Some(Background::Color(color)) => Backdrop::Color(*color),
            Some(Background::Image(path)) => Backdrop::Image(Image::load_png(path)?),
        };
        Ok(Style { palette, backdrop })
    }
}

impl Default for Style {
    fn default() -> Self {
        Style {
            palette: Palette::dark(),
            backdrop: Backdrop::Palette,
        }
    }
}

impl Image {
    /// Decode a PNG. PNG only: the point of `background` accepting an image
    /// is a wallpaper, not an image pipeline, and every wallpaper tool can
    /// write one.
    pub fn load_png(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
        // EXPAND turns indexed PNGs — what pngquant, optipng and GIMP's
        // indexed export produce — into RGB, and sub-byte greys into whole
        // bytes; STRIP_16 folds 16-bit channels to 8. Without these the
        // decoder hands back palette indices and the match below refuses a
        // perfectly valid wallpaper, blaming the administrator's file.
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder
            .read_info()
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let buf = &buf[..info.buffer_size()];

        // Normalise every colour type to opaque ARGB. A translucent wallpaper
        // would put the previous session's framebuffer on the login screen,
        // so alpha is composited against black here, once, rather than
        // trusted at draw time.
        let (width, height) = (info.width as i32, info.height as i32);
        if width <= 0 || height <= 0 {
            return Err(format!("{}: image is empty", path.display()));
        }
        let pixels: Vec<u32> = match info.color_type {
            png::ColorType::Rgb => buf
                .chunks_exact(3)
                .map(|p| {
                    0xff00_0000 | u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2])
                })
                .collect(),
            png::ColorType::Rgba => buf
                .chunks_exact(4)
                .map(|p| {
                    let a = u32::from(p[3]);
                    let ch = |v: u8| u32::from(v) * a / 255;
                    0xff00_0000 | ch(p[0]) << 16 | ch(p[1]) << 8 | ch(p[2])
                })
                .collect(),
            png::ColorType::Grayscale => buf
                .iter()
                .map(|&g| {
                    let g = u32::from(g);
                    0xff00_0000 | g << 16 | g << 8 | g
                })
                .collect(),
            png::ColorType::GrayscaleAlpha => buf
                .chunks_exact(2)
                .map(|p| {
                    let g = u32::from(p[0]) * u32::from(p[1]) / 255;
                    0xff00_0000 | g << 16 | g << 8 | g
                })
                .collect(),
            other => {
                // Unreachable for a well-formed file: EXPAND above rewrites
                // Indexed to Rgb/Rgba. Kept as an error rather than a panic
                // because the file is the administrator's, not ours.
                return Err(format!(
                    "{}: unsupported colour type {other:?}",
                    path.display()
                ));
            }
        };
        if pixels.len() != (width * height) as usize {
            return Err(format!("{}: truncated image data", path.display()));
        }
        Ok(Image::new(width, height, pixels))
    }

    pub fn new(width: i32, height: i32, pixels: Vec<u32>) -> Self {
        Image {
            width,
            height,
            pixels,
            cover: std::cell::RefCell::new(None),
        }
    }

    /// Fill the canvas with the image, scaled to cover and centre-cropped —
    /// what every wallpaper setter calls "fill". Nearest-neighbour, because
    /// this runs on resize only and a login screen's wallpaper does not
    /// justify a resampling kernel.
    pub fn draw_cover(&self, canvas: &mut Canvas) {
        let (cw, ch) = (canvas.width, canvas.height);
        if cw <= 0 || ch <= 0 {
            return;
        }
        let mut cover = self.cover.borrow_mut();
        if !matches!(&*cover, Some((w, h, _)) if *w == cw && *h == ch) {
            *cover = Some((cw, ch, self.render_cover(cw, ch)));
        }
        let Some((_, _, bytes)) = &*cover else {
            unreachable!("filled above");
        };
        canvas.data.copy_from_slice(bytes);
    }

    /// The scaling pass behind [`Image::draw_cover`]'s cache: runs on resize,
    /// never on an ordinary repaint.
    fn render_cover(&self, cw: i32, ch: i32) -> Vec<u8> {
        let scale = f64::max(
            f64::from(cw) / f64::from(self.width),
            f64::from(ch) / f64::from(self.height),
        );
        // The scaled image overhangs the canvas on one axis; centring the
        // crop keeps the subject of the picture on screen.
        let ox = (f64::from(self.width) * scale - f64::from(cw)) / 2.0;
        let oy = (f64::from(self.height) * scale - f64::from(ch)) / 2.0;

        // One column table instead of a division per pixel: every row samples
        // the same source columns.
        let columns: Vec<i32> = (0..cw)
            .map(|x| (((f64::from(x) + 0.5 + ox) / scale) as i32).clamp(0, self.width - 1))
            .collect();

        let mut bytes = vec![0u8; (cw * ch * 4) as usize];
        for y in 0..ch {
            let sy = (((f64::from(y) + 0.5 + oy) / scale) as i32).clamp(0, self.height - 1);
            let row = (sy * self.width) as usize;
            for (x, &sx) in columns.iter().enumerate() {
                let pixel = self.pixels[row + sx as usize];
                let offset = (y as usize * cw as usize + x) * 4;
                // Same byte order `Canvas::fill` writes: the u32's native
                // bytes, so the compositor reads it as the ARGB it expects.
                bytes[offset..offset + 4].copy_from_slice(&pixel.to_ne_bytes());
            }
        }
        bytes
    }
}

/// Fill the screen with whatever sits behind the panel.
fn draw_backdrop(canvas: &mut Canvas, style: &Style) {
    match &style.backdrop {
        Backdrop::Palette => canvas.fill(style.palette.background),
        Backdrop::Color(color) => canvas.fill(*color),
        Backdrop::Image(image) => image.draw_cover(canvas),
    }
}

const PANEL_WIDTH: i32 = 460;
const PANEL_HEIGHT: i32 = 300;
const PADDING: i32 = 32;

/// Rows the drop-down shows at once. Beyond this it scrolls, so a machine with
/// a dozen desktops installed does not get a list taller than the screen.
pub const MENU_ROWS: usize = 6;
const MENU_ROW_HEIGHT: i32 = 28;

/// Characters of an echoed answer kept on screen, counted from the end.
///
/// Comfortably more than the field is wide, so the clip and not this decides
/// what the user sees; this is only what stops the layout pass growing with an
/// answer nothing bounds.
const VISIBLE_TAIL: usize = 64;

const TITLE_SIZE: f32 = 26.0;
const BODY_SIZE: f32 = 17.0;
const SMALL_SIZE: f32 = 14.0;

/// What to draw. Owned by the client's state and handed here each frame.
pub struct View<'a> {
    /// The user currently selected.
    pub username: &'a str,
    /// Their display name, if the enumerate phase supplied one.
    pub display_name: &'a str,
    /// Every session, in the order the compositor advertised them.
    pub sessions: &'a [String],
    /// Which of them will be launched.
    pub session_index: usize,
    /// Whether the session drop-down is open.
    pub menu_open: bool,
    /// Text of the prompt PAM is waiting on, if any.
    pub prompt: Option<&'a str>,
    /// What the user has typed for the current prompt.
    pub answer: &'a str,
    /// Whether the answer must be masked.
    pub secret: bool,
    /// An error to show, from `auth_failed` or `last_error`.
    pub error: Option<&'a str>,
    /// Informational text from PAM.
    pub info: Option<&'a str>,
    /// True once `auth_ok` arrived and the session is starting.
    pub launching: bool,
    /// Whether more than one user or session is selectable.
    pub multiple_users: bool,
    pub multiple_sessions: bool,
}

/// Paint the whole screen.
pub fn paint(canvas: &mut Canvas, view: &View<'_>, style: &Style) {
    draw_backdrop(canvas, style);
    let palette = &style.palette;

    let panel_x = (canvas.width - PANEL_WIDTH) / 2;
    let panel_y = (canvas.height - PANEL_HEIGHT) / 2;

    // A one pixel border rather than a real outline: enough to separate the panel
    // from the background without a compositing pass.
    canvas.rect(
        panel_x - 1,
        panel_y - 1,
        PANEL_WIDTH + 2,
        PANEL_HEIGHT + 2,
        palette.panel_edge,
    );
    canvas.rect(panel_x, panel_y, PANEL_WIDTH, PANEL_HEIGHT, palette.panel);

    let left = (panel_x + PADDING) as f32;
    let mut y = (panel_y + PADDING) as f32;

    let title = if view.display_name.is_empty() {
        view.username.to_owned()
    } else {
        format!("{} ({})", view.display_name, view.username)
    };
    text::draw(canvas, left, y, TITLE_SIZE, palette.text, &title);
    y += TITLE_SIZE * 2.0;

    if view.launching {
        text::draw(
            canvas,
            left,
            y,
            BODY_SIZE,
            palette.accent,
            "Starting session…",
        );
        return;
    }

    // Prompt label, then the field. PAM decides the wording, so it is shown
    // verbatim rather than replaced with "Password:".
    let label = view.prompt.unwrap_or("Waiting…");
    text::draw(canvas, left, y, SMALL_SIZE, palette.dim, label);
    y += SMALL_SIZE * 1.8;

    let field_height = (BODY_SIZE * 1.9) as i32;
    let field_x = panel_x + PADDING;
    let field_width = PANEL_WIDTH - PADDING * 2;
    canvas.rect(field_x, y as i32, field_width, field_height, palette.field);

    let shown = if view.secret {
        // Fixed-width mask: revealing the length of a password is a small leak,
        // but showing nothing at all leaves the user unsure the keyboard works.
        "•".repeat(view.answer.chars().count().min(32))
    } else {
        // An echoed answer is windowed to its tail rather than shown whole, for
        // the same reason the mask is capped. Nothing bounds what PAM's echo-on
        // questions get typed into them, and the whole string was laid out and
        // blended on every keystroke — O(n) per key, O(n²) over the answer — on
        // the one path a login screen actually exercises. The tail is the end
        // the caret is at, so typing still echoes what was just typed.
        let count = view.answer.chars().count();
        view.answer
            .chars()
            .skip(count.saturating_sub(VISIBLE_TAIL))
            .collect()
    };

    let text_y = y + (field_height as f32 - BODY_SIZE) / 2.0 - 2.0;
    let shown = text::Shaped::new(&shown, BODY_SIZE);
    // Clipped to the field: the window above bounds the work, but a wide glyph
    // run still overhangs, and `Shaped::draw` on its own is bounded only by the
    // canvas — so the overhang landed on the rest of the form.
    let field = text::Clip {
        x0: field_x,
        y0: y as i32,
        x1: field_x + field_width,
        y1: y as i32 + field_height,
    };
    shown.draw_clipped(canvas, left + 8.0, text_y, palette.text, Some(field));

    // Caret, so an empty field still looks focused. Pinned inside the field for
    // the same reason: `Canvas::rect` clips to the canvas, not to the widget.
    let caret_x = (left + 8.0 + shown.width() + 1.0) as i32;
    let caret_x = caret_x.min(field.x1 - 2);
    canvas.rect(caret_x, text_y as i32, 2, BODY_SIZE as i32, palette.accent);

    y += field_height as f32 + BODY_SIZE * 1.4;

    if let Some(error) = view.error {
        text::draw(canvas, left, y, SMALL_SIZE, palette.error, error);
        y += SMALL_SIZE * 1.6;
    }
    if let Some(info) = view.info {
        text::draw(canvas, left, y, SMALL_SIZE, palette.dim, info);
    }

    // Footer: the session about to start, and the keys that change things. A
    // greeter that does not say how to switch session is one the user cannot.
    let footer_y = (panel_y + PANEL_HEIGHT - PADDING) as f32 - SMALL_SIZE;
    let current = view
        .sessions
        .get(view.session_index)
        .map(String::as_str)
        .unwrap_or("none");

    // The marker is what tells the user this is a control and not a label. Drawn
    // geometrically rather than as U+25BE, because the fonts wdm falls back to
    // are not guaranteed to have that glyph and a tofu box is worse than none.
    let session_label = text::Shaped::new(&format!("Session: {current} "), SMALL_SIZE);
    session_label.draw(canvas, left, footer_y, palette.dim);
    triangle(
        canvas,
        (left + session_label.width()) as i32,
        footer_y as i32 + (SMALL_SIZE / 2.0) as i32,
        7,
        palette.dim,
    );

    let mut hints = Vec::new();
    if view.multiple_sessions {
        hints.push("F2 session");
    }
    if view.multiple_users {
        hints.push("F1 user");
    }
    hints.push("Esc clear");
    let hint = text::Shaped::new(&hints.join("   "), SMALL_SIZE);
    let hint_x = (panel_x + PANEL_WIDTH - PADDING) as f32 - hint.width();
    hint.draw(canvas, hint_x, footer_y, palette.dim);

    // Drawn last so it sits over everything, and outside the panel bounds so a
    // long list is not clipped by the panel.
    if view.menu_open {
        draw_menu(canvas, panel_x, footer_y, view, palette);
    }
}

/// Draw a small downward-pointing triangle, `width` pixels across.
///
/// Rows of shrinking rectangles: enough for a 7px marker, and it costs no font.
fn triangle(canvas: &mut Canvas, x: i32, y: i32, width: i32, color: u32) {
    let rows = (width + 1) / 2;
    for row in 0..rows {
        let inset = row;
        let w = width - inset * 2;
        if w <= 0 {
            break;
        }
        canvas.rect(x + inset, y + row, w, 1, color);
    }
}

/// Where the drop-down is placed, given the space around its control.
///
/// Prefers opening downward like any other drop-down, and only flips above the
/// control when the list would run off the bottom of the screen.
pub fn menu_origin(anchor_y: i32, menu_height: i32, canvas_height: i32, row_height: i32) -> i32 {
    let below = anchor_y + row_height;
    if below + menu_height <= canvas_height {
        return below;
    }

    let above = anchor_y - menu_height - 6;
    if above >= 0 {
        return above;
    }

    // Neither fits: pin it to the top so the first rows are readable rather than
    // letting it hang off the bottom.
    0
}

/// Which slice of the session list to show.
///
/// A list that fits is shown whole and never moves. A longer one keeps the
/// selection centred, clamped at either end, so there is always as much
/// context visible around the selected row as the list allows.
pub fn menu_window(len: usize, selected: usize, rows: usize) -> std::ops::Range<usize> {
    if len <= rows {
        return 0..len;
    }
    // Centre the selection, then clamp so the window never runs past either end.
    let half = rows / 2;
    let start = selected.saturating_sub(half).min(len - rows);
    start..start + rows
}

fn draw_menu(canvas: &mut Canvas, panel_x: i32, anchor_y: f32, view: &View<'_>, palette: &Palette) {
    let window = menu_window(view.sessions.len(), view.session_index, MENU_ROWS);
    let shown = window.len() as i32;
    if shown == 0 {
        return;
    }

    // A scrolled list reserves a strip at the bottom for the position counter,
    // so it does not sit on top of the last row.
    let scrolls = view.sessions.len() > MENU_ROWS;
    let counter_strip = if scrolls { SMALL_SIZE as i32 + 6 } else { 0 };

    let width = PANEL_WIDTH - PADDING * 2;
    let height = shown * MENU_ROW_HEIGHT + 8 + counter_strip;
    let x = panel_x + PADDING;

    let y = menu_origin(anchor_y as i32, height, canvas.height, MENU_ROW_HEIGHT);

    canvas.rect(x - 1, y - 1, width + 2, height + 2, palette.panel_edge);
    canvas.rect(x, y, width, height, palette.field);

    for (row, index) in window.enumerate() {
        let row_y = y + 4 + row as i32 * MENU_ROW_HEIGHT;

        if index == view.session_index {
            canvas.rect(x + 2, row_y, width - 4, MENU_ROW_HEIGHT, palette.selected);
        }

        let color = if index == view.session_index {
            palette.text
        } else {
            palette.dim
        };
        let baseline = row_y as f32 + (MENU_ROW_HEIGHT as f32 - BODY_SIZE) / 2.0 - 2.0;
        text::draw(
            canvas,
            (x + 12) as f32,
            baseline,
            BODY_SIZE,
            color,
            &view.sessions[index],
        );
    }

    // A hint that there is more above or below, so a scrolled list does not look
    // like the whole list.
    if scrolls {
        let more = text::Shaped::new(
            &format!("{}/{}", view.session_index + 1, view.sessions.len()),
            SMALL_SIZE,
        );
        let more_x = (x + width) as f32 - more.width() - 8.0;
        more.draw(
            canvas,
            more_x,
            (y + height) as f32 - SMALL_SIZE - 2.0,
            palette.dim,
        );
    }
}

/// Draw a message with no login form, used before the enumerate phase completes
/// and when there is nothing to log in as.
pub fn paint_message(canvas: &mut Canvas, message: &str, is_error: bool, style: &Style) {
    draw_backdrop(canvas, style);
    let color = if is_error {
        style.palette.error
    } else {
        style.palette.dim
    };
    text::draw_centered(
        canvas,
        (canvas.height / 2) as f32 - BODY_SIZE,
        BODY_SIZE,
        color,
        message,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSIONS: &[&str] = &[
        "Sway", "GNOME", "Plasma", "Hyprland", "River", "Weston", "Cage", "Niri",
    ];

    fn names(count: usize) -> Vec<String> {
        SESSIONS[..count].iter().map(|s| (*s).to_owned()).collect()
    }

    fn view_with<'a>(sessions: &'a [String]) -> View<'a> {
        View { sessions, ..view() }
    }

    fn view() -> View<'static> {
        View {
            username: "testuser",
            display_name: "Test User",
            sessions: &[],
            session_index: 0,
            menu_open: false,
            prompt: Some("Password:"),
            answer: "hunter2",
            secret: true,
            error: None,
            info: None,
            launching: false,
            multiple_users: true,
            multiple_sessions: true,
        }
    }

    /// Every pixel opaque, or the login form would show whatever the framebuffer
    /// happened to contain through it.
    fn assert_opaque(canvas: &Canvas) {
        assert!(
            canvas.data.chunks_exact(4).all(|p| p[3] == 0xff),
            "found a transparent pixel"
        );
    }

    #[test]
    fn paints_without_panicking_at_any_size() {
        // Smaller than the panel, exactly the panel, and much larger.
        for (w, h) in [
            (1, 1),
            (320, 200),
            (PANEL_WIDTH, PANEL_HEIGHT),
            (3840, 2160),
        ] {
            let mut canvas = Canvas::new(w, h);
            paint(&mut canvas, &view(), &Style::default());
            assert_opaque(&canvas);
        }
    }

    #[test]
    fn secret_answers_are_masked() {
        if !text::have_font() {
            return;
        }
        let mut secret = Canvas::new(800, 600);
        paint(&mut secret, &view(), &Style::default());

        let mut visible = Canvas::new(800, 600);
        paint(
            &mut visible,
            &View {
                secret: false,
                ..view()
            },
            &Style::default(),
        );

        // The masked and unmasked renderings must differ, or the password is on
        // screen in plain text.
        assert_ne!(secret.data, visible.data);
    }

    #[test]
    fn very_long_answers_do_not_panic() {
        let long = "a".repeat(4096);
        let mut canvas = Canvas::new(800, 600);
        paint(
            &mut canvas,
            &View {
                answer: &long,
                secret: true,
                ..view()
            },
            &Style::default(),
        );
        paint(
            &mut canvas,
            &View {
                answer: &long,
                secret: false,
                ..view()
            },
            &Style::default(),
        );
    }

    #[test]
    fn an_echoed_answer_stays_inside_its_field() {
        if !text::have_font() {
            // The whole point is where the ink lands, so a host with no font
            // must say it proved nothing rather than pass quietly.
            eprintln!("skipped: no font available");
            return;
        }

        // The defect this guards: an echo-on prompt's answer was drawn whole
        // and clipped only by the canvas, so a long one wrote across the form
        // and off the panel.
        let (w, h) = (800, 600);
        let empty = render(w, h, "");
        let long = render(w, h, &"W".repeat(4096));

        let differing: Vec<(i32, i32)> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let o = ((y * w + x) * 4) as usize;
                empty.data[o..o + 4] != long.data[o..o + 4]
            })
            .collect();

        assert!(
            !differing.is_empty(),
            "nothing was echoed, so containment proves nothing"
        );

        let panel_x = (w - PANEL_WIDTH) / 2;
        let panel_y = (h - PANEL_HEIGHT) / 2;
        let (top, bottom) = (
            differing.iter().map(|p| p.1).min().unwrap(),
            differing.iter().map(|p| p.1).max().unwrap(),
        );
        for &(x, y) in &differing {
            assert!(
                x >= panel_x + PADDING
                    && x < panel_x + PANEL_WIDTH - PADDING
                    && y >= panel_y
                    && y < panel_y + PANEL_HEIGHT,
                "the answer drew outside the panel at {x},{y}"
            );
        }
        assert!(
            bottom - top < (BODY_SIZE * 1.9) as i32,
            "the answer drew outside the field's height: rows {top}..={bottom}"
        );
    }

    /// Paint one form whose only variable is the echoed answer.
    fn render(w: i32, h: i32, answer: &str) -> Canvas {
        let mut canvas = Canvas::new(w, h);
        paint(
            &mut canvas,
            &View {
                answer,
                secret: false,
                ..view()
            },
            &Style::default(),
        );
        canvas
    }

    #[test]
    fn error_and_info_are_both_drawn() {
        if !text::have_font() {
            return;
        }
        let mut plain = Canvas::new(800, 600);
        paint(&mut plain, &view(), &Style::default());

        let mut annotated = Canvas::new(800, 600);
        paint(
            &mut annotated,
            &View {
                error: Some("Authentication failure"),
                info: Some("Password expires in 3 days"),
                ..view()
            },
            &Style::default(),
        );

        assert_ne!(plain.data, annotated.data);
    }

    #[test]
    fn launching_replaces_the_form() {
        if !text::have_font() {
            return;
        }
        let mut canvas = Canvas::new(800, 600);
        paint(
            &mut canvas,
            &View {
                launching: true,
                ..view()
            },
            &Style::default(),
        );
        assert_opaque(&canvas);
    }

    #[test]
    fn message_screen_is_opaque() {
        let mut canvas = Canvas::new(400, 300);
        paint_message(&mut canvas, "Connecting…", false, &Style::default());
        assert_opaque(&canvas);
        paint_message(&mut canvas, "No users available", true, &Style::default());
        assert_opaque(&canvas);
    }

    #[test]
    fn short_lists_show_everything_and_never_scroll() {
        for selected in 0..MENU_ROWS {
            assert_eq!(menu_window(MENU_ROWS, selected, MENU_ROWS), 0..MENU_ROWS);
        }
        assert_eq!(menu_window(3, 2, MENU_ROWS), 0..3);
        assert_eq!(menu_window(0, 0, MENU_ROWS), 0..0);
    }

    #[test]
    fn long_lists_keep_the_selection_visible() {
        let len = 20;
        for selected in 0..len {
            let window = menu_window(len, selected, MENU_ROWS);
            assert_eq!(window.len(), MENU_ROWS, "window changed size at {selected}");
            assert!(
                window.contains(&selected),
                "selection {selected} fell outside {window:?}"
            );
        }
    }

    #[test]
    fn the_window_never_runs_past_either_end() {
        let len = 20;
        assert_eq!(menu_window(len, 0, MENU_ROWS).start, 0);
        let last = menu_window(len, len - 1, MENU_ROWS);
        assert_eq!(last.end, len);
    }

    #[test]
    fn the_menu_opens_downward_when_there_is_room() {
        // 100px of control, a 60px menu, 600px of screen: plenty below.
        assert_eq!(menu_origin(100, 60, 600, 28), 128);
    }

    #[test]
    fn the_menu_flips_above_when_it_would_run_off_the_bottom() {
        // Control near the bottom edge: opening downward would clip the list.
        assert_eq!(menu_origin(560, 100, 600, 28), 560 - 100 - 6);
    }

    #[test]
    fn a_menu_taller_than_the_screen_is_pinned_to_the_top() {
        // Neither direction fits; the first rows must still be readable.
        assert_eq!(menu_origin(300, 700, 600, 28), 0);
    }

    #[test]
    fn menu_paints_at_every_size_and_length() {
        // A list longer than the screen, and a screen too short for the panel:
        // neither may panic or draw outside the canvas.
        for (w, h) in [(320, 200), (800, 600), (1, 1)] {
            for count in [2, MENU_ROWS, SESSIONS.len()] {
                let sessions = names(count);
                let mut canvas = Canvas::new(w, h);
                paint(
                    &mut canvas,
                    &View {
                        menu_open: true,
                        session_index: count - 1,
                        ..view_with(&sessions)
                    },
                    &Style::default(),
                );
                assert_opaque(&canvas);
            }
        }
    }

    #[test]
    fn open_menu_changes_what_is_drawn() {
        if !text::have_font() {
            return;
        }
        let sessions = names(4);

        let mut closed = Canvas::new(800, 600);
        paint(&mut closed, &view_with(&sessions), &Style::default());

        let mut open = Canvas::new(800, 600);
        paint(
            &mut open,
            &View {
                menu_open: true,
                ..view_with(&sessions)
            },
            &Style::default(),
        );

        assert_ne!(closed.data, open.data, "the drop-down drew nothing");
    }

    #[test]
    fn the_highlighted_row_is_distinguishable() {
        if !text::have_font() {
            return;
        }
        let sessions = names(4);

        let mut first = Canvas::new(800, 600);
        paint(
            &mut first,
            &View {
                menu_open: true,
                session_index: 0,
                ..view_with(&sessions)
            },
            &Style::default(),
        );

        let mut second = Canvas::new(800, 600);
        paint(
            &mut second,
            &View {
                menu_open: true,
                session_index: 1,
                ..view_with(&sessions)
            },
            &Style::default(),
        );

        // Moving the selection must move the highlight, or the list gives the
        // user no feedback about what they are choosing.
        assert_ne!(first.data, second.data);
    }

    #[test]
    fn missing_display_name_falls_back_to_username() {
        if !text::have_font() {
            return;
        }
        let mut canvas = Canvas::new(800, 600);
        paint(
            &mut canvas,
            &View {
                display_name: "",
                ..view()
            },
            &Style::default(),
        );
        assert_opaque(&canvas);
    }

    /// The first pixel, decoded back to the u32 the palette speaks.
    fn corner(canvas: &Canvas) -> u32 {
        u32::from_ne_bytes(canvas.data[0..4].try_into().unwrap())
    }

    #[test]
    fn the_light_scheme_actually_changes_the_frame() {
        let light = Style {
            palette: Palette::light(),
            backdrop: Backdrop::Palette,
        };
        // Larger than the panel, so the corner shows the backdrop.
        let mut dark_canvas = Canvas::new(800, 600);
        paint(&mut dark_canvas, &view(), &Style::default());
        let mut light_canvas = Canvas::new(800, 600);
        paint(&mut light_canvas, &view(), &light);

        assert_ne!(dark_canvas.data, light_canvas.data);
        assert_eq!(corner(&light_canvas), Palette::light().background);
        assert_opaque(&light_canvas);
    }

    #[test]
    fn a_background_color_fills_behind_the_panel() {
        let style = Style {
            palette: Palette::dark(),
            backdrop: Backdrop::Color(0xff336699),
        };
        let mut canvas = Canvas::new(800, 600);
        paint(&mut canvas, &view(), &style);
        assert_eq!(corner(&canvas), 0xff336699);

        let mut message = Canvas::new(320, 200);
        paint_message(&mut message, "Connecting…", false, &style);
        assert_eq!(corner(&message), 0xff336699);
    }

    #[test]
    fn a_background_image_is_scaled_to_cover_and_centre_cropped() {
        // A 2x1 image on a square canvas must scale by height (the larger
        // ratio), leaving one source column visible: the crop is centred, so
        // both halves of the canvas sample from the middle of the image —
        // which for 2 columns means each half keeps its own column.
        let image = Image::new(2, 1, vec![0xffff0000, 0xff0000ff]);
        let mut canvas = Canvas::new(4, 4);
        image.draw_cover(&mut canvas);
        assert_eq!(corner(&canvas), 0xffff0000);
        let last = (4 * 4 - 1) * 4;
        assert_eq!(
            u32::from_ne_bytes(canvas.data[last..last + 4].try_into().unwrap()),
            0xff0000ff
        );
        assert_opaque(&canvas);
    }

    #[test]
    fn an_exact_fit_image_maps_pixel_for_pixel() {
        let image = Image::new(2, 2, vec![0xff102030, 0xff405060, 0xff708090, 0xffa0b0c0]);
        let mut canvas = Canvas::new(2, 2);
        image.draw_cover(&mut canvas);
        let px = |i: usize| u32::from_ne_bytes(canvas.data[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(
            [px(0), px(1), px(2), px(3)],
            [0xff102030, 0xff405060, 0xff708090, 0xffa0b0c0]
        );
    }

    #[test]
    fn a_png_background_loads_and_a_missing_one_is_an_error() {
        let dir = std::env::temp_dir().join(format!("wdm-greeter-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bg.png");

        // A 2x1 RGBA PNG: opaque red, half-transparent blue. The transparent
        // pixel must come back darkened and opaque, or the previous session's
        // framebuffer shows through the wallpaper.
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[0xff, 0, 0, 0xff, 0, 0, 0xff, 0x80])
            .unwrap();
        writer.finish().unwrap();

        let image = Image::load_png(&path).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.pixels[0], 0xffff0000);
        assert_eq!(image.pixels[1] >> 24, 0xff, "alpha must be composited away");
        assert!(
            image.pixels[1] & 0xff <= 0x81,
            "half-transparent blue must darken"
        );

        let missing = Image::load_png(&dir.join("nope.png")).unwrap_err();
        assert!(missing.contains("nope.png"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn from_config_surfaces_a_bad_image_as_an_error() {
        let config = crate::config::Config {
            color_scheme: ColorScheme::Light,
            background: Some(Background::Image("/nonexistent/wall.png".into())),
        };
        let err = Style::from_config(&config).unwrap_err();
        assert!(err.contains("wall.png"), "{err}");

        let plain = crate::config::Config {
            color_scheme: ColorScheme::Light,
            background: Some(Background::Color(0xff123456)),
        };
        let style = Style::from_config(&plain).unwrap();
        assert_eq!(style.palette, Palette::light());
        assert!(matches!(style.backdrop, Backdrop::Color(0xff123456)));
    }

    #[test]
    fn indexed_and_sixteen_bit_pngs_load_too() {
        // The defect this guards: the decoder was left on IDENTITY
        // transformations, so a colour-indexed PNG — what pngquant, optipng
        // and GIMP's indexed export all produce — arrived as ColorType::Indexed
        // and was refused, blaming the administrator's perfectly valid file.
        let dir = std::env::temp_dir().join(format!("wdm-greeter-idx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let indexed = dir.join("indexed.png");
        let file = std::fs::File::create(&indexed).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 2, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        // Palette: entry 0 red, entry 1 blue.
        encoder.set_palette(vec![0xff, 0, 0, 0, 0, 0xff]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0, 1]).unwrap();
        writer.finish().unwrap();

        let image = Image::load_png(&indexed).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.pixels, vec![0xffff0000, 0xff0000ff]);

        let deep = dir.join("sixteen.png");
        let file = std::fs::File::create(&deep).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().unwrap();
        // 16-bit big-endian: pure green at full depth.
        writer.write_image_data(&[0, 0, 0xff, 0xff, 0, 0]).unwrap();
        writer.finish().unwrap();

        let image = Image::load_png(&deep).unwrap();
        assert_eq!(image.pixels, vec![0xff00ff00]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_cover_cache_survives_a_resize() {
        // One Image drawn at two sizes: the cached frame for the first size
        // must not be pasted onto the second.
        let image = Image::new(2, 2, vec![0xff102030, 0xff405060, 0xff708090, 0xffa0b0c0]);
        let mut small = Canvas::new(2, 2);
        image.draw_cover(&mut small);

        let mut big = Canvas::new(4, 4);
        image.draw_cover(&mut big);
        let px =
            |c: &Canvas, i: usize| u32::from_ne_bytes(c.data[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(px(&big, 0), 0xff102030);
        assert_eq!(px(&big, 15), 0xffa0b0c0);

        // And back again, so the cache is a cache and not a latch.
        let mut small_again = Canvas::new(2, 2);
        image.draw_cover(&mut small_again);
        assert_eq!(small.data, small_again.data);
    }

    #[test]
    fn a_corrupt_png_is_an_error_not_a_blank_wallpaper() {
        // An administrator's file that exists and is readable but cannot be
        // decoded is a startup error, never a silent fallback.
        let dir = std::env::temp_dir().join(format!("wdm-greeter-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bg.png");
        std::fs::write(&path, b"not a png").unwrap();

        let err = Image::load_png(&path).unwrap_err();
        assert!(err.contains("bg.png"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
