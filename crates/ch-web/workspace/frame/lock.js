// Runs first in the terminal frame, before xterm or any other script (spec section 7, Frame).
// Locks the ways a page reaches the network or the tab without fetch: peer connections, history
// (which in WebKit could send the whole tab back), popups and execCommand. Each is replaced by a
// value that cannot be written or redefined afterwards.
(() => {
  "use strict";
  const lock = (o, k, v) => {
    try {
      Object.defineProperty(o, k, { value: v, writable: false, configurable: false, enumerable: false });
    } catch (e) {
      // Already locked or not present in this engine.
    }
  };
  for (const k of ["RTCPeerConnection", "webkitRTCPeerConnection", "RTCDataChannel", "RTCSessionDescription", "RTCIceCandidate"]) {
    lock(globalThis, k, undefined);
  }
  const nothing = function () {};
  for (const k of ["back", "forward", "go", "pushState", "replaceState"]) lock(History.prototype, k, nothing);
  lock(globalThis, "open", () => null);
  lock(Document.prototype, "execCommand", () => false);
})();
