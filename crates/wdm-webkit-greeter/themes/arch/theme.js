// The Arch theme's logic.
//
// Everything below the clock is themes/default with different element
// classes. That is deliberate and it is not laziness: the parts of a theme
// that are not appearance are the parts that lock accounts out when they are
// got wrong — when a conversation is opened, whether an empty answer costs an
// attempt, whether a buffered password can reach an echo-on prompt, whether
// the verdict erases the reason it failed. themes/default is the reference for
// all of those and its comments explain why each is shaped the way it is.
// Read it. This file repeats only what it had to change.

const el = (id) => document.getElementById(id);
const form = el("login");

// --- The clock -----------------------------------------------------------

// 24-hour or 12-hour, switched by clicking the clock.
//
// Not read from a config file: the greeter sets
// allow_file_access_from_file_urls(false), so a theme cannot fetch() anything
// beside it — a config would have to be a <script src> that assigns a global,
// which is a heavier contract than one click deserves.
//
// ponytail: this does not survive a greeter restart. The web view is built on
// a NetworkSession::new_ephemeral(), so localStorage is in-memory and dies
// with the process, and the greeter is restarted on every failed login
// generation. The write below is best-effort for that reason, and wrapped
// because a file:// origin is entitled to refuse storage outright rather than
// merely forget it. The upgrade path is a value in wdm.toml surfaced through
// the protocol, which is a protocol change for a preference — the ceiling is
// that the format resets to the default whenever the greeter is respawned.
const CLOCK_KEY = "wdm.arch.clock24";
const DEFAULT_24_HOUR = true;

const readStoredClock = () => {
  try {
    const stored = window.localStorage.getItem(CLOCK_KEY);
    return stored === null ? DEFAULT_24_HOUR : stored === "true";
  } catch {
    return DEFAULT_24_HOUR;
  }
};

const storeClock = (value) => {
  try {
    window.localStorage.setItem(CLOCK_KEY, String(value));
  } catch {
    // Storage refused or unavailable. The toggle still works for as long as
    // this page is up, which is the whole of what the user can see.
  }
};

let clock24 = readStoredClock();

// Rendered from Date rather than toLocaleTimeString with an hour12 option,
// because the greeter's locale is whatever wdm handed it and a login screen
// that shows one user's date format to everyone is worse than one that picks
// a fixed unambiguous one. The date line does use the locale: a month name is
// not ambiguous the way 03/04 is.
const paintClock = () => {
  const now = new Date();
  const hours = now.getHours();
  const minutes = String(now.getMinutes()).padStart(2, "0");

  let display;
  if (clock24) {
    display = `${String(hours).padStart(2, "0")}:${minutes}`;
  } else {
    // 0 and 12 both display as 12; the modulo alone would show "0:05 AM".
    const hour12 = hours % 12 === 0 ? 12 : hours % 12;
    display = `${hour12}:${minutes} ${hours < 12 ? "AM" : "PM"}`;
  }

  el("clock-time").textContent = display;
  el("clock-hint").textContent = clock24 ? "24h" : "12h";
  el("clock-date").textContent = now.toLocaleDateString(undefined, {
    weekday: "long",
    day: "numeric",
    month: "long",
  });
};

el("clock").addEventListener("click", () => {
  clock24 = !clock24;
  storeClock(clock24);
  paintClock();
  // The click moved focus to the clock button. Put it back where a password
  // gets typed — a user who glanced at the time should not have to click the
  // field again, and on a login screen the keyboard is the only input some
  // people have.
  if (!el("answer").disabled) {
    el("answer").focus();
  }
});

paintClock();
// Aligned to the next second rather than started at an arbitrary offset, so
// the minute changes on screen when it changes on the machine.
window.setTimeout(() => {
  paintClock();
  window.setInterval(paintClock, 1000);
}, 1000 - (Date.now() % 1000));

// --- Everything below is themes/default -----------------------------------

for (const user of wdm.users) {
  const option = new Option(user.display_name || user.name, user.name);
  el("user").add(option);
}
for (const session of wdm.sessions) {
  el("session").add(new Option(session.name, session.id));
}

// History → the machine's configured default → whatever is first, each checked
// against the sessions actually installed: a recorded id can name one that has
// been uninstalled since, and assigning a value no <option> carries leaves the
// dropdown blank.
const selectPreferredSession = () => {
  const user = wdm.users.find((u) => u.name === el("user").value);
  const installed = (id) => wdm.sessions.some((s) => s.id === id);
  const wanted = [user && user.last_session, wdm.default_session].find(
    (id) => id && installed(id),
  );
  el("session").value = wanted || wdm.sessions[0].id;
};

const replaceText = (id, text) => {
  const node = el(id);
  node.replaceChildren();
  if (text) {
    node.append(text);
  }
  node.hidden = !text;
};

// Appends, because the greeter calls show_message once per message PAM sent
// and the verdict arrives as one more of them. Assigning would leave the user
// reading "Authentication failure" with the reason erased.
//
// ponytail: as in themes/default, more than six messages of one severity in a
// single conversation lose the second onward, silently. Six per severity, not
// six in total. The screen has no scroll by design, which is what forces a cap
// at all; a scrollable message region removes the need for one.
const MAX_MESSAGES = 6;
const appendText = (id, text, kind) => {
  if (!text) {
    return;
  }
  const node = el(id);
  const line = document.createElement("span");
  line.className = kind === "error" ? "line error" : "line info";
  // childNodes, not children: replaceText writes a bare text node, and the
  // first append into an element it had already written must not run into it.
  line.textContent = (node.childNodes.length ? " " : "") + text;
  node.append(line);
  // children here, deliberately: this removes spans. The first is kept
  // whatever happens — it carries the lockout reason — so it is the second
  // that goes.
  while (node.children.length > MAX_MESSAGES) {
    node.children[1].remove();
  }
  node.hidden = false;
};

const linkDead = () => typeof wdm.link_dead !== "undefined" && wdm.link_dead;

let gaveUp = false;
const giveUp = () => {
  if (!gaveUp) {
    gaveUp = true;
    appendText(
      "error",
      "Connection to wdm lost — switch to a text console",
      "error",
    );
  }
  el("prompt").textContent = "";
  // A password typed just as the link dropped would otherwise sit in a
  // disabled input in the WebKit web process — a separate address space from
  // the greeter — until the machine is rebooted.
  el("answer").value = "";
  pendingAnswer = null;
  for (const id of ["user", "session", "answer"]) {
    el(id).disabled = true;
  }
};

// What the user typed before there was a prompt to put it in. Held across
// authenticate() and spent on the first prompt that wants an answer, so the
// password is typed once even though nothing arms PAM until submit.
let pendingAnswer = null;

const usable = () => {
  if (wdm.users.length === 0 || wdm.sessions.length === 0) {
    pendingAnswer = null;
    replaceText(
      "error",
      wdm.users.length === 0
        ? "No users available to log in"
        : "No sessions installed",
    );
    el("prompt").textContent = "";
    for (const id of ["user", "session", "answer"]) {
      el(id).disabled = true;
    }
    return false;
  }

  // Before any clear, deliberately: an attempt that cannot be made must not
  // wipe the text saying why.
  if (linkDead()) {
    giveUp();
    return false;
  }

  return true;
};

// The form's "your move" state. It must not call authenticate(): every
// conversation is a login attempt as far as pam_faillock is concerned, for its
// whole duration, and one cannot be ended without failing it. A login screen
// that armed PAM on its own used to lock out the first user in the list,
// unattended.
const ready = () => {
  if (!usable()) {
    return;
  }
  replaceText("message", "");
  replaceText("error", "");
  el("answer").value = "";
  el("answer").disabled = false;
  el("prompt").textContent = "Password";
  selectPreferredSession();
  el("answer").focus();
};

// Arms PAM. Reached only from the submit handler.
const start = () => {
  if (!usable()) {
    return;
  }
  replaceText("message", "");
  replaceText("error", "");
  el("prompt").textContent = "Waiting…";
  selectPreferredSession();
  wdm.authenticate(el("user").value);
};

// --- The greeter calls these ---------------------------------------------

window.show_prompt = (text, kind) => {
  // Spent only on a masked question. It was typed into an input rendered as
  // type="password", so it is a password; a stack whose first answerable
  // question is echo-on — pam_oath's token, a username re-prompt — would
  // otherwise be answered with it, and then log it in the clear.
  //
  // Cleared either way and before it is sent: it is worth exactly one prompt.
  const buffered = pendingAnswer;
  pendingAnswer = null;
  if (buffered !== null && kind === "password") {
    el("prompt").textContent = "Checking…";
    el("answer").disabled = true;
    wdm.respond(buffered);
    return;
  }

  el("prompt").textContent = text;
  el("answer").type = kind === "password" ? "password" : "text";
  // Cleared before the type changes, for the same reason the buffer is
  // refused: anything typed while the input was masked is a password, and an
  // echo-on question would put it on screen.
  el("answer").value = "";
  el("answer").disabled = false;
  el("answer").focus();
};

window.show_message = (text, kind) =>
  appendText(kind === "error" ? "error" : "message", text, kind);

window.authentication_complete = () => {
  // The conversation this was buffered for is over, whatever the verdict.
  pendingAnswer = null;

  if (wdm.is_authenticated) {
    el("prompt").textContent = "Starting session…";
    el("answer").disabled = true;
    wdm.start_session(el("session").value);
    return;
  }

  if (linkDead()) {
    giveUp();
    return;
  }

  // Deliberately not retrying on its own: restarting here would clear the
  // message saying why it failed, and against pam_faillock each attempt can
  // extend the lock.
  el("prompt").textContent = "Press Enter to try again";
  el("answer").value = "";
  el("answer").disabled = false;
  el("answer").focus();
};

// --- Input ---------------------------------------------------------------

form.addEventListener("submit", (event) => {
  event.preventDefault();
  if (wdm._prompt) {
    // Read, clear, then send. Disabling an input does not empty it, and an
    // answer left in the DOM sits in the WebKit web process for as long as the
    // page is up.
    const answer = el("answer").value;
    el("answer").value = "";
    el("answer").disabled = true;
    wdm.respond(answer);
  } else {
    // Enter on an empty field is not a login attempt and must not cost one: it
    // would run the whole PAM stack against an empty password, fail, and be
    // charged by pam_faillock. Only the first prompt is guarded — once a
    // conversation is underway an empty answer is a real choice.
    if (el("answer").value === "") {
      el("prompt").textContent = "Enter your password";
      el("answer").focus();
      return;
    }
    pendingAnswer = el("answer").value;
    el("answer").value = "";
    el("answer").disabled = true;
    start();
  }
});

// Switching user ends the current conversation rather than starting a new one:
// PAM's is per user, and starting one here would arm PAM for whoever the list
// happens to land on.
el("user").addEventListener("change", () => {
  wdm.cancel();
  pendingAnswer = null;
  ready();
});

ready();
