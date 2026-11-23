// Byte rules for files in the workspace (spec section 8, Bytes).
//
// Changed means different bytes from the base: the editor's own diff is never the authority.
// An untouched file is never rewritten. A byte order mark, a single line-ending style and the
// executable bit are kept. Mixed endings or any lone CR open read only until the person picks
// an ending. Binary, non-UTF-8 and LFS pointer files are read only.

const LFS = "version https://git-lfs.github.com/spec/v1\n";
const BOM = [0xef, 0xbb, 0xbf];

/** What a file's bytes are, and the text to edit when they are editable. */
export function inspect(bytes) {
  const info = { bom: false, eol: "none", text: "", readOnly: false, reason: "", fixable: false };
  const head = bytes.subarray(0, 8000);
  if (head.indexOf(0) !== -1) return { ...info, readOnly: true, reason: "This is a binary file." };
  let body = bytes;
  if (bytes.length >= 3 && bytes[0] === BOM[0] && bytes[1] === BOM[1] && bytes[2] === BOM[2]) {
    info.bom = true;
    body = bytes.subarray(3);
  }
  let text;
  try {
    text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(body);
  } catch {
    return { ...info, readOnly: true, reason: "This file is not UTF-8 text." };
  }
  if (text.startsWith(LFS)) return { ...info, text, readOnly: true, reason: "This is a Git LFS pointer. Edit the file with Git LFS." };
  let crlf = 0, lf = 0, cr = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c === 13) {
      if (text.charCodeAt(i + 1) === 10) { crlf++; i++; } else cr++;
    } else if (c === 10) lf++;
  }
  if (cr > 0 || (crlf > 0 && lf > 0)) {
    return { ...info, text, eol: "mixed", readOnly: true, fixable: true, reason: "This file mixes line endings. Choose one to edit it." };
  }
  info.eol = crlf > 0 ? "crlf" : lf > 0 ? "lf" : "none";
  info.text = crlf > 0 ? text.replaceAll("\r\n", "\n") : text;
  return info;
}

/** The person chose an ending for a mixed file: the text to edit, with that ending on save. */
export function chooseEnding(info, eol) {
  const text = info.text.replaceAll("\r\n", "\n").replaceAll("\r", "\n");
  return { ...info, eol, text, readOnly: false, fixable: false, reason: "" };
}

/** Editor text back to bytes, with the file's own ending and byte order mark. */
export function encode(text, info) {
  let t = text.replaceAll("\r\n", "\n");
  if (info.eol === "crlf") t = t.replaceAll("\n", "\r\n");
  const body = new TextEncoder().encode(t);
  if (!info.bom) return body;
  const out = new Uint8Array(body.length + 3);
  out.set(BOM, 0);
  out.set(body, 3);
  return out;
}

export function sameBytes(a, b) {
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** Standard base64 of bytes, in chunks so a 2 MB file does not overflow the call stack. */
export function toB64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

export function fromB64(b64) {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

/** The longest line in a text, for the long-line warning. */
export function longestLine(text) {
  let max = 0, start = 0;
  for (let i = 0; i <= text.length; i++) {
    if (i === text.length || text.charCodeAt(i) === 10) {
      max = Math.max(max, i - start);
      start = i + 1;
    }
  }
  return max;
}

// Control characters, bidi controls, line and paragraph separators and the BOM.
const UNSAFE = new RegExp("[\\u0000-\\u001f\\u007f-\\u009f\\u061c\\u200e\\u200f\\u202a-\\u202e\\u2066-\\u2069\\u2028\\u2029\\ufeff]", "g");

/** A path as safe display text: control and bidi characters become visible escapes. */
export function displayPath(p) {
  return p.replace(UNSAFE, (c) => "\\u" + c.charCodeAt(0).toString(16).padStart(4, "0"));
}
