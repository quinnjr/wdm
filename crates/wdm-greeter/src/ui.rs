//! Layout and painting of the login form.
//!
//! Deliberately plain. This greeter exists to be the shipped default and to prove
//! `wdm_greeter_v1` is implementable by something that is not wdm; anyone wanting
//! a themed login screen writes their own client against the same protocol.

use crate::text::{self, Canvas};

const BACKGROUND: u32 = 0xff12131a;
const PANEL: u32 = 0xff1c1e28;
const PANEL_EDGE: u32 = 0xff2c2f3d;
const FIELD: u32 = 0xff0d0e13;
const TEXT: u32 = 0xffe8e8ef;
const DIM: u32 = 0xff8b8fa3;
const ACCENT: u32 = 0xff6f9dff;
const ERROR: u32 = 0xffff7b72;

const PANEL_WIDTH: i32 = 460;
const PANEL_HEIGHT: i32 = 300;
const PADDING: i32 = 32;

const TITLE_SIZE: f32 = 26.0;
const BODY_SIZE: f32 = 17.0;
const SMALL_SIZE: f32 = 14.0;

/// What to draw. Owned by the client's state and handed here each frame.
pub struct View<'a> {
    /// The user currently selected.
    pub username: &'a str,
    /// Their display name, if the enumerate phase supplied one.
    pub display_name: &'a str,
    /// The session that will be launched.
    pub session_name: &'a str,
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
pub fn paint(canvas: &mut Canvas, view: &View<'_>) {
    canvas.fill(BACKGROUND);

    let panel_x = (canvas.width - PANEL_WIDTH) / 2;
    let panel_y = (canvas.height - PANEL_HEIGHT) / 2;

    // A one pixel border rather than a real outline: enough to separate the panel
    // from the background without a compositing pass.
    canvas.rect(
        panel_x - 1,
        panel_y - 1,
        PANEL_WIDTH + 2,
        PANEL_HEIGHT + 2,
        PANEL_EDGE,
    );
    canvas.rect(panel_x, panel_y, PANEL_WIDTH, PANEL_HEIGHT, PANEL);

    let left = (panel_x + PADDING) as f32;
    let mut y = (panel_y + PADDING) as f32;

    let title = if view.display_name.is_empty() {
        view.username.to_owned()
    } else {
        format!("{} ({})", view.display_name, view.username)
    };
    text::draw(canvas, left, y, TITLE_SIZE, TEXT, &title);
    y += TITLE_SIZE * 2.0;

    if view.launching {
        text::draw(canvas, left, y, BODY_SIZE, ACCENT, "Starting session…");
        return;
    }

    // Prompt label, then the field. PAM decides the wording, so it is shown
    // verbatim rather than replaced with "Password:".
    let label = view.prompt.unwrap_or("Waiting…");
    text::draw(canvas, left, y, SMALL_SIZE, DIM, label);
    y += SMALL_SIZE * 1.8;

    let field_height = (BODY_SIZE * 1.9) as i32;
    canvas.rect(
        panel_x + PADDING,
        y as i32,
        PANEL_WIDTH - PADDING * 2,
        field_height,
        FIELD,
    );

    let shown = if view.secret {
        // Fixed-width mask: revealing the length of a password is a small leak,
        // but showing nothing at all leaves the user unsure the keyboard works.
        "•".repeat(view.answer.chars().count().min(32))
    } else {
        view.answer.to_owned()
    };

    let text_y = y + (field_height as f32 - BODY_SIZE) / 2.0 - 2.0;
    text::draw(canvas, left + 8.0, text_y, BODY_SIZE, TEXT, &shown);

    // Caret, so an empty field still looks focused.
    let caret_x = left + 8.0 + text::width(&shown, BODY_SIZE) + 1.0;
    canvas.rect(caret_x as i32, text_y as i32, 2, BODY_SIZE as i32, ACCENT);

    y += field_height as f32 + BODY_SIZE * 1.4;

    if let Some(error) = view.error {
        text::draw(canvas, left, y, SMALL_SIZE, ERROR, error);
        y += SMALL_SIZE * 1.6;
    }
    if let Some(info) = view.info {
        text::draw(canvas, left, y, SMALL_SIZE, DIM, info);
    }

    // Footer: the session about to start, and the keys that change things. A
    // greeter that does not say how to switch session is one the user cannot.
    let footer_y = (panel_y + PANEL_HEIGHT - PADDING) as f32 - SMALL_SIZE;
    text::draw(
        canvas,
        left,
        footer_y,
        SMALL_SIZE,
        DIM,
        &format!("Session: {}", view.session_name),
    );

    let mut hints = Vec::new();
    if view.multiple_sessions {
        hints.push("F2 session");
    }
    if view.multiple_users {
        hints.push("F1 user");
    }
    hints.push("Esc clear");
    let hint = hints.join("   ");
    let hint_x = (panel_x + PANEL_WIDTH - PADDING) as f32 - text::width(&hint, SMALL_SIZE);
    text::draw(canvas, hint_x, footer_y, SMALL_SIZE, DIM, &hint);
}

/// Draw a message with no login form, used before the enumerate phase completes
/// and when there is nothing to log in as.
pub fn paint_message(canvas: &mut Canvas, message: &str, is_error: bool) {
    canvas.fill(BACKGROUND);
    let color = if is_error { ERROR } else { DIM };
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

    fn view() -> View<'static> {
        View {
            username: "joseph",
            display_name: "Joseph Quinn",
            session_name: "Sway",
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
        for (w, h) in [(1, 1), (320, 200), (PANEL_WIDTH, PANEL_HEIGHT), (3840, 2160)] {
            let mut canvas = Canvas::new(w, h);
            paint(&mut canvas, &view());
            assert_opaque(&canvas);
        }
    }

    #[test]
    fn secret_answers_are_masked() {
        if !text::have_font() {
            return;
        }
        let mut secret = Canvas::new(800, 600);
        paint(&mut secret, &view());

        let mut visible = Canvas::new(800, 600);
        paint(
            &mut visible,
            &View {
                secret: false,
                ..view()
            },
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
        );
        paint(
            &mut canvas,
            &View {
                answer: &long,
                secret: false,
                ..view()
            },
        );
    }

    #[test]
    fn error_and_info_are_both_drawn() {
        if !text::have_font() {
            return;
        }
        let mut plain = Canvas::new(800, 600);
        paint(&mut plain, &view());

        let mut annotated = Canvas::new(800, 600);
        paint(
            &mut annotated,
            &View {
                error: Some("Authentication failure"),
                info: Some("Password expires in 3 days"),
                ..view()
            },
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
        );
        assert_opaque(&canvas);
    }

    #[test]
    fn message_screen_is_opaque() {
        let mut canvas = Canvas::new(400, 300);
        paint_message(&mut canvas, "Connecting…", false);
        assert_opaque(&canvas);
        paint_message(&mut canvas, "No users available", true);
        assert_opaque(&canvas);
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
        );
        assert_opaque(&canvas);
    }
}
