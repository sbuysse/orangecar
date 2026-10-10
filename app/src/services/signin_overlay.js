// Injected by services/signin.rs into every document of the sign-in tab (top
// frame only). The sign-in tab hides the dashboard, so it brings its own
// on-screen keyboard and a "Done" button. Keys are sent to the backend through
// the `__rascarSignin` DevTools binding, which types them as trusted input into
// the focused field. Styling reuses the dashboard's own keyboard rules and
// tokens (passed in as `css`), inside a closed shadow root so the site's CSS
// and ours never mix.
(css) => {
  if (window.top !== window || window.__rascarSigninUi) return;
  window.__rascarSigninUi = true;
  const send = (k, v) => { try { window.__rascarSignin(JSON.stringify({ k, v })); } catch (e) { /* binding gone: tab closing */ } };

  const LAYERS = {
    lower: ['1234567890', 'azertyuiop', 'qsdfghjklm', 'wxcvbn@.-_'],
    upper: ['1234567890', 'AZERTYUIOP', 'QSDFGHJKLM', 'WXCVBN@.-_'],
    symbols: ['!#$%&*+=?/', '()[]{}<>|\\', '"\':;,~^`€£', '@.-_§°²µ¨ '],
  };

  const mount = () => {
    const host = document.createElement('rascar-signin');
    const root = host.attachShadow({ mode: 'closed' });
    const style = document.createElement('style');
    // Layout only; every surface comes from the dashboard's .osk / .nav-kbd-btn rules.
    style.textContent = css + `
      :host { all: initial; position: fixed; inset: 0; z-index: 2147483647; pointer-events: none; font-family: var(--font); }
      .bar { position: absolute; left: 14px; bottom: 14px; display: flex; gap: 10px; pointer-events: auto; }
      .osk.nav-osk { pointer-events: auto; bottom: 14px; }
      /* The keyboard takes the half of the screen away from the focused field.
         The bar never moves; on the left the keyboard sits above it. */
      .osk.nav-osk.left { right: auto; left: 14px; bottom: 76px; }
      .done { width: auto; padding: 0 18px; gap: 8px; }`;
    const bar = document.createElement('div');
    bar.className = 'bar';
    const btn = (label, cls, onTap, aria) => {
      const b = document.createElement('button');
      b.className = cls;
      b.textContent = label;
      if (aria) b.setAttribute('aria-label', aria);
      b.addEventListener('click', onTap);
      return b;
    };
    const osk = document.createElement('div');
    osk.className = 'osk nav-osk hidden';
    let layer = 'lower';
    let left = false;
    const place = side => {
      left = side;
      osk.classList.toggle('left', left);
    };
    // Dodge the field being typed into: keyboard on the opposite half.
    const dodge = () => {
      const el = document.activeElement;
      if (!el || el === document.body || el === host) return;
      const r = el.getBoundingClientRect();
      place(r.left + r.width / 2 > innerWidth / 2);
    };
    document.addEventListener('focusin', dodge, true);
    const render = () => {
      const rows = LAYERS[layer].map((chars, r) => {
        const row = document.createElement('div');
        row.className = 'osk-row';
        if (r === 3) row.append(btn(layer === 'upper' ? '⇧' : '⇪', 'osk-wide', () => { layer = layer === 'upper' ? 'lower' : 'upper'; render(); }, 'Shift'));
        for (const c of chars) if (c !== ' ') row.append(btn(c, '', () => { send('text', c); if (layer === 'upper') { layer = 'lower'; render(); } }));
        if (r === 3) row.append(btn('⌫', 'osk-wide', () => send('back'), 'Delete'));
        return row;
      });
      const last = document.createElement('div');
      last.className = 'osk-row';
      last.append(btn(layer === 'symbols' ? 'abc' : '?#&', 'osk-wide', () => { layer = layer === 'symbols' ? 'lower' : 'symbols'; render(); }, 'Symbols'));
      last.append(btn('space', 'osk-space', () => send('text', ' ')));
      last.append(btn('⇆', 'osk-wide', () => place(!left), 'Move keyboard'));
      last.append(btn('↵ Enter', 'osk-go', () => send('enter')));
      osk.replaceChildren(...rows, last);
    };
    render();
    bar.append(
      btn('⌨', 'nav-kbd-btn', () => { dodge(); osk.classList.toggle('hidden'); }, 'Keyboard'),
      btn('✓ Done', 'nav-kbd-btn done', () => send('done'), 'Done'),
    );
    // A key press must never take focus away from the field being typed into.
    for (const el of [osk, bar]) for (const type of ['pointerdown', 'mousedown']) el.addEventListener(type, e => e.preventDefault());
    root.append(style, osk, bar);
    document.documentElement.append(host);
  };
  if (document.documentElement) mount();
  else document.addEventListener('DOMContentLoaded', mount, { once: true });
}
