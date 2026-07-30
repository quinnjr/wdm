//! The login form.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, DropDown, Entry,
    InputPurpose, Label, Orientation, StringList,
};
use gtk4_layer_shell::{Edge, Layer, LayerShell, KeyboardMode};

use wdm_greeter_client::{Shared, SharedLink};

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
.notice {
    color: #e8e8ef;
    font-size: 10pt;
    background: rgba(255, 123, 114, 0.12);
    border-left: 3px solid #ff7b72;
    padding: 8px 10px;
    border-radius: 4px;
}
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
    /// PAM's explanation of the account's state. Sticky, unlike `error`.
    notice: Label,
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
        notice: Label::builder()
            .halign(Align::Start)
            .wrap(true)
            .xalign(0.0)
            .css_classes(["notice"])
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
    card.append(&ui.notice);
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
    // half-answered one for someone else is not reusable. This is a deliberate
    // restart, so the in-flight guard is cleared before asking.
    ui.users.connect_selected_notify({
        let ui = ui.clone();
        move |_| {
            ui.select_preferred_session();
            ui.attempting.set(false);
            ui.begin_auth();
        }
    });
}

impl Ui {
    /// Rebuild the user and session lists from the model.
    fn reload_lists(&self) {
        // Everything is read out first and the borrow released before any widget
        // is touched. `set_model` emits `notify::selected` *synchronously*, and
        // that handler re-enters this type and borrows the model again — holding
        // a borrow across it is a panic waiting for the right handler.
        let (users, sessions, no_users, no_sessions) = {
            let model = self.model.borrow();
            (
                model.users.iter().map(|u| u.label()).collect::<Vec<_>>(),
                model
                    .sessions
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>(),
                model.users.is_empty(),
                model.sessions.is_empty(),
            )
        };

        self.users.set_model(Some(&StringList::new(&refs(&users))));
        self.sessions
            .set_model(Some(&StringList::new(&refs(&sessions))));

        // A machine with nothing to log into is worth saying out loud rather
        // than presenting an empty form.
        let usable = !no_users && !no_sessions;
        self.entry.set_sensitive(usable);
        self.submit.set_sensitive(usable);
        if !usable {
            self.error.set_label(if no_users {
                "No users available to log in"
            } else {
                "No sessions installed"
            });
            self.error.set_visible(true);
        }

        self.select_preferred_session();
    }

    fn select_preferred_session(&self) {
        // Scoped: set_selected re-enters through notify::selected.
        let index = {
            let model = self.model.borrow();
            model.preferred_session(self.users.selected() as usize)
        };
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
        // Idempotent while an attempt is in flight. `reload_lists` emits
        // notify::selected, whose handler starts a conversation, and build()
        // then asks for one itself — without this the greeter opens two before
        // the user has touched anything, burning a rate-limit slot and making
        // PAM do every attempt twice. The paths that restart deliberately
        // (user switch, auto-retry) clear the flag first.
        if self.attempting.get() {
            return;
        }

        // Cancel first: the compositor rejects create_session while another
        // conversation is live, and switching user is exactly that case.
        greeter.cancel();
        greeter.create_session(user.name.clone());
        self.attempting.set(true);

        drop(model);

        // The user chose to try again, so the previous attempt's verdict and
        // any explanation of it have been read. Safe to take a mutable borrow
        // here: reload_lists releases its own before touching any widget, so
        // nothing re-enters with the model still borrowed.
        self.model.borrow_mut().begin_attempt();

        self.entry.set_text("");
        self.prompt.set_label("Waiting…");
        self.error.set_visible(false);
        self.notice.set_visible(false);
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

        let (prompt, error, notice, authenticated, auto_retry) = {
            let model = self.model.borrow();
            (
                model.prompt.clone(),
                model.error.clone(),
                model.notice.clone(),
                model.authenticated,
                model.should_auto_retry(),
            )
        };

        self.error.set_label(error.as_deref().unwrap_or(""));
        self.error.set_visible(error.is_some());

        // The notice is what PAM said about the account rather than about the
        // password, so it stays on screen until the user acts on it.
        self.notice.set_label(notice.as_deref().unwrap_or(""));
        self.notice.set_visible(notice.is_some());

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

        // Restarting is safe: the compositor defers its refusal until the rate
        // limit expires, so this waits rather than spinning. It is suppressed
        // when PAM explained itself — see Model::should_auto_retry.
        if auto_retry {
            self.attempting.set(false);
            self.prompt.set_label("");
            self.begin_auth();
            self.flush();
        } else if notice.is_some() && !authenticated {
            // Waiting on the user instead. Say so, or the form looks stuck.
            self.attempting.set(false);
            self.prompt.set_label("Press Enter to try again");
            self.entry.set_text("");
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
