// Embedded VM console: noVNC (vendored, MPL-2.0, static/vendor/novnc) over
// the HostingOS websocket relay. The page never sees a hypervisor ticket or
// VNC password: it asks its own server for a one-time token (POST + CSRF),
// opens a same-origin websocket with it, and the server does the rest.
//
// Markup: an element with [data-console] carrying
//   data-token-url  POST endpoint returning {"token": "...", "ws": "/path"}
//   data-csrf       CSRF token for that POST
//   data-msg-*      status texts (connecting, connected, disconnected, failed)
// with children [data-console-screen], [data-console-status] and buttons
// [data-console-action="ctrlaltdel|fullscreen|reconnect|paste|scale"].
import RFB from '../vendor/novnc/core/rfb.js';

const root = document.querySelector('[data-console]');
if (root) init(root);

function init(root) {
  const screen = root.querySelector('[data-console-screen]');
  const status = root.querySelector('[data-console-status]');
  const clip = root.querySelector('[data-console-clipboard]');
  let rfb = null;
  let busy = false;

  const msg = (key) => root.dataset['msg' + key.charAt(0).toUpperCase() + key.slice(1)] || key;
  const setStatus = (state, text) => {
    root.dataset.state = state;
    if (status) status.textContent = text || msg(state);
  };
  const buttons = (connected) => {
    root.querySelectorAll('[data-console-action]').forEach((b) => {
      if (b.dataset.consoleAction !== 'reconnect') b.disabled = !connected;
    });
  };

  async function connect() {
    if (busy) return;
    busy = true;
    buttons(false);
    setStatus('connecting');
    try {
      const body = new URLSearchParams({ csrf_token: root.dataset.csrf });
      const res = await fetch(root.dataset.tokenUrl, {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Accept': 'application/json' },
        body,
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok || !data.token) {
        setStatus('failed', data.error || msg('failed'));
        return;
      }
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = proto + '//' + location.host + data.ws + '?token=' + encodeURIComponent(data.token);
      screen.textContent = '';
      rfb = new RFB(screen, url, { wsProtocols: ['binary'] });
      rfb.scaleViewport = true;
      rfb.resizeSession = false;
      rfb.focusOnClick = true;
      rfb.background = 'transparent';
      rfb.addEventListener('connect', () => {
        setStatus('connected');
        buttons(true);
        rfb.focus();
      });
      rfb.addEventListener('disconnect', (e) => {
        setStatus(e.detail && e.detail.clean ? 'disconnected' : 'failed');
        buttons(false);
        rfb = null;
      });
      rfb.addEventListener('securityfailure', () => setStatus('failed'));
      rfb.addEventListener('desktopname', (e) => {
        if (e.detail && e.detail.name) root.dataset.desktop = e.detail.name;
      });
    } catch (_) {
      setStatus('failed');
    } finally {
      busy = false;
    }
  }

  root.addEventListener('click', (e) => {
    const b = e.target.closest('[data-console-action]');
    if (!b) return;
    const action = b.dataset.consoleAction;
    if (action === 'reconnect') {
      if (rfb) rfb.disconnect();
      connect();
      return;
    }
    if (!rfb) return;
    if (action === 'ctrlaltdel') rfb.sendCtrlAltDel();
    if (action === 'fullscreen') {
      if (document.fullscreenElement) document.exitFullscreen();
      else screen.requestFullscreen().catch(() => {});
    }
    if (action === 'scale') {
      rfb.scaleViewport = !rfb.scaleViewport;
      b.setAttribute('aria-pressed', String(!rfb.scaleViewport));
    }
    if (action === 'paste' && clip && clip.value) {
      // Typed into the VM as clipboard text (RFB cut-text); noVNC limits it.
      rfb.clipboardPasteFrom(clip.value);
      rfb.focus();
    }
  });

  window.addEventListener('beforeunload', () => {
    if (rfb) rfb.disconnect();
  });
  connect();
}
