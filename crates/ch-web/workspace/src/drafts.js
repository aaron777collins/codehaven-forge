// Drafts: unsaved work kept in this browser (spec section 8, Drafts).
//
// - Stored in IndexedDB, encrypted with AES-GCM and a fresh 96-bit nonce per write. Paths,
//   contents, original bytes and branch names are all inside the ciphertext.
// - The associated data binds the schema, the user id, the repository id and the record id, so
//   a record cannot be moved to another person, repository or slot.
// - The key comes only from `boot`, is imported non-extractable and is never stored. After
//   sign-out or lost access, what is stored cannot be read, which matters on a shared computer.
// - Without a secure context there is no WebCrypto: drafts stay in memory and the page says so.
// - One tab edits a repository and branch at a time, by Web Lock.

const DB = "codehaven-workspace";
const STORE = "drafts";
const SCHEMA = "cw-draft-v1";

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => {
      const s = req.result.createObjectStore(STORE, { keyPath: "id" });
      s.createIndex("scope", "scope");
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tx(db, mode, fn) {
  return new Promise((resolve, reject) => {
    const t = db.transaction(STORE, mode);
    const out = fn(t.objectStore(STORE));
    t.oncomplete = () => resolve(out && out.result !== undefined ? out.result : undefined);
    t.onerror = () => reject(t.error);
    t.onabort = () => reject(t.error);
  });
}

function hex(buf) {
  return Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, "0")).join("");
}

function b64bytes(b64) {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

export class Drafts {
  /** `rawKeyB64` is boot's `draft_key`. */
  static async open(rawKeyB64, userLogin, repoId) {
    const d = new Drafts();
    d.repoId = String(repoId);
    d.user = String(userLogin);
    d.memory = new Map();
    d.persistent = false;
    d.reason = "";
    const canStore = window.isSecureContext && window.crypto && crypto.subtle && window.indexedDB;
    if (!canStore) {
      d.reason = "This page is not on HTTPS, so drafts stay in this tab only and are lost when it closes.";
      return d;
    }
    try {
      d.key = await crypto.subtle.importKey("raw", b64bytes(rawKeyB64), { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
      // The scope only groups records. It is a hash, so the store does not name the repository.
      d.scope = hex(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(`${SCHEMA}|${d.user}|${d.repoId}`)));
      d.db = await openDb();
      d.persistent = true;
    } catch {
      d.key = null;
      d.reason = "This browser would not store drafts, so they stay in this tab only.";
    }
    return d;
  }

  aad(id) {
    return new TextEncoder().encode(`${SCHEMA}|${this.user}|${this.repoId}|${id}`);
  }

  /** Saves one draft. `draft.id` is set on first save. */
  async save(draft) {
    if (!draft.id) draft.id = hex(crypto.getRandomValues(new Uint8Array(16)));
    const plain = JSON.stringify(draft);
    if (!this.persistent) {
      this.memory.set(draft.id, plain);
      return;
    }
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const data = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: this.aad(draft.id) }, this.key, new TextEncoder().encode(plain));
    await tx(this.db, "readwrite", (s) => s.put({ id: draft.id, scope: this.scope, iv, data }));
  }

  async remove(id) {
    this.memory.delete(id);
    if (this.persistent) await tx(this.db, "readwrite", (s) => s.delete(id));
  }

  /** Every draft of this person and repository this key can read. Others are skipped. */
  async all() {
    if (!this.persistent) return [...this.memory.values()].map((p) => JSON.parse(p));
    const rows = await new Promise((resolve, reject) => {
      const t = this.db.transaction(STORE, "readonly");
      const req = t.objectStore(STORE).index("scope").getAll(this.scope);
      req.onsuccess = () => resolve(req.result || []);
      req.onerror = () => reject(req.error);
    });
    const out = [];
    for (const r of rows) {
      try {
        const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: r.iv, additionalData: this.aad(r.id) }, this.key, r.data);
        const d = JSON.parse(new TextDecoder().decode(plain));
        if (d.id === r.id) out.push(d);
      } catch {
        // Another key wrote it, or it was moved: it cannot be read here.
      }
    }
    return out;
  }

  async forBranch(branch) {
    return (await this.all()).filter((d) => d.branch === branch);
  }

  /** Forget the key. What is stored stays encrypted and unreadable until the next `boot`. */
  lock() {
    this.key = null;
    this.memory.clear();
    if (this.db) this.db.close();
    this.db = null;
    this.persistent = false;
  }
}

/**
 * Holds the edit lock for this repository and branch while the tab is open. Resolves to
 * `true` when this tab may edit, `false` when another tab already does.
 */
export async function editLock(repoId, branch) {
  if (!navigator.locks) return true;
  const name = `codehaven-workspace:${repoId}:${branch}`;
  return new Promise((resolve) => {
    navigator.locks.request(name, { ifAvailable: true }, (lock) => {
      if (!lock) {
        resolve(false);
        return undefined;
      }
      resolve(true);
      // Held until the tab closes or `releaseEditLocks` runs.
      return new Promise((release) => heldLocks.push(release));
    });
  });
}

const heldLocks = [];

export function releaseEditLocks() {
  while (heldLocks.length) heldLocks.pop()();
}
