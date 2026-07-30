//! The login form.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, DropDown, Entry,
    InputPurpose, Label, Orientation, StringList,
};
use gtk4_layer_shell::{Edge, Layer, LayerShell, KeyboardMode};

use crate::proto::{Shared, SharedLink};

const STYLE: &str = "
window { background: #12131a; }
.card {
    background: #1c1e28;
    border: 1px solid #2c2f3d;
    border-radius: 12px;
    padding: 36px;
    min-width: 420px;
}
.title { font-size: 22pt; font-weight: 700; color: #e8e8ef; margin-bottom: 8px; }
.hint { color: #8b8fa3; font-size: 10pt; }
.error { color: #ff7b72; font-size: 10pt; }
.card entry {
    min-height: 38px;
    background: #0d0e13;
    color: #e8e8ef;
    border: 1px solid #2c2f3d;
}
.card entry:focus-within { border-color: #6f9dff; }
.card dropdown > button {
    min-height: 34px;
    background: #0d0e13;
    color: #e8e8ef;
    border: 1px solid #2c2f3d;
}
.card button.text-button {
    min-height: 36px;
    padding: 0 20px;
    background: #6f9dff;
    color: #0d0e13;
    font-weight: 600;
}
";

/// The widgets, and the small amount of state that is the greeter's own rather
/// than the compositor's.
pub struct Ui {
    users: DropDown,
    sessions: DropDown,
    prompt: Label,
    entry: Entry,
    error: Label,
    info: Label,
    submit: Button,

    model: Shared,
    link: SharedLink,

    /// Model revision already applied, so a refresh that changes nothing does
    /// not stomp on what the user is typing.
    applied: Cell<u64>,
    /// True once `start_session` has been sent; the compositor tears this
    /// process down shortly afterwards.
    launched: Cell<bool>,
    /// Guards the automatic retry, so a user switch mid-flight does not stack
    /// conversations.
    attempting: Cell<bool>,
}

/// Build the window and wire it up.
pub fn build(app: &Application, model: Shared, link: SharedLink) -> (ApplicationWindow, Rc<Ui>) {
    // A login screen runs before any user session, so there is no per-user theme
    // preference to honour and no settings daemon to ask. Without this GTK falls
    // back to the light Adwaita theme, whose entries and buttons are unreadable
    // against the dark background below.
    if let Some(settings) = gtk4::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }

    let provider = CssProvider::new();
    provider.load_from_data(STYLE);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("wdm")
        .build();

    // Layer shell is mandatory, not cosmetic: wdm exposes no xdg_toplevel, so an
    // ordinary window is closed the moment it is created.
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("wdm-greeter"));
    // Exclusive keyboard: this is a login screen, so nothing else may hold the
    // keyboard while it is up.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // No monitor is set on purpose: wdm places a layer surface with no output on
    // the rank 0 output, and moves it when ranks change on hotplug. Choosing one
    // here would only reimplement that, less well.

    let ui = Rc::new(Ui {
        users: DropDown::new(None::<StringList>, None::<gtk4::Expression>),
        sessions: DropDown::new(None::<StringList>, None::<gtk4::Expression>),
        prompt: Label::builder().halign(Align::Start).build(),
        entry: Entry::builder().activates_default(true).build(),
        error: Label::builder()
            .halign(Align::Start)
            .wrap(true)
            .css_classes(["error"])
            .build(),
        info: Label::builder()
            .halign(Align::Start)
            .wrap(true)
            .css_classes(["hint"])
            .build(),
        submit: Button::with_label("Log in"),
        model,
        link,
        applied: Cell::new(u64::MAX),
        launched: Cell::new(false),
        attempting: Cell::new(false),
    });

    window.set_child(Some(&layout(&ui)));
    connect(&ui);

    // Populate from the enumerate phase that Link::connect already collected.
    ui.reload_lists();
    ui.begin_auth();

    if std::env::var_os("WDM_GREETER_OPEN_SESSIONS").is_some() {
        let sessions = ui.sessions.clone();
        gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            sessions.activate();
        });
    }

    (window, ui)
}

fn layout(ui: &Ui) -> GtkBox {
    let card = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .halign(Align::Center)
        .valign(Align::Center)
        .css_classes(["card"])
        .build();

    let title = Label::builder()
        .label("Sign in")
        .halign(Align::Start)
        .css_classes(["title"])
        .build();

    card.append(&title);
    card.append(&labelled("User", &ui.users));
    card.append(&ui.prompt);
    card.append(&ui.entry);
    card.append(&ui.error);
    card.append(&ui.info);
    card.append(&labelled("Session", &ui.sessions));

    ui.submit.set_halign(Align::End);
    card.append(&ui.submit);

    let root = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .halign(Align::Center)
        .valign(Align::Center)
        .build();
    root.append(&card);
    root
}

fn labelled(text: &str, widget: &impl IsA<gtk4::Widget>) -> GtkBox {
    let row = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .build();
    row.append(
        &Label::builder()
            .label(text)
            .halign(Align::Start)
            .css_classes(["hint"])
            .build(),
    );
    row.append(widget);
    row
}

fn connect(ui: &Rc<Ui>) {
    ui.entry.connect_activate({
        let ui = ui.clone();
        move |_| ui.submit()
    });

    ui.submit.connect_clicked({
        let ui = ui.clone();
        move |_| ui.submit()
    });

    // Switching user restarts the conversation: PAM's is per user, and a
    // half-answered one for someone else is not reusable.
    ui.users.connect_selected_notify({
        let ui = ui.clone();
        move |_| {
            ui.select_preferred_session();
            ui.begin_auth();
        }
    });
}

impl Ui {
    /// Rebuild the user and session lists from the model.
    fn reload_lists(&self) {
        let model = self.model.borrow();

        let users: Vec<String> = model.users.iter().map(|u| u.label()).collect();
        self.users
            .set_model(Some(&StringList::new(&refs(&users))));

        let sessions: Vec<String> = model.sessions.iter().map(|s| s.name.clone()).collect();
        self.sessions
            .set_model(Some(&StringList::new(&refs(&sessions))));

        // A machine with nothing to log into is worth saying out loud rather
        // than presenting an empty form.
        let usable = !model.users.is_empty() && !model.sessions.is_empty();
        self.entry.set_sensitive(usable);
        self.submit.set_sensitive(usable);
        if !usable {
            self.error.set_label(if model.users.is_empty() {
                "No users available to log in"
            } else {
                "No sessions installed"
            });
        }

        drop(model);
        self.select_preferred_session();
    }

    fn select_preferred_session(&self) {
        let index = self
            .model
            .borrow()
            .preferred_session(self.users.selected() as usize);
        self.sessions.set_selected(index as u32);
    }

    /// Start a fresh conversation for the selected user.
    fn begin_auth(&self) {
        let model = self.model.borrow();
        let Some(greeter) = &model.greeter else {
            return;
        };
        let Some(user) = model.users.get(self.users.selected() as usize) else {
            return;
        };
        if self.launched.get() {
            return;
        }

        // Cancel first: the compositor rejects create_session while another
        // conversation is live, and switching user is exactly that case.
        greeter.cancel();
        greeter.create_session(user.name.clone());
        self.attempting.set(true);

        drop(model);
        self.entry.set_text("");
        self.prompt.set_label("Waiting…");
    }

    /// Answer the pending prompt, or start an attempt if there is none.
    fn submit(&self) {
        let text = self.entry.text().to_string();

        let pending = {
            let model = self.model.borrow();
            let Some(greeter) = &model.greeter else {
                return;
            };

            match &model.prompt {
                Some(prompt) => {
                    greeter.respond(prompt.id, text);
                    true
                }
                None => false,
            }
        };

        if pending {
            self.entry.set_text("");
            self.model.borrow_mut().prompt = None;
            self.prompt.set_label("Checking…");
            self.flush();
        } else if !self.attempting.get() {
            self.begin_auth();
            self.flush();
        }
    }

    /// Push queued requests to the compositor.
    ///
    /// GTK's main loop does not know about our queue, so nothing else would.
    fn flush(&self) {
        let mut model = self.model.borrow_mut();
        self.link.borrow_mut().pump(&mut model);
    }

    /// Apply the model to the widgets.
    pub fn refresh(&self) {
        let revision = self.model.borrow().revision;
        if self.applied.get() == revision {
            return;
        }
        self.applied.set(revision);

        // Lists only arrive once, on the enumerate phase.
        if self.users.model().is_none_or(|m| m.n_items() == 0) {
            self.reload_lists();
        }

        let (prompt, error, info, authenticated) = {
            let model = self.model.borrow();
            (
                model.prompt.clone(),
                model.error.clone(),
                model.info.clone(),
                model.authenticated,
            )
        };

        self.error.set_label(error.as_deref().unwrap_or(""));
        self.error.set_visible(error.is_some());
        self.info.set_label(info.as_deref().unwrap_or(""));
        self.info.set_visible(info.is_some());

        if let Some(prompt) = &prompt {
            self.attempting.set(true);
            self.prompt.set_label(&prompt.text);
            // PAM decides whether the answer is a secret; a token or a username
            // prompt is echoed, a password is not.
            self.entry.set_visibility(!prompt.secret);
            self.entry.set_input_purpose(if prompt.secret {
                InputPurpose::Password
            } else {
                InputPurpose::FreeForm
            });
            self.entry.grab_focus();
        }

        if authenticated && !self.launched.get() {
            self.launch();
            return;
        }

        // A failure with no prompt means the conversation is over. Start another
        // so the user can simply retype: the compositor delays its refusal until
        // the rate limit expires, so this waits rather than spinning.
        if error.is_some() && prompt.is_none() && !authenticated {
            self.attempting.set(false);
            self.prompt.set_label("");
            self.begin_auth();
            self.flush();
        }
    }

    fn launch(&self) {
        let model = self.model.borrow();
        let Some(greeter) = &model.greeter else {
            return;
        };
        let Some(session) = model.session_id(self.sessions.selected() as usize) else {
            log::error!("authenticated but no session is selected");
            return;
        };

        log::info!("starting session {session}");
        self.launched.set(true);
        // No extra environment: anything this greeter could add, wdm already
        // sets more authoritatively, and it filters the rest.
        greeter.start_session(session.to_owned(), Vec::new());

        drop(model);
        self.prompt.set_label("Starting session…");
        self.entry.set_sensitive(false);
        self.submit.set_sensitive(false);
        self.flush();
    }
}

fn refs(items: &[String]) -> Vec<&str> {
    items.iter().map(String::as_str).collect()
}
