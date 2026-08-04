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

// Two elements, not one, and the split is PAM's own severity: whatever PAM
// called an error goes in "error", whatever it called info goes in "message".
// The verdict — "Authentication failure" — arrives through the same
// show_message with kind "error", so it lands in the error element beside the
// reason rather than in place of it. That is why the error element *appends*:
// "the account is locked, 10 minutes left" must not be erased by the verdict
// that follows it, which on its own says nothing the user can act on.

// Assigns, and hides when there is nothing to say. For the fixed lines a theme
// owns rather than the running commentary PAM sends.
const replaceText = (id, text) => {
  const node = el(id);
  node.replaceChildren();
  if (text) {
    node.append(text);
  }
  node.hidden = !text;
};

// Adds one more message without disturbing the ones already there, because the
// greeter calls show_message once per message PAM sent and PAM routinely splits
// one explanation in two. Assigning here would leave the user reading
// "(10 minutes left to unlock)" with nothing saying what is locked.
//
// A child element per message rather than one concatenated string: each keeps
// its own `kind` so a stylesheet can tell them apart, and — because there is no
// scrolling on this screen by design — the count can be capped without cutting
// a message in half. The first is kept whatever happens; it is the one carrying
// the lockout reason. It is the *second* that is dropped when the line grows
// too long, so the oldest news the user still needs stays put while the newest
// arrives.
//
// ponytail: more than six messages in one conversation lose the second onward,
// silently — a stack running pam_faillock, pam_pwquality, a motd and a
// last-login line can reach that in a single attempt. The screen has no scroll
// by design, which is what forces a cap at all; a scrollable message region
// removes the need for one.
const MAX_MESSAGES = 6;
const appendText = (id, text, kind) => {
  // An empty message is not a message. Without this the element would unhide
  // onto a blank row and push the form down for nothing.
  if (!text) {
    return;
  }
  const node = el(id);
  const line = document.createElement("span");
  line.className = kind === "error" ? "line error" : "line info";
  // A span is inline and style.css gives .line no rules of its own, so without
  // this the messages would run together into one word. The separator lives
  // inside the span rather than beside it so that dropping a span drops its
  // separator with it, leaving no stray text nodes among node.children.
  //
  // childNodes, not children: `children` counts elements only, and replaceText
  // writes a bare text node. The first append into an element replaceText had
  // already written — "No users available to log in", then a lost-link notice
  // on the same element — would otherwise arrive with no space in front of it.
  // The cap below is right to use `children`: it removes spans.
  line.textContent = (node.childNodes.length ? " " : "") + text;
  node.append(line);
  while (node.children.length > MAX_MESSAGES) {
    node.children[1].remove();
  }
  node.hidden = false;
};

// The connection to wdm is gone for good. Link errors latch on the greeter
// side, so nothing sent after this reaches anyone: retrying would clear the one
// message explaining the silence and post into a socket nobody reads. Say what
// to do instead and stop. Read through `typeof` because a wdm that predates the
// field leaves it undefined, and a theme must not die on that.
const linkDead = () => typeof wdm.link_dead !== "undefined" && wdm.link_dead;

// Wording matches the GTK greeter, which reaches the same state. Appended, not
// assigned, and only once: whatever PAM managed to say before the link went is
// still the most useful thing on the screen, and both paths into here can be
// reached more than once.
//
// Named for what it is — terminal and latched, not a pause — to match
// wdm.link_dead, GREETER_GAVE_UP_EXIT and Disposition::GaveUp, which are the
// same condition seen from the other side.
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
  // The one clear start() would have done and this path skips. A password typed
  // just as the link dropped would otherwise sit in a disabled input in the
  // WebKit web process — a separate address space from the greeter — for as
  // long as the screen is up, which here is until the machine is rebooted. The
  // buffered copy is the same hazard with none of the visibility.
  el("answer").value = "";
  pendingAnswer = null;
  for (const id of ["user", "session", "answer"]) {
    el(id).disabled = true;
  }
};

// What the user typed before there was a prompt to put it in.
//
// Nothing here opens a PAM conversation until the user asks to log in, so the
// first thing they type arrives before PAM has asked for it. Held across
// authenticate() and spent on the first prompt that wants an answer.
let pendingAnswer = null;

// Whether this machine can be logged into at all, saying so if it cannot.
//
// Nothing to log into, or nobody to log in: say so and stop, like the other
// greeters do. Calling authenticate("") instead would throw from top level and
// take the rest of this script with it — a blank, dead form.
const usable = () => {
  if (wdm.users.length === 0 || wdm.sessions.length === 0) {
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

// Put the form in its "your move" state, without touching PAM.
//
// This is where the greeter sits on a machine nobody has touched. It must not
// call authenticate(): every conversation counts as a login attempt to
// pam_faillock, including the part where wdm is waiting for an answer, and a
// conversation cannot be ended without failing it. A login screen that armed
// PAM on its own used to lock out the first user in the list, unattended.
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

// Arm PAM. Reached only from the submit handler.
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

// PAM asked something. `kind` is "password" when the answer must be masked.
window.show_prompt = (text, kind) => {
  // The answer the user gave before PAM had asked for it. Spent on the first
  // question that wants one, so from their side they typed a password, pressed
  // Enter, and it was checked.
  //
  // Cleared before it is sent, not after: it is worth exactly one prompt. A
  // stack that asks twice must get the second answer from the user, and
  // silently resending the password would be putting that secret somewhere it
  // was never typed for.
  if (pendingAnswer !== null) {
    const answer = pendingAnswer;
    pendingAnswer = null;
    el("prompt").textContent = "Checking…";
    el("answer").disabled = true;
    wdm.respond(answer);
    return;
  }

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
  appendText(kind === "error" ? "error" : "message", text, kind);

// The conversation ended, either way.
window.authentication_complete = () => {
  // The conversation this was buffered for is over, whatever the verdict. It
  // must not survive into the next one: it would be spent on a prompt the user
  // never saw, and until then it is a password sitting in the WebKit web
  // process — a separate address space from the greeter.
  pendingAnswer = null;

  if (wdm.is_authenticated) {
    el("prompt").textContent = "Starting session…";
    el("answer").disabled = true;
    wdm.start_session(el("session").value);
    return;
  }

  // A conversation can also end because the link did. Inviting a retry then
  // would be an invitation to a blank screen: start() clears the messages and
  // sends into a socket nobody reads.
  if (linkDead()) {
    giveUp();
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
  // With a prompt pending this answers it; otherwise it is the user asking to
  // log in, which is the only thing that opens a conversation.
  if (wdm._prompt) {
    el("answer").disabled = true;
    wdm.respond(el("answer").value);
  } else {
    // Keep what was typed so the prompt PAM is about to send can be answered
    // with it, rather than making the user type their password twice.
    pendingAnswer = el("answer").value;
    el("answer").value = "";
    el("answer").disabled = true;
    start();
  }
});

// Switching user ends the current conversation: PAM's is per user, and a
// half-answered one for somebody else cannot be reused.
//
// It ends it rather than starting a new one. Starting one here would arm PAM
// for whoever the list happens to land on, which is what the submit handler
// above exists to be the only cause of.
el("user").addEventListener("change", () => {
  wdm.cancel();
  pendingAnswer = null;
  ready();
});

ready();
