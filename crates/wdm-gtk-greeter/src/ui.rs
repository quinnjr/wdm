//! The login form.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, DropDown, Entry,
    InputPurpose, Label, Orientation, StringList,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use wdm_greeter_client::{Model, Shared, SharedLink};

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
    /// Whether a conversation is in flight.
    ///
    /// Guards `begin_auth` against re-entry, so a second `submit` while PAM is
    /// still working does not open a conversation on top of the live one, and
    /// makes the end-of-conversation reset in `refresh` fire once on the
    /// transition rather than on every later revision — which would clear the
    /// entry under someone already typing their next attempt.
    attempting: Cell<bool>,
    /// The user and session lists arrive exactly once, in the enumerate phase
    /// Link::connect already waited for, so loading them is a latch. Asking the
    /// DropDown whether its model was empty instead was permanently true on a
    /// machine with zero users, so every revision change rebuilt both lists and
    /// re-entered `begin_auth` through `notify::selected`.
    lists_loaded: Cell<bool>,
    /// Nesting depth of `flush`, which reaches `refresh`, which can flush again.
    /// See [`Ui::flush`].
    depth: Cell<u32>,
    /// What the user typed before there was a prompt to put it in.
    ///
    /// The greeter no longer opens a PAM conversation until the user submits
    /// something, so the first thing they type arrives before PAM has asked for
    /// it. Held here across `create_session`, then handed to the first prompt
    /// that wants an answer — otherwise the user would type a password, watch
    /// the field clear, and have to type it again.
    pending_answer: RefCell<Option<String>>,
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
        // Masked from the very first frame. `refresh` sets this from
        // `prompt.secret` when PAM asks, but PAM is no longer asked until the
        // user submits — so the state the login screen sits in, and the one they
        // type their password into, is this one. Defaulting to visible put the
        // password on screen in plaintext.
        //
        // Secret is the right default rather than merely the safe one: every
        // PAM stack that reaches a greeter opens with a password. A stack whose
        // first question is a username or a token unmasks it on arrival, which
        // is the direction that can be corrected on screen — the other cannot.
        entry: Entry::builder()
            .activates_default(true)
            .visibility(false)
            .input_purpose(InputPurpose::Password)
            .build(),
        // Both start hidden. A GTK widget is visible on construction, and these
        // two are only ever hidden by `refresh`, which runs when the model's
        // revision changes — so on a greeter nobody has touched it does not run
        // at all, and the labels sat there empty. Their CSS gives them a
        // background, so an empty `error` is a small blank red box under the
        // entry: an alarm with nothing behind it, on a screen whose whole job is
        // to be trustworthy.
        //
        // Hidden is also what `refresh` itself decides for them —
        // `set_visible(error.is_some())` — so this is the same rule applied one
        // frame earlier rather than a second opinion about it.
        error: Label::builder()
            .halign(Align::Start)
            .wrap(true)
            .visible(false)
            .css_classes(["error"])
            .build(),
        notice: Label::builder()
            .halign(Align::Start)
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .css_classes(["notice"])
            .build(),
        submit: Button::with_label("Log in"),
        model,
        link,
        applied: Cell::new(u64::MAX),
        launched: Cell::new(false),
        attempting: Cell::new(false),
        lists_loaded: Cell::new(false),
        depth: Cell::new(0),
        pending_answer: RefCell::new(None),
    });

    window.set_child(Some(&layout(&ui)));
    connect(&ui);

    // The enumerate phase can carry `last_error` — the compositor's account of
    // why the previous session ended, e.g. a session that crashed straight back
    // to the greeter. The notify::selected handler that `reload_lists` fires can
    // reach `begin_attempt`, which clears `error` for a fresh conversation —
    // wiping the one message the user most needs before it is ever painted.
    // Snapshot it now and put it back afterwards.
    //
    // These three statements — snapshot, reload_lists, restore — are what
    // `an_enumerate_phase_error_survives_the_first_paint` stands in for; it
    // cannot drive them directly because reload_lists needs a display. Change
    // the order here and change it there.
    let enumerate_error = ui.model.borrow().error.clone();

    // Populate from the enumerate phase that Link::connect already collected.
    //
    // No conversation is started here, deliberately. Opening one costs a PAM
    // attempt for a user who has not touched the machine, and there is no way to
    // end a `pam_authenticate` that is waiting on a prompt without failing it —
    // so an unattended login screen was charging `pam_faillock` for its own
    // patience and locking the account out. `submit` is now the only thing that
    // arms PAM. See `Ui::begin_auth`.
    ui.reload_lists();
    ui.wait_for_user();

    restore_enumerate_error(&mut ui.model.borrow_mut(), enumerate_error.clone());
    if let Some(text) = &enumerate_error {
        // First paint shows the failure while the password prompt is ready;
        // refresh() keeps it up because the model holds it again, until the
        // user deliberately starts another attempt.
        ui.error.set_label(text);
        ui.error.set_visible(true);
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

    // Switching user ends the conversation: PAM's is per user, and a
    // half-answered one for someone else is not reusable.
    //
    // It ends it rather than restarting it. Starting one here would arm PAM for
    // whoever the list happens to land on — including at startup, where
    // `reload_lists`'s `set_model` fires this handler before the user has
    // touched anything. That is how an untouched machine used to lock its first
    // user out. `submit` arms PAM now; this only tears down.
    ui.users.connect_selected_notify({
        let ui = ui.clone();
        move |_| {
            ui.select_preferred_session();
            ui.abandon_attempt();
        }
    });
}

impl Ui {
    /// Rebuild the user and session lists from the model.
    fn reload_lists(&self) {
        // The latch is set *before* the first widget call, not after: `set_model`
        // and `set_selected` below emit notify::selected synchronously, and that
        // handler reaches begin_auth -> begin_attempt -> flush -> refresh, which
        // would read a still-false latch and rebuild both lists a second time,
        // discarding the selection this pass had just established.
        self.lists_loaded.set(true);

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
        let reason = unusable_reason(no_users, no_sessions);
        self.entry.set_sensitive(reason.is_none());
        self.submit.set_sensitive(reason.is_none());
        if let Some(text) = reason {
            // Painted here only so the very first frame — before any revision
            // has reached `refresh` — is not an insensitive form with nothing on
            // it. Keeping it there is `refresh`'s job, not this one's: it
            // re-derives the same reason from the lists on every revision, so
            // writing it into `model.error` here would be both dead (the
            // notify::selected this function's `set_model` fires reaches
            // `begin_attempt`, which clears `error` before anyone reads it) and
            // wrong — `error` belongs to an attempt, not to the machine.
            self.error.set_label(text);
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

    /// End any conversation in flight and drop the form back to "your move".
    ///
    /// Cancelling is what keeps PAM's attempt counter honest across a user
    /// switch: the conversation belongs to the account it was opened for, and
    /// leaving it open would let it time out later against a name the user has
    /// already moved on from.
    fn abandon_attempt(&self) {
        {
            let model = self.model.borrow();
            let Some(greeter) = &model.greeter else {
                return;
            };
            if self.launched.get() {
                return;
            }
            greeter.cancel();
        }

        self.attempting.set(false);
        *self.pending_answer.borrow_mut() = None;
        self.model.borrow_mut().prompt = None;
        self.wait_for_user();
    }

    /// Start a fresh conversation for the selected user.
    ///
    /// The only thing that arms PAM, and reached only from `submit`. Everything
    /// from here to `auth_ok` or `auth_failed` counts as a login attempt as far
    /// as `pam_faillock` is concerned, including the part where wdm is waiting
    /// for an answer — a conversation cannot be ended without failing it. So it
    /// must not be entered on behalf of a user who has not asked to log in.
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
        // Idempotent while an attempt is in flight, which is what makes a second
        // `submit` harmless: between `create_session` and PAM's first question
        // there is no prompt to answer, so another Enter — or a click on the
        // button — takes the "start an attempt" branch again, and without this
        // the greeter would open a second conversation, burning a rate-limit
        // slot and making PAM do every attempt twice. `submit` tests the same
        // flag before it gets here; this is the assertion that `begin_auth`
        // itself is safe to call twice, so that guard is an optimisation and not
        // the only thing standing between a fast typist and a doubled attempt.
        // `abandon_attempt`, the one path that deliberately starts over, clears
        // the flag first.
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
        self.prompt.set_label(WAITING);
        self.error.set_visible(false);
        self.notice.set_visible(false);
    }

    /// Answer the pending prompt, or start an attempt if there is none.
    fn submit(&self) {
        // Nothing sent from here arrives once `Link::pump` has latched dead, and
        // an attempt that cannot be made is worse than none: `begin_attempt`
        // clears the error and the notice, `begin_auth` writes "Waiting…", and
        // no further pump ever changes any of it — a login form with no error,
        // no notice and no instruction, permanently. `wait_for_user` re-asserts
        // what is actually true instead. The default webkit theme guards its own
        // `start()` the same way, for the same reason.
        //
        // Scoped, because `wait_for_user` borrows the model again.
        let link_alive = !self.model.borrow().link_dead;
        if !link_alive {
            self.wait_for_user();
            return;
        }

        let text = self.entry.text().to_string();

        // Only the id is read under the borrow, so `text` is moved into whichever
        // branch consumes it rather than cloned into both. Cloning would leave a
        // second copy of the password in the heap for no reason; the one copy
        // that must exist is discussed at length in wdm's own conversation code.
        let pending_id = {
            let model = self.model.borrow();
            if model.greeter.is_none() {
                return;
            }
            model.prompt.as_ref().map(|prompt| prompt.id)
        };

        if let Some(id) = pending_id {
            {
                let model = self.model.borrow();
                if let Some(greeter) = &model.greeter {
                    greeter.respond(id, text);
                }
            }
            self.entry.set_text("");
            self.model.borrow_mut().prompt = None;
            self.prompt.set_label(CHECKING);
            self.flush();
        } else if let Some(label) = unanswerable_prompt(self.attempting.get(), text.is_empty()) {
            // There is nothing for this Enter to answer, and nothing it should
            // start. Refused with a sentence rather than silently, and every
            // such refusal comes back here: an Enter that does nothing at all
            // reads as a wedged greeter, and the branch that used to fall off
            // the end of this function unhandled — an attempt in flight with no
            // prompt yet — was exactly that.
            self.prompt.set_label(label);
            self.entry.grab_focus();
        } else {
            // No prompt yet, because no conversation is open until right now.
            // Keep what was typed so the prompt PAM is about to send can be
            // answered with it: `begin_auth` clears the entry, and asking the
            // user to type their password a second time because the greeter
            // asked PAM a fraction of a second too late is not a login screen.
            *self.pending_answer.borrow_mut() = Some(text);
            self.begin_auth();
            self.flush();
        }
    }

    /// Push queued requests to the compositor.
    ///
    /// GTK's main loop does not know about our queue, so nothing else would.
    /// When the pump also delivered events, the model has changed and waiting
    /// for the next 16ms tick to show it is a needless frame of staleness — so
    /// refresh here, after every borrow is released.
    ///
    /// That makes flush -> refresh -> flush a cycle. The one branch that closes
    /// it today is the buffered answer: `refresh` spends `pending_answer` on the
    /// prompt that just arrived, sends `respond`, and flushes to get it moving
    /// rather than waiting for the next tick.
    ///
    /// Two things bound that, and neither is `depth`. The buffer is spent with
    /// `take()`, so the branch cannot be entered twice for one answer; and the
    /// branch does not call `begin_attempt`, so it does not bump the revision —
    /// which means `refresh`'s `applied` guard is live on the nested call and
    /// returns immediately unless the pump genuinely delivered something new.
    /// (This is where the old auto-retry differed: it went through
    /// `begin_attempt`, bumped the revision itself, and walked straight past the
    /// guard on every nested entry.)
    ///
    /// `depth` is still load-bearing, because both of those bounds are
    /// properties of the branches that happen to exist rather than of this
    /// function. Any future path that changes the model and flushes — the
    /// auto-retry was exactly that — re-arms the recursion, and a login screen
    /// that overflows its stack is a machine nobody can log into. Past a handful
    /// of nested entries the flush is abandoned and the timer in main.rs picks
    /// the model up on its next tick, 16ms later.
    fn flush(&self) {
        const MAX_DEPTH: u32 = 4;

        let depth = self.depth.get();
        if depth >= MAX_DEPTH {
            log::error!("flush recursion bounded at {MAX_DEPTH}; deferring to the pump timer");
            return;
        }
        self.depth.set(depth + 1);

        let changed = {
            let mut model = self.model.borrow_mut();
            self.link.borrow_mut().pump(&mut model)
        };
        if changed {
            self.refresh();
        }

        self.depth.set(depth);
    }

    /// Apply the model to the widgets.
    pub fn refresh(&self) {
        let revision = self.model.borrow().revision;
        if self.applied.get() == revision {
            return;
        }
        self.applied.set(revision);

        // The lists arrive exactly once, in the enumerate phase Link::connect
        // already waited for, so this is a latch rather than a comparison
        // against what the DropDowns currently hold.
        if !self.lists_loaded.get() {
            self.reload_lists();
        }

        // `conversation_over` matters as much as `notice`: refresh() must not
        // relabel the form as waiting while PAM is mid-conversation. PAM can
        // send an explanatory `info`/`error` prompt *during* a live attempt —
        // that is what becomes `notice` — while the secret it is waiting on is
        // still pending. The "wait for the user" branch below would otherwise
        // clear the half-typed answer and pretend the attempt had ended.
        let (prompt, error, notice, authenticated, conversation_over, unusable) = {
            let model = self.model.borrow();
            (
                model.prompt.clone(),
                model.error.clone(),
                // One label, so the pieces PAM sent separately are joined. The
                // per-notice styles are lost here deliberately: this greeter has
                // nowhere to put them, and the notice line already reads as the
                // account's news rather than the attempt's verdict.
                model.notice_text(),
                model.authenticated,
                model.conversation_over,
                unusable_reason(model.users.is_empty(), model.sessions.is_empty()),
            )
        };

        // The attempt's error wins while there is one, because it is the newer
        // news. The fallback is what keeps an unusable machine explained:
        // `begin_attempt` clears `error` because it belongs to an attempt, while
        // an empty user or session list is a fact about the machine that
        // outlives every attempt — so without it the form goes permanently
        // insensitive with nothing on it saying why.
        let error = error.or_else(|| unusable.map(str::to_owned));

        self.error.set_label(error.as_deref().unwrap_or(""));
        self.error.set_visible(error.is_some());

        // The notice is what PAM said about the account rather than about the
        // password, so it stays on screen until the user acts on it.
        self.notice.set_label(notice.as_deref().unwrap_or(""));
        self.notice.set_visible(notice.is_some());

        if let Some(prompt) = &prompt {
            self.attempting.set(true);

            // The answer the user gave before PAM had asked. Spend it on the
            // first question that wants one and show nothing — from the user's
            // side they typed a password, pressed Enter, and it was checked.
            //
            // `take()` rather than a peek: it is worth exactly one prompt. A
            // stack that asks twice must get the second answer from the user,
            // and a password silently resubmitted to a second question would be
            // the same secret sent somewhere it was never typed for.
            //
            // Which is enforced, not merely intended: see [`spend_buffered`].
            // The buffer is emptied either way and only ever answers a masked
            // question, because it was typed into an entry this greeter builds
            // masked and an echo-on prompt is a different question.
            let buffered = spend_buffered(self.pending_answer.borrow_mut().take(), prompt.secret);
            if let Some(answer) = buffered {
                {
                    let model = self.model.borrow();
                    if let Some(greeter) = &model.greeter {
                        greeter.respond(prompt.id, answer);
                    }
                }
                self.model.borrow_mut().prompt = None;
                self.entry.set_text("");
                self.prompt.set_label(CHECKING);
                self.flush();
                return;
            }

            self.prompt.set_label(&prompt.text);
            // Whatever is already in the entry was typed while it was masked,
            // so it is a password. Unmasking it for an echo-on question would
            // put it on screen — the same hazard `spend_buffered` refuses the
            // buffer for, one field along.
            if !prompt.secret {
                self.entry.set_text("");
            }
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

        // Every ended conversation lands the same way: back at the user, with
        // the form live and the verdict on screen.
        //
        // This used to restart the attempt by itself whenever PAM had not
        // explained its refusal, which read as "a mistyped password, so put the
        // prompt back up". The flaw was that a timeout is indistinguishable from
        // a mistyped password at this level, and restarting re-armed the timeout
        // — one `pam_faillock` entry per turn, until the account locked, with
        // nobody at the keyboard. Now nothing but `submit` opens a conversation,
        // so a wrong password costs the user one keystroke more and an
        // unattended greeter costs them nothing.
        //
        // Guarded on `attempting` rather than on the model, so it fires once on
        // the transition. Running it on every later revision would clear the
        // entry under someone already typing their next attempt.
        if conversation_over && !authenticated && self.attempting.get() {
            self.wait_for_user();
        }
    }

    /// Drop the form back to "your move": no attempt in flight, the entry live
    /// again, and a prompt that says what to do next.
    ///
    /// The one description of that state, shared by everything that arrives at
    /// it, so they cannot drift apart: `build()` on first paint, since nothing
    /// opens a conversation on the user's behalf any more; `abandon_attempt()`
    /// when the user switches account; `submit()` when the link is dead and the
    /// attempt cannot be made at all; `refresh()` when a conversation has ended
    /// without authenticating; and `launch()`'s no-session fallback.
    ///
    /// First paint reaching here is why [`wait_prompt`] takes `attempted`: the
    /// idle state and the state after a refusal are the same widgets in the same
    /// configuration, and the only thing that differs is whether there is
    /// anything to try *again*.
    ///
    /// What that prompt says is [`wait_prompt`]'s decision — one of the states
    /// that reaches here is a lost connection, and there is nothing to try
    /// against after that.
    ///
    /// Sensitivity is never re-enabled here: only `reload_lists` turns the form
    /// off, and it does so because there is nothing on this machine to log into.
    /// Re-enabling would hand the user an entry that cannot lead anywhere —
    /// which is exactly the state the no-session fallback is in.
    ///
    /// It is turned *off* here for the one state that is terminal rather than
    /// merely idle. A lost link reaches this through `refresh`'s
    /// end-of-conversation branch — `link_lost` pushes a notice and sets
    /// `conversation_over` — or through `submit`'s dead-link guard, whichever
    /// the user hits first, so this is where the transition is observed on every
    /// path into it, and an entry the user can
    /// still type into is an invitation to an attempt `submit` will only refuse.
    /// The default webkit theme disables the same controls in `giveUp`.
    fn wait_for_user(&self) {
        // Scoped, and released before any widget call: every path into a widget
        // from here can re-enter and borrow the model again.
        //
        // `conversation_over` is what separates the first paint from a retry:
        // it is false until an attempt has actually ended, and this is now
        // reached on startup as well as after a failure.
        let (link_alive, attempted) = {
            let model = self.model.borrow();
            (!model.link_dead, model.conversation_over)
        };

        self.attempting.set(false);
        // The conversation it was buffered for is over, whatever the verdict. It
        // must not survive into the next one, where it would be spent on a
        // prompt the user never saw.
        *self.pending_answer.borrow_mut() = None;
        self.prompt.set_label(wait_prompt(link_alive, attempted));
        self.entry.set_text("");
        // Back to masked. A previous attempt may have ended on a prompt PAM said
        // was echoable — a username, a token — and leaving the entry unmasked
        // would put the next password on screen in plaintext.
        self.entry.set_visibility(false);
        self.entry.set_input_purpose(InputPurpose::Password);
        if !link_alive {
            self.entry.set_sensitive(false);
            self.submit.set_sensitive(false);
            return;
        }

        // Focus the entry, because nothing else will.
        //
        // `refresh` grabs focus when a prompt arrives, and there used to always
        // be a prompt: the greeter opened a conversation before the user
        // arrived. Now that nothing arms PAM until they ask, this is the state
        // the login screen actually sits in, and without the grab the first
        // keystroke goes to whichever widget GTK focused first — the user
        // drop-down, where Return opens the list instead of logging in.
        self.entry.grab_focus();
    }

    fn launch(&self) {
        let model = self.model.borrow();
        let Some(greeter) = &model.greeter else {
            return;
        };
        let Some(session) = model.session_id(self.sessions.selected() as usize) else {
            // No request to send, and nothing will change on the next pump —
            // leaving `authenticated` true would make refresh() re-enter this
            // forever. Say why instead of pretending the attempt is still in
            // flight, and drop back to the form.
            //
            // The message goes into the *model*, not straight onto the label:
            // refresh() re-derives that label from `model.error` on every
            // revision change, so a widget-only message is wiped by the next
            // event and the user's correct password looks ignored. Wording
            // matches wdm-greeter and the default webkit theme.
            drop(model);
            log::error!("authenticated but no session is selected");
            // One binding for both writes: the helper exists so this wording
            // cannot drift from the other two places, and spelling the literal
            // twice here would be the drift it was extracted to prevent.
            let reason = unusable_reason(false, true)
                .expect("no_sessions is true, so there is always a reason");
            {
                let mut m = self.model.borrow_mut();
                m.error = Some(reason.to_owned());
                m.authenticated = false;
            }
            self.error.set_label(reason);
            self.error.set_visible(true);
            self.wait_for_user();
            return;
        };

        log::info!("starting session {session}");
        self.launched.set(true);
        // No extra environment: anything this greeter could add, wdm already
        // sets more authoritatively, and it filters the rest.
        greeter.start_session(session.to_owned(), Vec::new());

        drop(model);
        self.prompt.set_label(STARTING);
        self.entry.set_sensitive(false);
        self.submit.set_sensitive(false);
        self.flush();
    }
}

fn refs(items: &[String]) -> Vec<&str> {
    items.iter().map(String::as_str).collect()
}

/// The prompt line while an attempt is in flight and PAM has asked nothing yet.
///
/// Named rather than inlined for the same reason [`unusable_reason`] is: the
/// default webkit theme says it too and cannot share the binding, so
/// `the_other_greeters_spell_these_messages_the_same_way` compares the two — and
/// a comparison against a literal spelled out in the test only catches the
/// theme. Three call sites here, which is also three chances to drift.
const WAITING: &str = "Waiting…";

/// The prompt line while an answer is with PAM and its verdict is not back.
const CHECKING: &str = "Checking…";

/// The prompt line once `start_session` has gone and this greeter is on its way
/// out.
const STARTING: &str = "Starting session…";

/// Why this machine has nothing to log into, if it has nothing.
///
/// A free function so the choice of message can be checked without a display,
/// and so the three places that need it — `reload_lists`, which turns the form
/// off; `refresh`, which re-derives the label every revision; and `launch`'s
/// no-session fallback — cannot disagree about the wording. Missing users are
/// reported ahead of missing sessions because a machine with neither is first
/// of all a machine with nobody to be.
///
/// wdm-greeter and the default webkit theme say the same two things in their
/// own languages, and cannot share this constant across the crate and language
/// boundary. Changing a string here therefore means changing
/// `crates/wdm-greeter/src/main.rs` and
/// `crates/wdm-webkit-greeter/themes/default/theme.js` too — which is checked by
/// `the_other_greeters_spell_these_messages_the_same_way`, not left to hand.
fn unusable_reason(no_users: bool, no_sessions: bool) -> Option<&'static str> {
    if no_users {
        Some("No users available to log in")
    } else if no_sessions {
        Some("No sessions installed")
    } else {
        None
    }
}

/// What a buffered answer may be spent on: a masked prompt, and nothing else.
///
/// The buffer holds what was typed into an entry this greeter builds masked and
/// keeps masked between attempts, so it is a password. wdm forwards
/// `PAM_PROMPT_ECHO_ON` as a non-secret prompt, and a stack whose first
/// answerable question is echo-on — pam_oath's token, a username re-prompt —
/// would otherwise be handed that password as the answer to a question it was
/// never typed for, where the stack's own failure logging records it in the
/// clear. So an echo-on prompt gets nothing and is put on screen for the user
/// to answer, and the buffer is dropped rather than held for a later question:
/// it was typed for the one that did not arrive.
fn spend_buffered(buffered: Option<String>, secret: bool) -> Option<String> {
    buffered.filter(|_| secret)
}

/// What to say to an Enter that has nothing to answer, if it has nothing.
///
/// `None` means the press is a real request to log in and `submit` should open
/// a conversation. The two refusals, and why each is a refusal rather than an
/// attempt:
///
/// - An attempt is already in flight and PAM has not asked anything yet. The
///   protocol warns that a greeter must not assume a prompt arrives promptly —
///   wdm answers a rate-limited `create_session` late rather than refusing it,
///   so this window can last a whole cooldown — and a second `begin_auth`
///   inside it is refused by `attempting` anyway. Saying "Waiting…" again is
///   what makes that visible; this branch used to fall off the end of `submit`
///   with no arm at all, which is the silent Enter the comment there disclaims.
/// - Nothing is in flight and the field is empty. Opening a conversation would
///   run the whole PAM stack against an empty password, fail, and charge
///   `pam_faillock` for it, so three stray presses of Enter would lock the
///   account. Someone brushing the keyboard is not a failed login.
///
/// Only the *first* prompt is guarded on emptiness. Once a conversation is
/// underway an empty answer is a real choice — a stack that asks for an
/// optional token accepts one — and `submit` answers it before asking here.
fn unanswerable_prompt(attempting: bool, empty: bool) -> Option<&'static str> {
    if attempting {
        Some(WAITING)
    } else if empty {
        Some("Enter your password")
    } else {
        None
    }
}

/// What to tell the user when an attempt has ended and none has replaced it.
///
/// "Try again" is offered only while there is something to try against: after
/// `Link::pump` latches dead, Enter would send nothing and change nothing, and
/// the user would be pressing a key at a screen that cannot answer — which is
/// why `submit` refuses outright rather than starting an attempt that erases
/// this line. Wording matches the default webkit theme, and that it still does
/// is checked by `the_other_greeters_spell_these_messages_the_same_way`.
fn wait_prompt(link_alive: bool, attempted: bool) -> &'static str {
    match (link_alive, attempted) {
        (false, _) => "Connection to wdm lost — switch to a text console",
        // The first paint reaches here too, now that nothing opens a
        // conversation on the user's behalf: there is no PAM prompt to display
        // because PAM has not been asked anything yet. "Try again" would be a
        // lie on a login screen nobody has touched.
        (true, false) => "Password",
        (true, true) => "Press Enter to try again",
    }
}

/// Put the enumerate-phase `last_error` back after the first attempt cleared it.
///
/// `begin_attempt` rightly wipes `error` when the *user* starts over — they
/// have read the message and chosen to move on. Anything `build()` reaches on
/// its own is not that: nobody has read anything yet, so the compositor's
/// account of why the last session ended must survive it. A message the model
/// has since replaced wins over the snapshot, because it is newer.
fn restore_enumerate_error(model: &mut Model, snapshot: Option<String>) {
    if model.error.is_none() {
        model.error = snapshot;
    }
}

// What can be tested without a display is the decision logic above; the
// widget-bound half — that the label is painted, that notify::selected
// re-enters begin_auth — needs a GTK display and is exercised only by running
// the greeter against wdm's winit backend.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enumerate_phase_error_survives_the_first_paint() {
        // The defect this guards: build() ran reload_lists() and begin_auth(),
        // and begin_auth ends in begin_attempt(), which sets error = None — so
        // a user whose session crashed was bounced back to the prompt with no
        // explanation of why.
        //
        // build() no longer calls begin_auth at all — nothing arms PAM until the
        // user submits — but the snapshot is still load-bearing: reload_lists
        // fires notify::selected, and any path from there that reaches
        // begin_attempt clears `error` just the same.
        //
        // This replays build()'s snapshot -> begin_attempt -> restore sequence
        // by hand, because reload_lists needs a display and so build() itself
        // cannot run here. What it pins is that begin_attempt still clears
        // `error` (the reason the snapshot exists at all) and that
        // restore_enumerate_error puts it back; the ordering in build() is
        // pinned only by the comment there.
        let mut model = Model::default();
        model.error = Some("session ended: sway exited with status 1".to_owned());

        let snapshot = model.error.clone();
        // What begin_auth does on the way to the first conversation.
        model.begin_attempt();
        assert!(
            model.error.is_none(),
            "begin_attempt no longer clears error"
        );

        restore_enumerate_error(&mut model, snapshot);

        // First paint must have visible text, not None.
        assert_eq!(
            model.error.as_deref(),
            Some("session ended: sway exited with status 1")
        );
    }

    #[test]
    fn a_fresher_failure_is_not_overwritten_by_the_snapshot() {
        // If a conversation already failed between the snapshot and the
        // restore, its verdict is newer than the enumerate phase's and wins.
        let mut model = Model::default();
        model.error = Some("Authentication failure".to_owned());

        restore_enumerate_error(&mut model, Some("session ended: crashed".to_owned()));

        assert_eq!(model.error.as_deref(), Some("Authentication failure"));
    }

    #[test]
    fn a_buffered_password_is_only_spent_on_a_masked_prompt() {
        // The defect this guards: `refresh` took the buffer for any prompt at
        // all. The entry is masked while it is typed into, so the buffer is a
        // password; an echo-on first question — pam_oath, a username re-prompt
        // — was answered with it, putting the password into the PAM stack's
        // failure logging in the clear.
        assert_eq!(
            spend_buffered(Some("hunter2".to_owned()), true).as_deref(),
            Some("hunter2"),
            "the masked prompt is the one the buffer exists for"
        );
        // Dropped rather than kept: it was typed for the question that did not
        // arrive, so the next one must be answered by the user. `refresh` then
        // falls through and displays the prompt, which is the widget half and
        // needs a display to observe.
        assert_eq!(spend_buffered(Some("hunter2".to_owned()), false), None);
        assert_eq!(spend_buffered(None, true), None);
    }

    #[test]
    fn an_unusable_machine_says_which_half_is_missing() {
        assert_eq!(
            unusable_reason(true, false),
            Some("No users available to log in")
        );
        assert_eq!(unusable_reason(false, true), Some("No sessions installed"));
        // Neither: the missing users are the more fundamental of the two.
        assert_eq!(
            unusable_reason(true, true),
            Some("No users available to log in")
        );
        assert_eq!(unusable_reason(false, false), None);
    }

    #[test]
    fn an_attempts_verdict_wins_over_the_machines() {
        // Both are true at once on a machine with no sessions whose PAM stack
        // still refused the password. What the user just did is the newer news.
        let error = Some("Authentication failure".to_owned());
        let unusable = Some("No sessions installed");
        assert_eq!(
            error.or_else(|| unusable.map(str::to_owned)).as_deref(),
            Some("Authentication failure")
        );
    }

    #[test]
    fn the_unusable_reason_outlives_an_attempt() {
        // The regression this guards: `begin_attempt` clears `error` because it
        // belongs to an attempt, and refresh() re-derives the label from
        // `error` on every revision — so without the fallback the next event
        // left a permanently insensitive form with nothing on it to say why.
        // Delete the `or_else` fallback in `refresh` and this is the test that
        // fails.
        let mut model = Model::default();
        model.error = Some(unusable_reason(true, true).unwrap().to_owned());
        model.begin_attempt();
        assert!(
            model.error.is_none(),
            "an attempt clears `error` as it must"
        );

        let unusable = unusable_reason(model.users.is_empty(), model.sessions.is_empty());
        assert_eq!(
            model
                .error
                .clone()
                .or_else(|| unusable.map(str::to_owned))
                .as_deref(),
            Some("No users available to log in")
        );
    }

    #[test]
    fn a_usable_machine_between_attempts_says_nothing() {
        let error: Option<String> = None;
        let unusable: Option<&'static str> = None;
        assert_eq!(error.or_else(|| unusable.map(str::to_owned)), None);
    }

    #[test]
    fn an_enter_with_nothing_to_answer_always_says_something() {
        // The defect this guards: `submit`'s no-prompt arm was `else if
        // !attempting`, so an Enter pressed while a `create_session` was
        // outstanding and no prompt had arrived matched no branch at all. That
        // window is not brief — wdm answers a rate-limited create_session late
        // rather than refusing it, so it can last a whole cooldown — and an
        // Enter that does nothing at all is the wedged-greeter reading the
        // sentence in `submit` exists to prevent.
        assert_eq!(unanswerable_prompt(true, true), Some("Waiting…"));
        assert_eq!(
            unanswerable_prompt(true, false),
            Some("Waiting…"),
            "an attempt in flight has nothing to answer whatever was typed"
        );

        // Nothing in flight: an empty field must not cost a PAM attempt, and
        // anything else is a real request to log in.
        assert_eq!(
            unanswerable_prompt(false, true),
            Some("Enter your password")
        );
        assert_eq!(unanswerable_prompt(false, false), None);

        // The label it re-asserts is the one `begin_auth` put up, not a fourth
        // wording of the same state.
        assert_eq!(unanswerable_prompt(true, true), Some(WAITING));
    }

    #[test]
    fn a_dead_link_does_not_invite_a_retry() {
        // Driven off a real model rather than a bare bool, so the branch is
        // pinned to what `Link::pump` actually latches: once it is dead it
        // stays dead, and Enter would send into a socket nobody reads.
        let mut m = Model::default();
        assert_eq!(
            wait_prompt(!m.link_dead, m.conversation_over),
            "Password",
            "a login screen nobody has touched must not offer to try again"
        );
        m.conversation_over = true;
        assert_eq!(
            wait_prompt(!m.link_dead, m.conversation_over),
            "Press Enter to try again"
        );

        m.link_lost("broken pipe");
        assert_eq!(
            wait_prompt(!m.link_dead, m.conversation_over),
            "Connection to wdm lost — switch to a text console"
        );
    }

    #[test]
    fn a_dead_link_keeps_its_explanation_because_no_attempt_starts() {
        // What `submit`'s guard is for. `link_lost` leaves the only text on the
        // screen that explains the silence, and it lives in `notices` — which
        // `begin_attempt` clears, because notices belong to an attempt. Since
        // `Link::pump` never reports another change, whatever the wipe leaves
        // behind is what the user stares at for good.
        //
        // Two models rather than one, because the guarded path is the *absence*
        // of the call: the first is the form as `submit` leaves it, the second
        // is the form as it looked before the guard existed.
        let mut guarded = Model::default();
        guarded.link_lost("broken pipe");
        assert!(
            guarded
                .notice_text()
                .is_some_and(|t| t.contains("Lost the connection to wdm")),
            "link_lost no longer explains itself"
        );
        // The guard's whole action: re-assert the wording and stop.
        assert_eq!(
            wait_prompt(!guarded.link_dead, guarded.conversation_over),
            "Connection to wdm lost — switch to a text console"
        );

        let mut unguarded = Model::default();
        unguarded.link_lost("broken pipe");
        unguarded.begin_attempt();
        assert!(
            unguarded.notice_text().is_none() && unguarded.error.is_none(),
            "begin_attempt no longer erases the explanation, so the guard is \
             pinning a hazard that has moved"
        );
    }

    #[test]
    fn the_other_greeters_spell_these_messages_the_same_way() {
        // A drift check across a crate and a language boundary, on the model of
        // wdm-webkit-greeter's `the_default_theme_uses_the_api_it_is_shipped_with`.
        // The reference greeter paints these in Rust of its own and the default
        // theme writes them in JavaScript, so neither can share the constants
        // above; what they can do is fail the build when one of them is changed
        // alone. Matched on the message text only — the code around it in either
        // file is free to change.
        //
        // Everything the three greeters say to the user in words of their own
        // belongs here: the two `unusable_reason` values, all four states of
        // `wait_prompt`, the empty-submit refusal, and the three transient
        // lines. A message that is not in this test is a message the default
        // theme can be left behind on, and the way that is discovered is a user
        // reading two different sentences for the same condition on two
        // machines.
        let reference = include_str!("../../wdm-greeter/src/main.rs");
        let reference_ui = include_str!("../../wdm-greeter/src/ui.rs");
        let theme = include_str!("../../wdm-webkit-greeter/themes/default/theme.js");

        let no_users = unusable_reason(true, false).unwrap();
        let no_sessions = unusable_reason(false, true).unwrap();
        // The empty-submit refusal, which the reference greeter writes into
        // `self.error` and this greeter writes onto the prompt label.
        let empty_submit = unanswerable_prompt(false, true).unwrap();

        // What all three say, matched in the reference greeter at the *call
        // site* rather than anywhere in the file. It paints "No sessions
        // installed" from two places, and a bare `contains` was satisfied by
        // the enumerate-phase one while the other — authenticated with nothing
        // to launch, the one a user actually hits — read "no session to start"
        // for as long as it liked. Each needle is built from the sentence
        // itself, so rewording it here still fails rather than quietly
        // matching a line that no longer exists.
        for (shared, call_site) in [
            (
                no_users,
                format!(r#"ui::paint_message(canvas, "{no_users}", true)"#),
            ),
            (
                no_sessions,
                format!(r#"ui::paint_message(canvas, "{no_sessions}", true)"#),
            ),
            (
                no_sessions,
                format!(r#"self.error = Some("{no_sessions}".to_owned())"#),
            ),
            (
                empty_submit,
                format!(r#"self.error = Some("{empty_submit}".to_owned())"#),
            ),
        ] {
            assert!(
                reference.contains(&call_site),
                "wdm-greeter no longer says {shared:?} at `{call_site}`"
            );
            assert!(
                theme.contains(shared),
                "the default theme no longer says {shared:?}"
            );
        }

        // The transient lines: what the screen says while the greeter is busy
        // and the user is not. All three are in the theme; the reference
        // greeter paints two of them from its own `ui.rs` — it has no
        // "Checking…" state, because it puts PAM's prompt up verbatim and falls
        // straight back to "Waiting…" when there is none.
        for (line, in_reference) in [(WAITING, true), (CHECKING, false), (STARTING, true)] {
            assert!(
                theme.contains(line),
                "the default theme no longer says {line:?}"
            );
            assert_eq!(
                reference_ui.contains(line),
                in_reference,
                "wdm-greeter's use of {line:?} changed"
            );
        }

        // Only the theme is checked for the rest of the idle-state wording: of
        // these four the reference greeter has a line of its own for exactly
        // one, "Press Enter to try again", which it gained when its automatic
        // retry was removed. It shows PAM's prompt verbatim otherwise, and it
        // has no wording for a lost link at all — it ends when its dispatch
        // does. So this is a two-way check where the loop above is three-way,
        // and that is a fact about the reference greeter rather than an
        // oversight.
        //
        // Every arm, not just the terminal one: `wait_prompt` gained "Password"
        // when the greeter stopped opening a conversation before the user asked,
        // and that arm is what the login screen sits in for as long as nobody
        // touches the machine — the most-seen string in the whole greeter, and
        // the one that was unchecked.
        for (link_alive, attempted) in [(false, false), (false, true), (true, false), (true, true)]
        {
            let line = wait_prompt(link_alive, attempted);
            assert!(
                theme.contains(line),
                "the default theme no longer says {line:?}"
            );
        }
        assert!(
            reference.contains(&format!(r#""{}""#, wait_prompt(true, true))),
            "wdm-greeter no longer offers the retry its `idle_prompt` was added for"
        );
    }
}
