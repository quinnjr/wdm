import { describe, expect, it } from "vitest";
import {
  LINK_DEAD,
  MAX_MESSAGES,
  initialState,
  preferredSession,
  reduce,
} from "./machine.js";

const USERS = [
  { name: "ada", display_name: "Ada", last_session: "sway" },
  { name: "bob", display_name: "Bob", last_session: "" },
];
const SESSIONS = [
  { id: "hyprland", name: "Hyprland" },
  { id: "sway", name: "Sway" },
];

const boot = (over = {}) =>
  initialState({
    users: USERS,
    sessions: SESSIONS,
    defaultSession: "hyprland",
    ...over,
  });

/** Runs a sequence of events, collecting every effect they produced. */
const run = (state, events) => {
  const effects = [];
  for (const event of events) {
    const step = reduce(state, event);
    state = step.state;
    effects.push(...step.effects);
  }
  return { state, effects };
};

const kinds = (effects) => effects.map((e) => e.type);

describe("the rules that stop a lockout", () => {
  // These four are the reason this file exists. Each is a bug this project has
  // actually shipped, and each is invisible to every other kind of check: the
  // greeter cannot enforce them, because a greeter that decided them would be
  // fighting every theme that disagreed.

  it("does not arm PAM before the user asks", () => {
    // A conversation is a login attempt as far as pam_faillock is concerned,
    // for its whole duration, and one sitting on a prompt cannot be ended
    // without failing it. A theme that authenticates at load spends one every
    // time the screen is left alone — which locked an account out in 0.4.0,
    // unattended, in about three minutes.
    const { effects } = run(boot(), [
      { type: "selectUser", username: "bob" },
      { type: "selectSession", sessionId: "sway" },
    ]);
    expect(kinds(effects)).not.toContain("authenticate");
  });

  it("refuses an empty first answer without spending an attempt", () => {
    // Enter on an empty field would otherwise run the whole PAM stack against
    // an empty password, fail, and be charged to the user.
    const { state, effects } = run(boot(), [{ type: "submit", answer: "" }]);
    expect(kinds(effects)).toEqual([]);
    expect(state.phase).toBe("idle");
    expect(state.promptText).toBe("Enter your password");
  });

  it("allows an empty answer once a conversation is underway", () => {
    // The guard is on the *first* answer only: a stack that asks something
    // optional is entitled to be answered with nothing.
    const { state } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "prompt", text: "Token:", secret: false },
    ]);
    expect(state.phase).toBe("prompting");

    const { effects } = run(state, [{ type: "submit", answer: "" }]);
    expect(effects).toEqual([{ type: "respond", answer: "" }]);
  });

  it("never sends the buffered password to an echo-on prompt", () => {
    // The buffer was typed into an input rendered type="password", so it is a
    // password. A stack whose first answerable question is echo-on — pam_oath's
    // token, a username re-prompt — would otherwise be answered with it, and
    // the stack logs echo-on answers in the clear.
    const { state, effects } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "prompt", text: "Token:", secret: false },
    ]);
    expect(kinds(effects)).toEqual(["authenticate"]);
    expect(effects).not.toContainEqual({ type: "respond", answer: "hunter2" });
    // And it is gone rather than held for the next question: it is worth
    // exactly one prompt.
    expect(state.buffered).toBeNull();
    expect(state.promptText).toBe("Token:");
  });

  it("spends the buffered password on a masked prompt", () => {
    const { effects } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "prompt", text: "Password:", secret: true },
    ]);
    expect(effects).toEqual([
      { type: "authenticate", username: "ada" },
      { type: "respond", answer: "hunter2" },
    ]);
  });

  it("does not retry on its own after a failure", () => {
    // Restarting here would clear the message saying why it failed, and
    // against pam_faillock each attempt can extend the lock.
    //
    // Asserted on what the *completion* did, not on the whole run: the submit
    // that set this up authenticates legitimately, so a check over every
    // effect the sequence produced could never fail.
    const failed = run(boot(), [
      { type: "submit", answer: "wrong" },
      { type: "prompt", text: "Password:", secret: true },
      { type: "message", text: "Account locked", kind: "error" },
      { type: "message", text: "10 minutes left", kind: "info" },
    ]);
    const { state, effects } = reduce(failed.state, {
      type: "complete",
      authenticated: false,
      linkDead: false,
    });
    expect(effects).toEqual([]);
    expect(state.phase).toBe("idle");
    // And the explanation survives the verdict — this is the whole reason
    // messages accumulate rather than being assigned.
    expect(state.errors).toContain("Account locked");
    expect(state.infos).toContain("10 minutes left");
  });
});

describe("messages", () => {
  it("keeps the verdict beside the reason rather than in place of it", () => {
    // The verdict arrives through the same callback as everything else, after
    // the messages belonging to the attempt. A theme that assigned would show
    // "Authentication failure" alone, which tells the user nothing they can
    // act on.
    const { state } = run(boot(), [
      { type: "message", text: "Account locked", kind: "error" },
      { type: "message", text: "Authentication failure", kind: "error" },
    ]);
    expect(state.errors).toEqual(["Account locked", "Authentication failure"]);
  });

  it("drops the second, never the first, when full", () => {
    // The first carries the lockout reason and there is no scrolling on this
    // screen by design.
    const events = Array.from({ length: MAX_MESSAGES + 3 }, (_, i) => ({
      type: "message",
      text: `m${i}`,
      kind: "error",
    }));
    const { state } = run(boot(), events);
    expect(state.errors).toHaveLength(MAX_MESSAGES);
    expect(state.errors[0]).toBe("m0");
    expect(state.errors.at(-1)).toBe(`m${MAX_MESSAGES + 2}`);
    expect(state.errors).not.toContain("m1");
  });

  it("splits by severity rather than into one list", () => {
    const { state } = run(boot(), [
      { type: "message", text: "locked", kind: "error" },
      { type: "message", text: "10 minutes", kind: "info" },
    ]);
    expect(state.errors).toEqual(["locked"]);
    expect(state.infos).toEqual(["10 minutes"]);
  });

  it("ignores an empty message rather than showing a blank row", () => {
    const { state } = run(boot(), [
      { type: "message", text: "", kind: "error" },
    ]);
    expect(state.errors).toEqual([]);
  });
});

describe("the link dying", () => {
  it("latches, and sends nothing afterwards", () => {
    // Link errors latch on the greeter side, so nothing sent after this
    // reaches anyone. Retrying would clear the one message explaining the
    // silence and post into a socket nobody reads.
    const { state } = run(boot(), [{ type: "linkDied" }]);
    expect(state.phase).toBe("gaveUp");
    expect(state.errors).toContain(LINK_DEAD);

    const after = run(state, [
      { type: "submit", answer: "hunter2" },
      { type: "selectUser", username: "bob" },
      { type: "prompt", text: "Password:", secret: true },
      { type: "complete", authenticated: true, linkDead: false },
    ]);
    expect(after.effects).toEqual([]);
    expect(after.state.phase).toBe("gaveUp");
  });

  it("does not repeat its notice, and clears a password in flight", () => {
    const { state } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "linkDied" },
      { type: "linkDied" },
    ]);
    expect(state.errors.filter((m) => m === LINK_DEAD)).toHaveLength(1);
    // Typed just as the link dropped, it would otherwise sit in the WebKit web
    // process — a separate address space from the greeter — until reboot.
    expect(state.buffered).toBeNull();
  });

  it("gives up when a conversation ends because the link did", () => {
    const started = run(boot(), [{ type: "submit", answer: "hunter2" }]);
    const { state, effects } = reduce(started.state, {
      type: "complete",
      authenticated: false,
      linkDead: true,
    });
    expect(state.phase).toBe("gaveUp");
    expect(effects).toEqual([]);
    expect(state.errors).toContain(LINK_DEAD);
  });
});

describe("session preselection", () => {
  it("prefers the user's own history", () => {
    expect(preferredSession(USERS, SESSIONS, "ada", "hyprland")).toBe("sway");
  });

  it("falls back to the configured default, then to the first", () => {
    expect(preferredSession(USERS, SESSIONS, "bob", "hyprland")).toBe(
      "hyprland",
    );
    expect(preferredSession(USERS, SESSIONS, "bob", "")).toBe("hyprland");
  });

  it("ignores a recorded session that is no longer installed", () => {
    // History can name one uninstalled since, and selecting a value no option
    // carries leaves the dropdown showing nothing at all.
    const only = [{ id: "hyprland", name: "Hyprland" }];
    expect(preferredSession(USERS, only, "ada", "")).toBe("hyprland");
  });

  it("re-runs when the user changes", () => {
    const { state } = run(boot(), [
      { type: "selectUser", username: "ada" },
    ]);
    expect(state.sessionId).toBe("sway");
  });

  it("cancels the conversation when the user changes", () => {
    // PAM's conversation is per user; a half-answered one for somebody else
    // cannot be reused. It ends the old one rather than starting a new one.
    const { effects } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "selectUser", username: "bob" },
    ]);
    expect(kinds(effects)).toEqual(["authenticate", "cancel"]);
  });
});

describe("an unusable machine", () => {
  it("says so and stays inert with no users", () => {
    const state = boot({ users: [] });
    expect(state.phase).toBe("unusable");
    expect(state.errors).toEqual(["No users available to log in"]);
    expect(run(state, [{ type: "submit", answer: "x" }]).effects).toEqual([]);
  });

  it("says so and stays inert with no sessions", () => {
    const state = boot({ sessions: [] });
    expect(state.phase).toBe("unusable");
    expect(state.errors).toEqual(["No sessions installed"]);
    expect(run(state, [{ type: "submit", answer: "x" }]).effects).toEqual([]);
  });
});

describe("callbacks arriving twice or not at all", () => {
  // The greeter retransmits a callback whose evaluation it could not confirm,
  // and drops one it cannot confirm the page ran. Handlers must tolerate both.

  it("does not answer twice when a prompt is retransmitted", () => {
    const { effects } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "prompt", text: "Password:", secret: true },
      { type: "prompt", text: "Password:", secret: true },
    ]);
    expect(effects.filter((e) => e.type === "respond")).toHaveLength(1);
  });

  it("starts the session once when completion is retransmitted", () => {
    const { effects } = run(boot(), [
      { type: "submit", answer: "hunter2" },
      { type: "prompt", text: "Password:", secret: true },
      { type: "complete", authenticated: true, linkDead: false },
      { type: "complete", authenticated: true, linkDead: false },
    ]);
    expect(effects.filter((e) => e.type === "startSession")).toHaveLength(1);
  });
});

describe("the happy path", () => {
  it("authenticates, answers, and starts the selected session", () => {
    const { state, effects } = run(boot(), [
      { type: "selectSession", sessionId: "sway" },
      { type: "submit", answer: "hunter2" },
      { type: "prompt", text: "Password:", secret: true },
      { type: "complete", authenticated: true, linkDead: false },
    ]);
    expect(effects).toEqual([
      { type: "authenticate", username: "ada" },
      { type: "respond", answer: "hunter2" },
      { type: "startSession", sessionId: "sway" },
    ]);
    expect(state.phase).toBe("starting");
    expect(state.buffered).toBeNull();
  });
});
