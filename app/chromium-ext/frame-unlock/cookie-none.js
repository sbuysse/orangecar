// Embedded app frames only: a cookie a page script writes without a SameSite
// attribute defaults to Lax, and Chromium never stores a Lax cookie inside a
// cross-site frame. Waze's "I understand" (cookie-acknowledgement=true) was
// dropped that way, so its banner came back on every reload. background.js
// already relaxes cookies set by responses; this covers document.cookie.
(() => {
  if (window.top === window) return;
  const desc = Object.getOwnPropertyDescriptor(Document.prototype, 'cookie');
  if (!desc || !desc.set) return;
  Object.defineProperty(Document.prototype, 'cookie', {
    configurable: true,
    enumerable: desc.enumerable,
    get() { return desc.get.call(this); },
    set(value) {
      const v = String(value);
      desc.set.call(this, /;\s*samesite\s*=/i.test(v) ? v : `${v}; SameSite=None; Secure`);
    },
  });
})();
