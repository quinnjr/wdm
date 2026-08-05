// Every decision this theme makes, as a pure function.
//
// Nothing here imports React, touches the DOM, or calls window.wdm. That is
// the point: the rules a greeter theme has to get right are the ones that
// lock accounts out when they are got wrong, and in the other two shipped
// themes they can only be checked by grepping the source for the shape of the
// guard. Here they are executed — see machine.test.js, which runs under
// `node --test` and is the only place in this repository where a theme's
// logic is tested rather than pattern-matched.
//
// The reducer returns effects rather than performing them. `wdm.authenticate`
// and friends are side effects on a socket, and a reducer that called them
// could not be run in a test without a compositor on the other end.

/** The most messages of one severity kept on screen. */
export const MAX_MESSAGES = 6;

/**
 * @typedef {"unusable" | "idle" | "waiting" | "prompting" | "checking"
 *           | "starting" | "gaveUp"} Phase
 *
 * - `unusable` — nothing to log into, or nobody to log in as.
 * - `idle`     — the form is armed and PAM is *not* running. This is where the
 *                screen sits when nobody is at it.
 * - `waiting`  — authenticate() sent, no prompt yet.
 * - `prompting`— PAM asked something and is waiting for the answer.
 * - `checking` — an answer is in flight.
 * - `starting` — authenticated; the session is being started.
 * - `gaveUp`   — the link to wdm is gone. Terminal, and latched.
 */

/**
 * The session to preselect: the user's own history, then the machine's
 * configured default, then whatever is first — each checked against the
 * sessions actually installed, because a recorded id can name one that has
 * been uninstalled since and selecting it would leave the dropdown blank.
 */
export const preferredSession = (users, sessions, username, defaultSession) => {
  if (sessions.length === 0) {
    return "";
  }
  const user = users.find((u) => u.name === username);
  const installed = (id) => sessions.some((s) => s.id === id);
  const wanted = [user && user.last_session, defaultSession].find(
    (id) => id && installed(id),
  );
  return wanted || sessions[0].id;
};

/**
 * Adds a message, keeping the first and dropping the second when full.
 *
 * The first is the one carrying the lockout reason and there is no scrolling
 * on this screen by design, so the oldest news the user still needs stays put
 * while the newest arrives.
 */
const withMessage = (list, text) => {
  if (!text) {
    return list;
  }
  const next = [...list, text];
  return next.length > MAX_MESSAGES
    ? [next[0], ...next.slice(next.length - (MAX_MESSAGES - 1))]
    : next;
};

export const initialState = ({ users, sessions, defaultSession }) => {
  const username = users.length > 0 ? users[0].name : "";
  return {
    users,
    sessions,
    defaultSession,
    username,
    sessionId: preferredSession(users, sessions, username, defaultSession),
    // Not `idle` when there is nothing to log into: the form must be inert
    // rather than merely unhelpful, or Enter still costs a login attempt.
    phase: users.length === 0 || sessions.length === 0 ? "unusable" : "idle",
    promptText: "Password",
    promptSecret: true,
    errors:
      users.length === 0
        ? ["No users available to log in"]
        : sessions.length === 0
          ? ["No sessions installed"]
          : [],
    infos: [],
    // The answer typed before PAM had asked for it. Held across authenticate()
    // and spent on the first prompt that wants one, so the user types their
    // password once even though nothing arms PAM until they submit.
    buffered: null,
  };
};

/** Terminal and latched: nothing sent after this reaches anyone. */
const giveUp = (state) => ({
  ...state,
  phase: "gaveUp",
  promptText: "",
  // A password typed just as the link dropped must not sit in the web
  // process — a separate address space from the greeter — until reboot.
  buffered: null,
  errors: state.errors.includes(LINK_DEAD)
    ? state.errors
    : withMessage(state.errors, LINK_DEAD),
});

export const LINK_DEAD = "Connection to wdm lost — switch to a text console";

/**
 * @returns {{state: object, effects: Array<object>}}
 */
export const reduce = (state, event) => {
  const done = (next, ...effects) => ({ state: next, effects });

  // Once the link is gone nothing reaches wdm, so every event is inert. This
  // is checked before anything else precisely so no later branch can send.
  if (state.phase === "gaveUp" && event.type !== "linkDied") {
    return done(state);
  }

  switch (event.type) {
    case "linkDied":
      return done(giveUp(state));

    case "selectUser":
      // Switching user ends the conversation rather than starting one: PAM's
      // is per user, and starting one here would arm PAM for whoever the list
      // happened to land on.
      return done(
        {
          ...state,
          username: event.username,
          sessionId: preferredSession(
            state.users,
            state.sessions,
            event.username,
            state.defaultSession,
          ),
          phase: state.phase === "unusable" ? "unusable" : "idle",
          promptText: "Password",
          promptSecret: true,
          errors: [],
          infos: [],
          buffered: null,
        },
        { type: "cancel" },
      );

    case "selectSession":
      return done({ ...state, sessionId: event.sessionId });

    case "submit": {
      if (state.phase === "unusable") {
        return done(state);
      }

      // With a prompt pending this answers it. An empty answer is legitimate
      // here — a stack that asks something optional is entitled to be
      // answered with nothing.
      if (state.phase === "prompting") {
        return done(
          { ...state, phase: "checking" },
          { type: "respond", answer: event.answer },
        );
      }

      // Otherwise it is the user asking to log in, which is the only thing
      // that opens a conversation. Enter on an empty field is not a login
      // attempt and must not cost one: it would run the whole PAM stack
      // against an empty password, fail, and be charged by pam_faillock, so
      // three stray presses at an unattended screen can lock the account.
      if (state.phase !== "idle") {
        return done(state);
      }
      if (event.answer === "") {
        return done({ ...state, promptText: "Enter your password" });
      }

      return done(
        {
          ...state,
          phase: "waiting",
          promptText: "Waiting…",
          errors: [],
          infos: [],
          buffered: event.answer,
        },
        { type: "authenticate", username: state.username },
      );
    }

    case "prompt": {
      // Idempotent by construction: the greeter may retransmit a callback
      // whose evaluation it could not confirm, and answering the same prompt
      // twice would spend an answer nobody typed.
      const buffered = state.buffered;
      const cleared = { ...state, buffered: null };

      // Spent only on a masked question. It was typed into an input rendered
      // type="password", so it is a password; a stack whose first answerable
      // question is echo-on — pam_oath's token, a username re-prompt — would
      // otherwise be answered with it, and log it in the clear.
      if (buffered !== null && event.secret) {
        return done(
          { ...cleared, phase: "checking", promptText: "Checking…" },
          { type: "respond", answer: buffered },
        );
      }

      return done({
        ...cleared,
        phase: "prompting",
        promptText: event.text,
        promptSecret: event.secret,
      });
    }

    case "message":
      return done(
        event.kind === "error"
          ? { ...state, errors: withMessage(state.errors, event.text) }
          : { ...state, infos: withMessage(state.infos, event.text) },
      );

    case "complete": {
      // The conversation this was buffered for is over, whatever the verdict.
      const cleared = { ...state, buffered: null };

      // Already handing over. The greeter retransmits a callback whose
      // evaluation it could not confirm, and without this a repeat would send
      // a second start_session — which wdm answers by killing the greeter for
      // a protocol violation, on the one screen that cannot report it.
      if (state.phase === "starting") {
        return done(cleared);
      }

      if (event.authenticated) {
        return done(
          { ...cleared, phase: "starting", promptText: "Starting session…" },
          { type: "startSession", sessionId: state.sessionId },
        );
      }

      if (event.linkDead) {
        return done(giveUp(cleared));
      }

      // Deliberately not retrying. Restarting here would clear the message
      // saying why it failed — for a locked account the only thing on screen
      // worth reading — and against pam_faillock each attempt extends the
      // lock. The messages stay; only the phase changes.
      return done({
        ...cleared,
        phase: "idle",
        promptText: "Press Enter to try again",
        promptSecret: true,
      });
    }

    default:
      return done(state);
  }
};
