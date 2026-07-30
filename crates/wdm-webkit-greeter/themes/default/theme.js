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

// The session the selected user last used. wdm reports it; preselecting it is
// the theme's choice, which is the point of putting the policy here.
const selectPreferredSession = () => {
  const user = wdm.users.find((u) => u.name === el("user").value);
  if (user && user.last_session) {
    el("session").value = user.last_session;
  }
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

const start = () => {
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
window.show_message = (text, kind) =>
  show(kind === "error" ? "error" : "message", text);

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
