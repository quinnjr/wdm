import { useEffect, useRef, useState } from "react";
import { FontAwesomeIcon } from "@fortawesome/react-fontawesome";
import {
  faCircleInfo,
  faDisplay,
  faLock,
  faRightToBracket,
  faTriangleExclamation,
  faUser,
} from "@fortawesome/free-solid-svg-icons";
import { useWdm } from "./useWdm.js";

// Font Awesome as SVG components rather than the webfont themes/arch uses.
//
// The React packages are the right shape here for a reason that is specific to
// this greeter, not just idiom: each icon is imported by name and rendered as
// inline <svg>, so the bundle carries the six paths below and nothing else,
// and the theme installs no font file at all. themes/arch has to ship a
// 119 kB woff2 because a stylesheet can only name a glyph, and a webfont whose
// file is missing renders every icon as a tofu box on a screen with no console.
// There is no such file to lose here.
//
// Explicit named imports, never the string form (`icon="user"`): that requires
// a global icon library registered at startup, which defeats tree-shaking and
// fails at runtime — a blank space where the icon goes — rather than at build
// time if a name is wrong.

const Notice = ({ kind, messages }) =>
  messages.length === 0 ? null : (
    <p
      className={
        kind === "error"
          ? "flex gap-3 rounded-xl border border-red-500/30 bg-red-500/10 px-4 py-3 text-sm leading-relaxed text-red-200"
          : "flex gap-3 rounded-xl border border-sky-500/30 bg-sky-500/10 px-4 py-3 text-sm leading-relaxed text-sky-200"
      }
    >
      {/*
        Decorative: the severity is already carried by the message text and by
        the colour, and a screen reader announcing "warning" before PAM's own
        wording would be reading the theme's opinion rather than the stack's.
      */}
      <FontAwesomeIcon
        icon={kind === "error" ? faTriangleExclamation : faCircleInfo}
        className="mt-1 shrink-0"
        aria-hidden="true"
      />
      <span>
        {/*
          One <span> per message, not one joined string. PAM sends them one at
          a time and routinely splits an explanation in two — "the account is
          locked" and "10 minutes left to unlock" — and each keeps its own
          identity so a stylesheet can tell them apart. Keyed by index because
          the list is append-only and two messages can legitimately be
          identical.
        */}
        {messages.map((text, i) => (
          <span key={i}>
            {i > 0 ? " " : ""}
            {text}
          </span>
        ))}
      </span>
    </p>
  );

const Field = ({ icon, children }) => (
  <label className="mb-1.5 flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-slate-400">
    {icon ? <FontAwesomeIcon icon={icon} className="text-[11px]" aria-hidden="true" /> : null}
    {children}
  </label>
);

const select =
  "w-full appearance-none rounded-xl border border-white/10 bg-slate-950/60 " +
  "px-4 py-3 text-slate-100 transition focus:border-sky-400 focus:outline-none " +
  "focus:ring-2 focus:ring-sky-400/40 disabled:cursor-not-allowed disabled:opacity-40";

export const App = ({ api }) => {
  const { state, send } = useWdm(api);
  const [answer, setAnswer] = useState("");
  const input = useRef(null);

  const busy =
    state.phase === "waiting" ||
    state.phase === "checking" ||
    state.phase === "starting";
  const inert = state.phase === "gaveUp" || state.phase === "unusable";

  // Clear the field whenever the conversation moves on. A controlled input
  // keeps the password in React state, so this is where it stops existing —
  // and it must happen on every phase change rather than only on success,
  // because an answer left behind sits in the WebKit web process, a separate
  // address space from the greeter, for as long as the page is up.
  useEffect(() => {
    setAnswer("");
  }, [state.phase, state.promptText]);

  // Focus follows the phase: back to the field whenever it is the user's move
  // again, so a failed attempt can be retried without reaching for the mouse.
  useEffect(() => {
    if (!busy && !inert && input.current) {
      input.current.focus();
    }
  }, [busy, inert, state.phase]);

  const submit = (event) => {
    event.preventDefault();
    send({ type: "submit", answer });
  };

  return (
    <main className="relative w-full max-w-md px-6">
      <div className="rounded-3xl border border-white/10 bg-slate-900/70 p-8 shadow-2xl shadow-black/50 backdrop-blur-xl">
        <div className="mb-7">
          <h1 className="text-xl font-semibold leading-tight">Sign in</h1>
          <p className="text-xs text-slate-400">
            wdm — React theme
          </p>
        </div>

        <form onSubmit={submit} autoComplete="off" className="space-y-5">
          <div>
            <Field icon={faUser}>User</Field>
            <select
              className={select}
              value={state.username}
              disabled={inert}
              onChange={(e) =>
                send({ type: "selectUser", username: e.target.value })
              }
            >
              {state.users.map((user) => (
                <option key={user.name} value={user.name}>
                  {user.display_name || user.name}
                </option>
              ))}
            </select>
          </div>

          <div>
            {/*
              The label is PAM's own words, so the icon is the only fixed part
              of it — and it tracks whether the answer is masked rather than
              always saying "lock". An echo-on question (pam_oath's token, a
              username re-prompt) is not a password prompt and must not be
              dressed as one.
            */}
            <Field icon={state.promptSecret ? faLock : faCircleInfo}>
              {state.promptText}
            </Field>
            <input
              ref={input}
              className={select}
              // PAM's own answer — wdm forwards PAM_PROMPT_ECHO_ON as a
              // non-secret kind, and an echo-on question answered into a
              // masked field is a question the user cannot see themselves
              // answer. The reducer never lets a password buffered from the
              // masked field reach one of these.
              type={state.promptSecret ? "password" : "text"}
              value={answer}
              disabled={busy || inert}
              onChange={(e) => setAnswer(e.target.value)}
            />
          </div>

          <Notice kind="error" messages={state.errors} />
          <Notice kind="info" messages={state.infos} />

          <div>
            <Field icon={faDisplay}>Session</Field>
            <select
              className={select}
              value={state.sessionId}
              disabled={inert}
              onChange={(e) =>
                send({ type: "selectSession", sessionId: e.target.value })
              }
            >
              {state.sessions.map((session) => (
                <option key={session.id} value={session.id}>
                  {session.name}
                </option>
              ))}
            </select>
          </div>

          <button
            type="submit"
            disabled={busy || inert}
            className="flex w-full items-center justify-center gap-2 rounded-xl bg-sky-500 px-4 py-3 font-medium text-slate-950 transition hover:bg-sky-400 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-400 focus-visible:ring-offset-2 focus-visible:ring-offset-slate-900 disabled:cursor-not-allowed disabled:opacity-40"
          >
            <FontAwesomeIcon icon={faRightToBracket} aria-hidden="true" />
            {state.phase === "starting" ? "Starting session…" : "Log in"}
          </button>
        </form>
      </div>
    </main>
  );
};
