'use strict';

const token = document.querySelector('meta[name="x-token"]').content;
const $ = (id) => document.getElementById(id);
const DASH = '—';
let provider = 'claude';
let state = null;

const pct = (v) => (v === null ? DASH : v.toFixed(1));
const when = (iso) => (iso ? new Date(iso) : null);
const pad = (n) => String(n).padStart(2, '0');
const current = () => (state ? state.providers[provider] : null);
// An empty ledger is the only case that reports weekly_observed_at null with one of these reasons.
const noReading = (p) => p.weekly_observed_at === null && /^(weekly_observation|short_window)_unavailable$/.test(p.reason);

function countdown(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const days = Math.floor(s / 86400);
  const clock = pad(Math.floor((s % 86400) / 3600)) + ':' + pad(Math.floor((s % 3600) / 60)) + ':' + pad(s % 60);
  return days ? days + 'd ' + clock : clock;
}

function ago(date) {
  const minutes = Math.max(0, Math.floor((Date.now() - date.getTime()) / 60000));
  if (minutes < 1) return 'just now';
  if (minutes < 60) return minutes + ' min ago';
  const hours = Math.floor(minutes / 60);
  return hours < 48 ? hours + ' h ago' : Math.floor(hours / 24) + ' d ago';
}

async function api(path, body) {
  let response;
  try {
    response = await fetch(path, body === undefined ? undefined : {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Token': token },
      body: JSON.stringify(body),
    });
  } catch (err) {
    throw new Error('the local server is not answering');
  }
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || 'request failed (' + response.status + ')');
  return data;
}

// Runs every second: the countdown and the "read N min ago" line.
function tick() {
  const p = current();
  if (!p || document.hidden) return;
  const reset = when(p.weekly_reset_at);
  const seen = when(p.weekly_observed_at);
  const reason = p.reason.replace(/_/g, ' ');
  $('reset').textContent = reset ? countdown(reset - Date.now()) : DASH;
  $('fresh').textContent = noReading(p) ? 'no reading yet' : seen ? 'read ' + ago(seen) + ' · ' + reason : reason;
}

function render() {
  const p = current();
  if (!p) return;
  const used = p.weekly_used_percent;
  const estimated = p.weekly_estimated_percent;
  const reset = when(p.weekly_reset_at);
  const shortReset = when(p.short_reset_at);
  $('hud').dataset.mode = p.mode.toLowerCase();
  $('mode').textContent = p.mode.toUpperCase();
  $('empty').hidden = !noReading(p);
  $('reset-at').textContent = reset
    ? 'resets ' + reset.toLocaleString([], { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' })
    : '';
  $('left').textContent = used === null ? DASH : (100 - used).toFixed(1);
  $('used').textContent = used === null ? '' : 'used ' + pct(used) + ' %' + (estimated === null ? '' : ' · estimated ' + pct(estimated) + ' %');
  $('short').textContent = pct(p.short_used_percent);
  $('short-at').textContent = shortReset ? 'used · resets ' + shortReset.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : '';
  $('hook').textContent = JSON.stringify(p.hook, null, 2);
  tick();
}

async function refresh() {
  try {
    state = await api('/api/state');
    $('hud-error').textContent = '';
    render();
  } catch (err) {
    $('hud-error').textContent = 'Error: ' + err.message;
  }
}

function choose(name) {
  provider = name;
  document.querySelectorAll('.switch button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.provider === name)));
  $('r-provider').value = name;
  render();
}

// Posts one form's body; shows a failure in that form's own live region. Success is silent: the readouts update.
async function send(form, path, body) {
  const error = form.querySelector('.error');
  const button = form.querySelector('button[type="submit"]');
  error.textContent = '';
  button.disabled = true;
  button.setAttribute('aria-busy', 'true');
  try {
    state = await api(path, body);
    return true;
  } catch (err) {
    error.textContent = 'Error: ' + err.message;
    return false;
  } finally {
    button.disabled = false;
    button.removeAttribute('aria-busy');
  }
}

const iso = (value) => {
  const date = new Date(value);
  return value && !isNaN(date) ? date.toISOString() : undefined;
};
const num = (value) => (value === '' ? undefined : parseFloat(value));

$('record').addEventListener('submit', async (event) => {
  event.preventDefault();
  const f = event.target.elements;
  const name = f.provider.value;
  const ok = await send(event.target, '/api/record', {
    provider: name,
    weekly: num(f.weekly.value),
    resets: iso(f.resets.value),
    five_hour: num(f.five_hour.value),
    five_hour_resets: iso(f.five_hour_resets.value),
    at: iso(f.observed.value),
  });
  if (ok) {
    event.target.reset();
    choose(name);
  }
});

$('ingest').addEventListener('submit', async (event) => {
  event.preventDefault();
  const ok = await send(event.target, '/api/ingest', { text: event.target.elements.text.value });
  if (ok) {
    event.target.reset();
    choose('claude');
  }
});

document.querySelectorAll('.switch button').forEach((b) => b.addEventListener('click', () => choose(b.dataset.provider)));

$('copy').addEventListener('click', async () => {
  const note = $('copy-note');
  try {
    await navigator.clipboard.writeText($('hook').textContent);
    note.textContent = 'Copied.';
  } catch (err) {
    note.textContent = 'Copy is not available here. Select the text above and copy it.';
  }
  setTimeout(() => { note.textContent = ''; }, 4000);
});

// The mark is inlined so CSS can drive it. Its own <style> and style="" attributes are cut out before parsing
// (the page's CSP forbids them, and its :root block would clobber the page tokens); app.css restyles it.
fetch('/brand/mark.svg')
  .then((response) => response.text())
  .then((text) => {
    const bare = text.replace(/<style[\s\S]*?<\/style>/g, '').replace(/\sstyle="[^"]*"/g, '');
    const svg = new DOMParser().parseFromString(bare, 'image/svg+xml').documentElement;
    if (svg.nodeName !== 'svg' || svg.querySelector('parsererror')) return;
    svg.querySelectorAll('title').forEach((n) => n.remove());
    ['width', 'height', 'role', 'aria-labelledby'].forEach((a) => svg.removeAttribute(a));
    svg.setAttribute('aria-hidden', 'true');
    $('mark').replaceChildren(document.importNode(svg, true));
  })
  .catch(() => {});

setInterval(tick, 1000);
setInterval(() => { if (!document.hidden) refresh(); }, 15000);
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
refresh();
