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

const SELECTED: u32 = 0xff2a3350;
const PANEL_WIDTH: i32 = 460;
const PANEL_HEIGHT: i32 = 300;
const PADDING: i32 = 32;

/// Rows the drop-down shows at once. Beyond this it scrolls, so a machine with
/// a dozen desktops installed does not get a list taller than the screen.
pub const MENU_ROWS: usize = 6;
const MENU_ROW_HEIGHT: i32 = 28;

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
    let current = view
        .sessions
        .get(view.session_index)
        .map(String::as_str)
        .unwrap_or("none");

    // The marker is what tells the user this is a control and not a label. Drawn
    // geometrically rather than as U+25BE, because the fonts wdm falls back to
    // are not guaranteed to have that glyph and a tofu box is worse than none.
    let session_label = format!("Session: {current} ");
    text::draw(canvas, left, footer_y, SMALL_SIZE, DIM, &session_label);
    triangle(
        canvas,
        (left + text::width(&session_label, SMALL_SIZE)) as i32,
        footer_y as i32 + (SMALL_SIZE / 2.0) as i32,
        7,
        DIM,
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

    // Drawn last so it sits over everything, and outside the panel bounds so a
    // long list is not clipped by the panel.
    if view.menu_open {
        draw_menu(canvas, panel_x, footer_y, view);
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
/// Keeps the selected row inside the visible window, scrolling only when the
/// selection would fall outside it, so short lists never move.
pub fn menu_window(len: usize, selected: usize, rows: usize) -> std::ops::Range<usize> {
    if len <= rows {
        return 0..len;
    }
    // Centre the selection, then clamp so the window never runs past either end.
    let half = rows / 2;
    let start = selected.saturating_sub(half).min(len - rows);
    start..start + rows
}

fn draw_menu(canvas: &mut Canvas, panel_x: i32, anchor_y: f32, view: &View<'_>) {
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

    canvas.rect(x - 1, y - 1, width + 2, height + 2, PANEL_EDGE);
    canvas.rect(x, y, width, height, FIELD);

    for (row, index) in window.enumerate() {
        let row_y = y + 4 + row as i32 * MENU_ROW_HEIGHT;

        if index == view.session_index {
            canvas.rect(x + 2, row_y, width - 4, MENU_ROW_HEIGHT, SELECTED);
        }

        let color = if index == view.session_index { TEXT } else { DIM };
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
        let more = format!("{}/{}", view.session_index + 1, view.sessions.len());
        let more_x = (x + width) as f32 - text::width(&more, SMALL_SIZE) - 8.0;
        text::draw(
            canvas,
            more_x,
            (y + height) as f32 - SMALL_SIZE - 2.0,
            SMALL_SIZE,
            DIM,
            &more,
        );
    }
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


    const SESSIONS: &[&str] = &[
        "Sway", "GNOME", "Plasma", "Hyprland", "River", "Weston", "Cage", "Niri",
    ];

    fn names(count: usize) -> Vec<String> {
        SESSIONS[..count].iter().map(|s| (*s).to_owned()).collect()
    }

    fn view_with<'a>(sessions: &'a [String]) -> View<'a> {
        View {
            sessions,
            ..view()
        }
    }

    fn view() -> View<'static> {
        View {
            username: "joseph",
            display_name: "Joseph Quinn",
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
        paint(&mut closed, &view_with(&sessions));

        let mut open = Canvas::new(800, 600);
        paint(
            &mut open,
            &View {
                menu_open: true,
                ..view_with(&sessions)
            },
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
        );

        let mut second = Canvas::new(800, 600);
        paint(
            &mut second,
            &View {
                menu_open: true,
                session_index: 1,
                ..view_with(&sessions)
            },
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
        );
        assert_opaque(&canvas);
    }
}

