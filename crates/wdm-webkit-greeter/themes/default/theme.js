// The default theme's logic, and the reference for what a theme must do.
//
// The greeter calls four globals — show_prompt, show_message,
// authentication_complete and nothing else — and everything a theme asks for
// goes through window.wdm. There is no other channel.

const el = (id) => document.getElementById(id);
const form = el("login");

// wdm.users and wdm.sessions are already populated when this script runs; the
// API object is injected at document-start precisely so this works.
for (const user of wdm.users) {
  const option = new Option(user.display_name || user.name, user.name);
  el("user").add(option);
}
for (const session of wdm.sessions) {
  el("session").add(new Option(session.name, session.id));
}

// The session to preselect: the user's history, then the machine's configured
// default, then whatever is first. That chain is Model::preferred_session in
// wdm-greeter-client, which is the reference implementation the other greeters
// share; this is the same chain written in the theme, because wdm reports the
// facts and choosing between them is policy. Each candidate is checked against
// the installed sessions: history can name one that was uninstalled since, and
// assigning a value no <option> carries leaves the dropdown showing nothing at
// all. Falling through to the first session is that same guarantee — like
// preferred_session's unwrap_or(0), never leave the dropdown blank.
const selectPreferredSession = () => {
  const user = wdm.users.find((u) => u.name === el("user").value);
  const installed = (id) => wdm.sessions.some((s) => s.id === id);
  const wanted = [user && user.last_session, wdm.default_session].find(
    (id) => id && installed(id),
  );
  el("session").value = wanted || wdm.sessions[0].id;
};

// Two elements, not one, and this is the whole reason: "Authentication
// failure" is the verdict, and "the account is locked, 10 minutes left" is the
// explanation. A theme that puts both in the same place shows the user only
// the half that says nothing.
const show = (id, text) => {
  const node = el(id);
  node.textContent = text;
  node.hidden = !text;
};

// Appends rather than replaces, because the greeter calls show_message once per
// message PAM sent and PAM routinely splits one explanation in two. Assigning
// textContent here would leave the user reading "(10 minutes left to unlock)"
// with nothing saying what is locked.
const append = (id, text) => {
  const node = el(id);
  if (node.textContent) {
    node.textContent += " ";
  }
  node.textContent += text;
  node.hidden = false;
};

const start = () => {
  // Nothing to log into, or nobody to log in: say so and stop, like the other
  // greeters do. Calling authenticate("") instead would throw from top level
  // and take the rest of this script with it — a blank, dead form.
  if (wdm.users.length === 0 || wdm.sessions.length === 0) {
    show(
      "error",
      wdm.users.length === 0
        ? "No users available to log in"
        : "No sessions installed",
    );
    el("prompt").textContent = "";
    for (const id of ["user", "session", "answer"]) {
      el(id).disabled = true;
    }
    return;
  }

  show("message", "");
  show("error", "");
  el("answer").value = "";
  el("prompt").textContent = "Waiting…";
  selectPreferredSession();
  wdm.authenticate(el("user").value);
};

// --- The greeter calls these ---------------------------------------------

// PAM asked something. `kind` is "password" when the answer must be masked.
window.show_prompt = (text, kind) => {
  el("prompt").textContent = text;
  el("answer").type = kind === "password" ? "password" : "text";
  el("answer").value = "";
  el("answer").disabled = false;
  el("answer").focus();
};

// PAM said something, without asking. This is where a locked account explains
// itself, so it stays until the user starts another attempt — including past
// the "Authentication failure" that follows it.
//
// One call per message, carrying that message's own style, so the two elements
// split by severity: "the account is locked" (error) above "10 minutes left to
// unlock" (info). The verdict arrives the same way and joins the error line,
// which is why both accumulate — the last thing said must not erase the reason.
window.show_message = (text, kind) =>
  append(kind === "error" ? "error" : "message", text);

// The conversation ended, either way.
window.authentication_complete = () => {
  if (wdm.is_authenticated) {
    el("prompt").textContent = "Starting session…";
    el("answer").disabled = true;
    wdm.start_session(el("session").value);
    return;
  }

  // Deliberately not retrying on its own. Restarting here would clear the
  // message that says *why* it failed — which for a locked account is the only
  // thing on screen worth reading.
  el("prompt").textContent = "Press Enter to try again";
  el("answer").value = "";
  el("answer").disabled = false;
  el("answer").focus();
};

// --- Input ---------------------------------------------------------------

form.addEventListener("submit", (event) => {
  event.preventDefault();
  // With a prompt pending this answers it; otherwise it is the user asking for
  // another attempt after a failure.
  if (wdm._prompt) {
    el("answer").disabled = true;
    wdm.respond(el("answer").value);
  } else {
    start();
  }
});

// Switching user ends the current conversation: PAM's is per user, and a
// half-answered one for somebody else cannot be reused.
el("user").addEventListener("change", start);

start();
