// The workspace page (spec section 10): policy, session, layout, editor, views, keys, and the
// two ways the page can stop being usable, a lost session and a hidden tab coming back.

import { readMeta, Session } from "./session.js";
import { h, clear, announce, button } from "./ui.js";
import { buildLayout } from "./layout.js";
import { createEditor } from "./editor.js";
import { mountFiles, mountSearch, quickOpen } from "./tree.js";
import { mountChanges, openCommitDialog } from "./changes.js";
import { mountRuns } from "./runs.js";

const meta = readMeta();
const ASSETS = new URL(meta.assets + "/", location.href).href;

// Only script addresses inside this build's asset folder may load. Nothing else is allowed to
// hand a URL to the page, and nothing from the repository is ever an address.
if (window.trustedTypes && !trustedTypes.defaultPolicy) {
  trustedTypes.createPolicy("default", {
    createScriptURL(u) {
      const a = new URL(u, location.href).href;
      if (!a.startsWith(ASSETS)) throw new TypeError("This script address is not allowed: " + a);
      return a;
    },
  });
}

const phone = matchMedia("(max-width: 639px)").matches
  || (matchMedia("(pointer: coarse)").matches && !matchMedia("(any-pointer: fine)").matches);

const session = new Session(meta);
session.phone = phone;

const root = document.getElementById("ws-root");
const layout = buildLayout(root, session, { phone });
const startNote = document.getElementById("ws-start");
if (startNote) startNote.remove();

// Everything after the layout runs in one function, so a failed start can stop it early.
async function main() {
  let editor = null;
  let searchInput = null;
  let locked = false;
  let terminal = null;

  try {
    await session.start(meta.branch || undefined);
  } catch (e) {
    clear(layout.editorEl).append(h("div", { class: "ws-fatal", role: "alert" },
      h("p", { class: "ws-fatal-text", text: e.code === "network" ? "Codehaven could not be reached." : e.message }),
      e.code === "network" ? button("Retry", () => location.reload(), { icon: "refresh" }) : null));
    return;
  }

  editor = await createEditor(session, layout, { phone, assets: ASSETS });
  layout.setEditor(editor);

  mountFiles(session, layout.views.files, editor);
  searchInput = mountSearch(session, layout.views.search, editor) || null;
  mountChanges(session, layout.views.changes, editor);
  mountRuns(session, layout.runsEl, editor);
  // Nothing of the terminal loads until it is opened: not its module, not the shell.
  let loadingTerminal = null;
  async function toggleTerminal() {
    if (!terminal) {
      loadingTerminal = loadingTerminal || import("./terminal.js").then((m) => m.mountTerminal(session, layout, editor));
      terminal = await loadingTerminal;
    }
    terminal.toggle();
  }
  if (session.boot.terminal) layout.enableTerminal(toggleTerminal);

  if (meta.path && session.paths().includes(meta.path)) editor.openPath(meta.path);

  // ---------------------------------------------------------------- keys

  addEventListener("keydown", (e) => {
    if (locked || !editor) return;
    const mod = e.metaKey || e.ctrlKey;
    const k = (e.key || "").toLowerCase();
    if (mod && !e.shiftKey && !e.altKey && k === "p") {
      e.preventDefault();
      quickOpen(session, editor);
      return;
    }
    if (mod && e.shiftKey && k === "f") {
      e.preventDefault();
      layout.show("search");
      if (searchInput) searchInput.focus();
      return;
    }
    if (mod && !e.shiftKey && !e.altKey && k === "s") {
      e.preventDefault();
      editor.saveActive();
      return;
    }
    if (e.altKey && e.key === "Enter") {
      e.preventDefault();
      openCommitDialog(session, editor);
      return;
    }
    if (e.key === "F1" && !phone) {
      e.preventDefault();
      editor.palette();
      return;
    }
    if (e.ctrlKey && (e.key === "`" || e.code === "Backquote")) {
      e.preventDefault();
      if (session.boot && session.boot.terminal) toggleTerminal();
      else announce("The terminal is off on this server.");
    }
  });

  // ---------------------------------------------------------------- leaving and losing the session

  addEventListener("beforeunload", (e) => {
    if (session.drafts.size || session.dirty || (terminal && terminal.isOpen())) {
      session.flush();
      e.preventDefault();
      e.returnValue = "";
    }
  });

  session.on("lock", () => {
    locked = true;
    if (terminal) terminal.destroy("");
    if (editor) editor.dispose();
    // The keys this tab held are gone from the page too, not only from memory.
    for (const name of ["ws-key", "ws-csrf"]) {
      const m = document.querySelector(`meta[name="${name}"]`);
      if (m) m.remove();
    }
    const next = encodeURIComponent(location.pathname + location.search);
    clear(root).append(h("div", { class: "ws-lock", role: "alert" },
      h("h1", { class: "ws-lock-title", text: "Sign in again" }),
      h("p", { class: "ws-lock-text", text: "Your session ended or this tab is out of date. Your drafts are saved and encrypted in this browser." }),
      h("a", { class: "ws-btn ws-btn-primary", href: "/login?next=" + next }, "Go to sign in")));
  });

  // ---------------------------------------------------------------- coming back to the tab

  let checking = false;
  async function checkAccess() {
    if (locked || checking) return;
    checking = true;
    const cover = h("div", { class: "ws-cover" }, h("p", { class: "ws-cover-text", text: "Checking access..." }));
    root.append(cover);
    try {
      await session.checkHead();
      cover.remove();
    } catch (e) {
      cover.remove();
      if (e.code === "network") {
        const bad = h("div", { class: "ws-cover", role: "alert" },
          h("p", { class: "ws-cover-text", text: "Codehaven could not be reached." }),
          button("Retry", () => { bad.remove(); checkAccess(); }, { icon: "refresh" }));
        root.append(bad);
      }
    } finally {
      checking = false;
    }
  }

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") checkAccess();
  });
  addEventListener("pageshow", (e) => {
    if (e.persisted) checkAccess();
  });
}

main();
