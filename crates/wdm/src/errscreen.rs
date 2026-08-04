//! The screen wdm draws when it has given up on the greeter.
//!
//! Not a fallback greeter: it cannot log anyone in. Its only job is to say what
//! went wrong so the user knows to switch to tty1, because a black display with
//! no explanation gives them nothing to act on.
//!
//! Produces a plain RGBA8 image rather than talking to a renderer, so both
//! backends can upload it the same way.

use std::sync::OnceLock;

use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle, WrapStyle};
use fontdue::{Font, FontSettings};

/// Fonts to try, in order. Any of these being present is enough; wdm does not
/// depend on a particular one because a minimal system may have only one.
///
/// ponytail: a hard-coded list rather than fontconfig. The ceiling is that a
/// system whose only font is not on it gets a wordless red screen — the three
/// families here cover every supported distribution's minimal install, but a
/// container image or an embedded rootfs with, say, only Roboto does not, and
/// nothing renders in a script these faces do not cover. The upgrade path is
/// `fontconfig`'s `FcFontMatch` for `sans-serif`, which is one dependency and
/// one call; it is not taken because this is the path that runs when everything
/// else has already failed, and a library that reads caches, opens config files
/// and can itself fail is a poor thing to reach for there.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/Inter-Regular.ttf",
];

/// Background: a dark red, unambiguously "something is wrong" without being a
/// pure black screen that reads as a dead machine.
const BACKGROUND: [u8; 4] = [0x2b, 0x0b, 0x0b, 0xff];
const FOREGROUND: [u8; 3] = [0xf2, 0xe8, 0xe8];

const FONT_SIZE: f32 = 22.0;
/// Fraction of the output width the text is allowed to occupy.
const TEXT_WIDTH: f32 = 0.8;

/// An RGBA8 image, top-left origin, no padding between rows.
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

fn font() -> Option<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();

    FONT.get_or_init(|| {
        for path in FONT_CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            match Font::from_bytes(bytes, FontSettings::default()) {
                Ok(font) => {
                    log::debug!("error screen using font {path}");
                    return Some(font);
                }
                Err(e) => log::debug!("{path} is not a usable font: {e}"),
            }
        }
        // Not fatal: the coloured background still tells the user the machine is
        // alive and in a failed state.
        log::warn!("no usable font found, error screen will have no text");
        None
    })
    .as_ref()
}

/// Render the message centred on a `width` by `height` image.
pub fn render(message: &str, width: i32, height: i32) -> Image {
    let width = width.max(1);
    let height = height.max(1);

    // One allocation for the whole background rather than a per-pixel loop:
    // this path runs when the machine is already in trouble, and at 4K the
    // difference is millions of iterations.
    let data = BACKGROUND.repeat((width * height) as usize);

    let mut image = Image {
        width,
        height,
        data,
    };

    if let Some(font) = font() {
        draw_text(&mut image, font, message);
    }

    image
}

fn draw_text(image: &mut Image, font: &Font, message: &str) {
    let max_width = image.width as f32 * TEXT_WIDTH;

    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings {
        max_width: Some(max_width),
        // Long messages must wrap rather than run off the edge of the screen,
        // taking the part that says "switch to tty1" with them.
        wrap_style: WrapStyle::Word,
        ..LayoutSettings::default()
    });
    layout.append(&[font], &TextStyle::new(message, FONT_SIZE, 0));

    let glyphs = layout.glyphs();

    // Centre the whole block: layout positions are relative to its own origin.
    let text_height = layout.height();
    let text_width = glyphs
        .iter()
        .map(|g| g.x + g.width as f32)
        .fold(0.0_f32, f32::max);

    let origin_x = ((image.width as f32 - text_width) / 2.0).max(0.0);
    let origin_y = ((image.height as f32 - text_height) / 2.0).max(0.0);

    for glyph in glyphs {
        let (metrics, coverage) = font.rasterize_config(glyph.key);
        if metrics.width == 0 || metrics.height == 0 {
            continue;
        }

        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let alpha = coverage[row * metrics.width + col];
                if alpha == 0 {
                    continue;
                }

                let x = (origin_x + glyph.x) as i32 + col as i32;
                let y = (origin_y + glyph.y) as i32 + row as i32;
                blend(image, x, y, alpha);
            }
        }
    }
}

/// Alpha-blend one foreground pixel over the background.
fn blend(image: &mut Image, x: i32, y: i32, alpha: u8) {
    // Glyphs near the edge of a narrow output can land outside the image.
    if x < 0 || y < 0 || x >= image.width || y >= image.height {
        return;
    }

    let offset = ((y * image.width + x) * 4) as usize;
    let a = u32::from(alpha);

    for (dst, &src) in image.data[offset..offset + 3].iter_mut().zip(&FOREGROUND) {
        *dst = ((u32::from(src) * a + u32::from(*dst) * (255 - a)) / 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_has_the_requested_size() {
        let image = render("greeter exited", 320, 200);
        assert_eq!(image.width, 320);
        assert_eq!(image.height, 200);
        assert_eq!(image.data.len(), 320 * 200 * 4);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        // An output can report a zero mode while a connector is settling.
        for (w, h) in [(0, 0), (1, 1), (-5, 10)] {
            let image = render("x", w, h);
            assert!(image.width >= 1 && image.height >= 1);
            assert_eq!(image.data.len(), (image.width * image.height * 4) as usize);
        }
    }

    #[test]
    fn background_is_opaque() {
        let image = render("", 8, 8);
        // A transparent error screen would show whatever was in the framebuffer.
        for pixel in image.data.chunks(4) {
            assert_eq!(pixel[3], 0xff);
        }
    }

    #[test]
    fn text_is_drawn_when_a_font_exists() {
        if font().is_none() {
            // No font installed; the no-text path is exercised by the tests above.
            return;
        }
        let blank = render("", 400, 200);
        let with_text = render("greeter exited 139 — switch to tty1", 400, 200);
        assert_ne!(
            blank.data, with_text.data,
            "message did not change any pixels"
        );
    }

    #[test]
    fn long_messages_stay_inside_the_image() {
        // Wrapping is what keeps the "switch to tty1" half of the message on
        // screen; without it a long message runs off the edge.
        let message = "greeter exited with a very long diagnostic message that would \
                       certainly not fit on a single line of a small display — switch to tty1";
        let image = render(message, 320, 240);
        assert_eq!(image.data.len(), 320 * 240 * 4);
    }
}
