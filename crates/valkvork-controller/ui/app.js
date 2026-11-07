// Valkvork web UI (FR-UI). Vanilla ES module, no third-party code.
// All text from the API is inserted with textContent, never as HTML.

const SVG = 'http://www.w3.org/2000/svg';
const SVG_TAGS = new Set(['svg', 'rect', 'polyline', 'line', 'title']);
const main = document.getElementById('main');
const nav = document.getElementById('nav');
const liveEl = document.getElementById('live');
const logoutBtn = document.getElementById('logout');

let csrf = null;

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

function h(tag, props, ...kids) {
  const el = SVG_TAGS.has(tag) ? document.createElementNS(SVG, tag) : document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'text') el.textContent = v;
    else if (k === 'value') el.value = v;
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  for (const c of kids.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const link = (href, text) => h('a', { href, 'data-link': true }, text);
const enc = encodeURIComponent;

const STATUS_CLASS = {
  RUNNING: 'ok', HEALTHY: 'ok', ACTIVE: 'ok', created: 'ok',
  PENDING: 'warn', ASSIGNED: 'warn', STOPPING: 'warn', IN_PROGRESS: 'warn', DELETING: 'warn', DRAINING: 'warn',
  'scale-only': 'warn', 'policy-only': 'warn', replacing: 'warn',
  FAILED: 'bad', LOST: 'bad', UNHEALTHY: 'bad', DEGRADED: 'bad', HALTED: 'bad',
};
// Status is always text plus colour, never colour alone (FR-UI-08).
const badge = (s) => h('span', { class: `badge ${STATUS_CLASS[s] || ''}` }, s || '—');

function table(headers, rows, empty = 'Nothing here yet.') {
  if (!rows.length) return h('p', { class: 'muted' }, empty);
  return h('div', { class: 'table-wrap' },
    h('table', {},
      h('thead', {}, h('tr', {}, headers.map((x) => h('th', { class: x.startsWith('#') ? 'num' : null }, x.replace(/^#/, ''))))),
      h('tbody', {}, rows.map((r) => h('tr', {}, r.map((c, i) => h('td', { class: headers[i].startsWith('#') ? 'num' : null }, c)))))));
}

function bar(used, total) {
  const ratio = total > 0 ? Math.min(used / total, 1) : 0;
  const w = 120;
  return h('span', { class: 'bar' },
    h('svg', { width: w, height: 10, viewBox: `0 0 ${w} 10`, role: 'img', 'aria-label': `${pct(ratio)} allocated` },
      h('rect', { class: 'bg', width: w, height: 10, rx: 3 }),
      h('rect', { class: ratio > 0.8 ? 'fg hot' : 'fg', width: Math.round(ratio * w), height: 10, rx: 3 })),
    h('span', { class: 'pct' }, pct(ratio)));
}

function sparkline(values, { width = 260, height = 44, max } = {}) {
  if (!values.length) return h('span', { class: 'muted' }, 'no samples yet (one per minute)');
  const top = Math.max(max ?? 0, ...values, 1e-9);
  const step = values.length > 1 ? width / (values.length - 1) : 0;
  const points = values.map((v, i) => `${(i * step).toFixed(1)},${(height - 2 - (v / top) * (height - 4)).toFixed(1)}`).join(' ');
  return h('svg', { class: 'spark', width, height, viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': 'last 60 minutes' },
    h('line', { x1: 0, y1: height - 1, x2: width, y2: height - 1 }),
    h('polyline', { points }));
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

const pct = (r) => `${Math.round(r * 100)} %`;
const cores = (m) => `${(m / 1000).toFixed(m % 1000 ? 2 : 0)}`;
function bytes(b) {
  for (const [u, s] of [['GiB', 30], ['MiB', 20], ['KiB', 10]]) if (b >= 2 ** s) return `${(b / 2 ** s).toFixed(1)} ${u}`;
  return `${b} B`;
}
function dur(sec) {
  sec = Math.max(0, Math.floor(sec));
  if (sec < 60) return `${sec}s`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ${Math.floor((sec % 3600) / 60)}m`;
  return `${Math.floor(sec / 86400)}d ${Math.floor((sec % 86400) / 3600)}h`;
}
const ago = (ms) => (ms ? `${dur((Date.now() - ms) / 1000)} ago` : '—');
const age = (ms) => (ms ? dur((Date.now() - ms) / 1000) : '—');
const time = (ms) => (ms ? new Date(ms).toLocaleString() : '—');
const short = (id) => (id ? id.slice(0, 8) : '—');

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

class ApiError extends Error {
  constructor(status, message, errors) { super(message); this.status = status; this.errors = errors || []; }
}

async function api(path, { method = 'GET', body } = {}) {
  const opts = { method, headers: {}, credentials: 'same-origin' };
  if (body !== undefined) { opts.headers['content-type'] = 'application/json'; opts.body = JSON.stringify(body); }
  if (method !== 'GET') opts.headers['x-valkvork-csrf'] = csrf || '';
  const r = await fetch(`/ui/api/${path}`, opts);
  const data = await r.json().catch(() => ({}));
  if (r.status === 401 && path !== 'login') { signedOut(); throw new ApiError(401, 'Signed out'); }
  if (!r.ok) throw new ApiError(r.status, data.error || r.statusText, data.errors);
  return data;
}

function errorBox(e) {
  return h('div', { class: 'msg error', role: 'alert' }, e.message,
    e.errors && e.errors.length ? h('ul', {}, e.errors.map((x) => h('li', {}, h('code', {}, x.code), ` at ${x.field || '<root>'}: ${x.message}`))) : null);
}

// ---------------------------------------------------------------------------
// Live updates (FR-UI-04): SSE, falling back to 5 s polling
// ---------------------------------------------------------------------------

let source = null;
let poll = null;
let refreshTimer = null;

function startLive() {
  stopLive();
  source = new EventSource('/ui/api/events');
  source.addEventListener('change', scheduleRefresh);
  source.onopen = () => { liveEl.textContent = '● live'; if (poll) { clearInterval(poll); poll = null; } };
  source.onerror = () => {
    liveEl.textContent = '○ polling';
    if (!poll) poll = setInterval(scheduleRefresh, 5000);
  };
}

function stopLive() {
  if (source) source.close();
  source = null;
  if (poll) clearInterval(poll);
  poll = null;
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => {
    // Never clobber a form the operator is typing in.
    const a = document.activeElement;
    if (a && main.contains(a) && ['INPUT', 'TEXTAREA', 'SELECT'].includes(a.tagName)) return;
    if (currentPage && !currentPage.static) render(true);
  }, 300);
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

const routes = [
  [/^\/ui\/?$/, 'overview', overviewPage],
  [/^\/ui\/services\/?$/, 'services', servicesPage],
  [/^\/ui\/services\/([^/]+)$/, 'services', servicePage],
  [/^\/ui\/nodes\/?$/, 'nodes', nodesPage],
  [/^\/ui\/nodes\/([^/]+)$/, 'nodes', nodePage],
  [/^\/ui\/workloads\/?$/, 'workloads', workloadsPage],
  [/^\/ui\/events\/?$/, 'events', eventsPage],
  [/^\/ui\/deploy\/?$/, 'deploy', deployPage, true],
];

let currentPage = null;

function go(href) {
  history.pushState(null, '', href);
  render();
}

document.addEventListener('click', (e) => {
  const a = e.target.closest('a[data-link]');
  if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
  e.preventDefault();
  go(a.getAttribute('href'));
});
window.addEventListener('popstate', () => render());

let renderSeq = 0;
async function render(isRefresh = false) {
  if (!csrf) return;
  const path = location.pathname;
  const route = routes.find(([re]) => re.test(path));
  const seq = ++renderSeq;
  if (!route) { main.replaceChildren(h('h1', {}, 'Not found'), h('p', {}, link('/ui/', 'Back to the overview'))); return; }
  const [re, section, page, isStatic] = route;
  currentPage = { static: !!isStatic };
  for (const a of nav.querySelectorAll('a')) {
    if (a.dataset.nav === section) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  }
  const params = path.match(re).slice(1).map(decodeURIComponent);
  try {
    const node = await page(...params, new URLSearchParams(location.search));
    if (seq !== renderSeq) return; // a newer navigation won
    main.replaceChildren(node);
    if (!isRefresh) main.focus({ preventScroll: true });
  } catch (e) {
    if (seq !== renderSeq || e.status === 401) return;
    main.replaceChildren(errorBox(e));
  }
}

// ---------------------------------------------------------------------------
// Session (FR-UI-06)
// ---------------------------------------------------------------------------

function signedOut() {
  csrf = null;
  stopLive();
  nav.hidden = true; logoutBtn.hidden = true; liveEl.hidden = true;
  const input = h('input', { type: 'password', name: 'token', autocomplete: 'current-password', required: true, 'aria-label': 'Admin token' });
  const msg = h('div');
  const form = h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      msg.replaceChildren();
      try {
        const r = await api('login', { method: 'POST', body: { token: input.value } });
        signedIn(r.csrf);
      } catch (err) { msg.replaceChildren(errorBox(err)); }
    },
  }, h('label', {}, 'Admin token'), input, h('button', { type: 'submit' }, 'Sign in'), msg);
  main.replaceChildren(h('div', { class: 'login panel' }, h('h1', {}, 'Sign in'), h('p', { class: 'muted' }, 'Use the controller admin token.'), form));
  input.focus();
}

function signedIn(token) {
  csrf = token;
  nav.hidden = false; logoutBtn.hidden = false; liveEl.hidden = false;
  startLive();
  render();
}

logoutBtn.addEventListener('click', async () => {
  try { await api('logout', { method: 'POST' }); } catch (_) { /* already signed out */ }
  signedOut();
});

// ---------------------------------------------------------------------------
// Pages (FR-UI-03)
// ---------------------------------------------------------------------------

function eventsTable(events) {
  return table(['Time', 'Kind', 'Actor', 'Message'],
    events.map((e) => [time(e.created_at), badge(e.kind), h('span', { class: 'mono' }, e.actor), h('span', {}, e.message)]),
    'No events yet.');
}

async function overviewPage() {
  const { overview: o, events, controller } = await api('overview');
  const w = o.workloads;
  const health = [
    `Controller ${controller.version}`,
    o.recovery_grace_remaining_seconds > 0 ? `recovery grace ${o.recovery_grace_remaining_seconds} s` : 'healthy',
    `uptime ${dur(o.uptime_seconds)}`,
    `DB ${bytes(controller.db_bytes)}`,
    'backup not monitored',
    controller.tls ? 'TLS' : 'plaintext (development)',
  ].join(' · ');
  const tile = (label, big, rows) => h('div', { class: 'panel tile' }, h('div', { class: 'label' }, label), h('div', { class: 'big' }, big), h('div', { class: 'rows' }, rows));
  return h('div', {},
    h('h1', {}, 'Overview'),
    h('p', { class: 'sub' }, health),
    h('div', { class: 'tiles' },
      tile('Nodes', `${o.nodes_healthy} healthy`, `${o.nodes_unhealthy} unhealthy · ${o.nodes_draining} draining`),
      tile('Workloads', `${w.RUNNING} running`, `${w.PENDING + w.ASSIGNED} starting · ${w.FAILED} failed · ${w.LOST} lost`),
      tile('Unplaced', `${o.unplaced_pending} pending`, o.unplaced_pending ? 'capacity needed' : 'all placed'),
      tile('Autoscaling (1 h)', `${o.scaling_actions_last_hour} actions`, `${o.services_total} services · ${o.services_degraded} degraded · ${o.rollouts_in_progress} rolling out`)),
    h('h2', {}, 'Capacity by class'),
    table(['Class', 'Nodes', 'CPU allocated', 'Memory allocated', 'CPU allocation, last 60 min'],
      o.classes.map((c) => [
        h('strong', {}, c.class),
        `${c.healthy_nodes}/${c.nodes} healthy`,
        h('span', {}, bar(c.cpu_allocated, c.cpu_allocatable), ` ${cores(c.cpu_allocated)}/${cores(c.cpu_allocatable)} cores`),
        h('span', {}, bar(c.memory_allocated, c.memory_allocatable), ` ${bytes(c.memory_allocated)}/${bytes(c.memory_allocatable)}`),
        sparkline(c.series.map((p) => p.cpu), { width: 160, height: 28, max: 1 }),
      ]), 'No nodes have registered yet.'),
    h('h2', {}, 'Recent events'),
    eventsTable(events));
}

async function servicesPage() {
  const services = await api('services');
  services.sort((a, b) => a.service_id.localeCompare(b.service_id));
  return h('div', {},
    h('div', { class: 'row' }, h('h1', {}, 'Services'), h('span', { class: 'spacer' }), link('/ui/deploy', 'Deploy a manifest')),
    h('p', { class: 'sub' }, `${services.length} services`),
    table(['Service', 'Image', '#Version', '#Running / target', '#CPU', 'Status', 'Rollout'],
      services.map((s) => [
        link(`/ui/services/${enc(s.service_id)}`, s.service_id),
        h('span', { class: 'mono' }, s.image),
        s.version,
        `${s.running_replicas} / ${s.target_replicas}`,
        `${Math.round(s.cpu_utilization)} %`,
        badge(s.status),
        badge(s.rollout_status),
      ]), 'No services. Use Deploy to apply a manifest.'));
}

function workloadRows(ws, { service = false, node = true } = {}) {
  return ws.map((w) => [
    h('span', { class: 'mono' }, short(w.workload_id)),
    service ? link(`/ui/services/${enc(w.service_id)}`, w.service_id) : null,
    w.service_version,
    node ? (w.node_id ? link(`/ui/nodes/${enc(w.node_id)}`, short(w.node_id)) : '—') : null,
    badge(w.status),
    h('span', {}, w.status_reason || ''),
    w.restart_count,
    age(w.created_at),
  ].filter((c) => c !== null));
}

function workloadHeaders({ service = false, node = true } = {}) {
  return ['Workload', service ? 'Service' : null, '#Version', node ? 'Node' : null, 'Status', 'Reason', '#Restarts', 'Age'].filter((x) => x);
}

async function servicePage(id) {
  const { service: s, cpu_series, scaling_events } = await api(`services/${enc(id)}`);
  const msg = h('div');
  const live = s.workloads.filter((w) => !['STOPPED', 'FAILED'].includes(w.status));
  const current = live.filter((w) => w.service_version === s.version && w.status === 'RUNNING').length;

  const replicas = h('input', { type: 'number', min: 0, value: s.target_replicas, 'aria-label': 'Replicas' });
  const scale = h('form', {
    class: 'inline',
    onsubmit: async (e) => {
      e.preventDefault();
      try {
        const r = await api(`services/${enc(id)}/scale`, { method: 'POST', body: { replicas: Number(replicas.value) } });
        msg.replaceChildren(h('div', { class: 'msg ok' }, `Replicas ${r.old_replicas} → ${r.new_replicas}${r.new_replicas !== Number(replicas.value) ? ' (clamped to the autoscale range)' : ''}`));
        replicas.blur();
        render(true);
      } catch (err) { msg.replaceChildren(errorBox(err)); }
    },
  }, h('label', {}, 'Scale to'), replicas, h('button', { type: 'submit' }, 'Scale'));

  const confirm = h('input', { type: 'text', placeholder: `type ${s.service_id}`, 'aria-label': 'Type the service id to confirm' });
  const del = h('button', { type: 'submit', class: 'danger', disabled: true }, 'Delete');
  confirm.addEventListener('input', () => { del.disabled = confirm.value !== s.service_id; });
  const delForm = h('form', {
    class: 'inline',
    onsubmit: async (e) => {
      e.preventDefault();
      try {
        await api(`services/${enc(id)}/delete`, { method: 'POST', body: { confirm: confirm.value } });
        confirm.value = '';
        confirm.blur();
        msg.replaceChildren(h('div', { class: 'msg ok' }, 'Deleting: workloads are stopping.'));
        render(true);
      } catch (err) { msg.replaceChildren(errorBox(err)); }
    },
  }, confirm, del);

  const lastCpu = cpu_series.length ? `${Math.round(cpu_series[cpu_series.length - 1].value)} %` : '—';
  return h('div', {},
    h('div', { class: 'row' }, h('h1', {}, s.service_id), badge(s.status), badge(s.rollout_status)),
    h('p', { class: 'sub' }, `version ${s.version} · ${s.running_replicas} running / ${s.target_replicas} target · ${s.pending_replicas} pending · ${s.failed_replicas} failed`),
    msg,
    s.status === 'DELETING' ? null : h('div', { class: 'panel actions' }, scale, delForm),
    h('div', { class: 'grid2' },
      h('div', {},
        h('h2', {}, 'Rollout'),
        h('div', { class: 'panel' },
          s.rollout_status === 'NONE'
            ? h('span', { class: 'muted' }, `All live workloads run version ${s.version}.`)
            : h('span', {}, `${current} of ${s.target_replicas} replicas on version ${s.version} `, bar(current, s.target_replicas)))),
      h('div', {},
        h('h2', {}, `CPU utilisation, last 60 min (now ${lastCpu})`),
        h('div', { class: 'panel' }, sparkline(cpu_series.map((p) => p.value), { max: 100 })))),
    h('h2', {}, 'Workloads'),
    table(workloadHeaders(), workloadRows(s.workloads)),
    h('h2', {}, 'Scaling events'),
    table(['Time', '#From', '#To', '#CPU', 'Reason'],
      scaling_events.map((e) => [time(e.created_at), e.from_replicas, e.to_replicas, `${e.metric_value.toFixed(1)} %`, e.reason]),
      'No autoscaler actions.'),
    h('h2', {}, 'Spec'),
    h('pre', {}, s.spec_toml));
}

function nodeStatus(n) {
  return h('span', {}, badge(n.status), n.draining ? [' ', badge('DRAINING')] : null);
}

async function nodesPage() {
  const nodes = await api('nodes');
  nodes.sort((a, b) => a.node_class.localeCompare(b.node_class) || a.hostname.localeCompare(b.hostname));
  return h('div', {},
    h('h1', {}, 'Nodes'),
    h('p', { class: 'sub' }, `${nodes.length} nodes`),
    table(['Node', 'Hostname', 'Class', 'Status', 'CPU', 'Memory', '#Workloads', 'Heartbeat', 'Agent', 'Runtime'],
      nodes.map((n) => [
        link(`/ui/nodes/${enc(n.node_id)}`, short(n.node_id)),
        n.hostname, n.node_class, nodeStatus(n),
        bar(n.used_cpu_millicores, n.allocatable_cpu_millicores),
        bar(n.used_memory_bytes, n.allocatable_memory_bytes),
        n.workload_count, ago(n.last_heartbeat_at), n.agent_version, n.runtime,
      ]), 'No nodes have registered yet.'));
}

async function nodePage(id) {
  const { node: n, workloads } = await api(`nodes/${enc(id)}`);
  const msg = h('div');
  const action = n.draining ? 'undrain' : 'drain';
  const btn = h('button', {
    type: 'button',
    class: n.draining ? 'secondary' : 'danger',
    onclick: async () => {
      if (action === 'drain' && !window.confirm(`Drain ${n.hostname}? Its workloads move to other nodes.`)) return;
      try {
        await api(`nodes/${enc(id)}/${action}`, { method: 'POST' });
        msg.replaceChildren(h('div', { class: 'msg ok' }, action === 'drain' ? 'Draining: workloads migrate start-new-then-stop-old.' : 'Node accepts placements again.'));
        render(true);
      } catch (err) { msg.replaceChildren(errorBox(err)); }
    },
  }, n.draining ? 'Undrain' : 'Drain');
  // Only a node that stopped heartbeating can be forgotten.
  const removeBtn = n.status !== 'UNHEALTHY' ? null : h('button', {
    type: 'button',
    class: 'danger',
    onclick: async () => {
      if (!window.confirm(`Remove ${n.hostname}? Its lost workloads are marked STOPPED. If the node comes back it joins again as a new node.`)) return;
      try {
        await api(`nodes/${enc(id)}/remove`, { method: 'POST' });
        go('/ui/nodes');
      } catch (err) { msg.replaceChildren(errorBox(err)); }
    },
  }, 'Remove');
  return h('div', {},
    h('div', { class: 'row' }, h('h1', {}, n.hostname), nodeStatus(n), h('span', { class: 'spacer' }), btn, removeBtn ? [' ', removeBtn] : null),
    h('p', { class: 'sub mono' }, n.node_id),
    msg,
    h('div', { class: 'grid2' },
      h('div', { class: 'panel' },
        h('dl', { class: 'facts' },
          h('dt', {}, 'Class'), h('dd', {}, n.node_class),
          h('dt', {}, 'Address'), h('dd', { class: 'mono' }, n.ip_address),
          h('dt', {}, 'Heartbeat'), h('dd', {}, ago(n.last_heartbeat_at)),
          h('dt', {}, 'Agent'), h('dd', {}, n.agent_version),
          h('dt', {}, 'Runtime'), h('dd', {}, n.runtime),
          h('dt', {}, 'Host volumes'), h('dd', {}, n.host_volumes.length ? n.host_volumes.join(', ') : '—'))),
      h('div', { class: 'panel' },
        h('dl', { class: 'facts' },
          h('dt', {}, 'CPU'), h('dd', {}, bar(n.used_cpu_millicores, n.allocatable_cpu_millicores), ` ${cores(n.used_cpu_millicores)} / ${cores(n.allocatable_cpu_millicores)} cores`),
          h('dt', {}, 'Memory'), h('dd', {}, bar(n.used_memory_bytes, n.allocatable_memory_bytes), ` ${bytes(n.used_memory_bytes)} / ${bytes(n.allocatable_memory_bytes)}`),
          h('dt', {}, 'Workloads'), h('dd', {}, n.workload_count)))),
    h('h2', {}, 'Workloads on this node'),
    table(workloadHeaders({ service: true, node: false }), workloadRows(workloads, { service: true, node: false })));
}

const STATUSES = ['', 'PENDING', 'ASSIGNED', 'RUNNING', 'STOPPING', 'LOST', 'STOPPED', 'FAILED'];

async function workloadsPage(params) {
  const q = new URLSearchParams();
  for (const k of ['service', 'node', 'status']) if (params.get(k)) q.set(k, params.get(k));
  const ws = await api(`workloads?${q}`);
  const service = h('input', { type: 'text', value: params.get('service') || '', placeholder: 'any' });
  const node = h('input', { type: 'text', value: params.get('node') || '', placeholder: 'any' });
  const status = h('select', {}, STATUSES.map((s) => h('option', { value: s, selected: s === (params.get('status') || '') }, s || 'any')));
  const filters = h('form', {
    class: 'filters',
    onsubmit: (e) => {
      e.preventDefault();
      const next = new URLSearchParams();
      if (service.value) next.set('service', service.value.trim());
      if (node.value) next.set('node', node.value.trim());
      if (status.value) next.set('status', status.value);
      document.activeElement?.blur();
      go(`/ui/workloads${next.toString() ? `?${next}` : ''}`);
    },
  },
  h('label', {}, 'Service', service), h('label', {}, 'Node id', node), h('label', {}, 'Status', status),
  h('button', { type: 'submit' }, 'Filter'));
  return h('div', {},
    h('h1', {}, 'Workloads'),
    h('p', { class: 'sub' }, `${ws.length} workloads`),
    filters,
    table(workloadHeaders({ service: true }), workloadRows(ws, { service: true })));
}

async function eventsPage(params) {
  const kind = params.get('kind') || '';
  const events = await api(`cluster-events?limit=200${kind ? `&kind=${enc(kind)}` : ''}`);
  const select = h('select', {
    'aria-label': 'Kind',
    onchange: (e) => { e.target.blur(); go(`/ui/events${e.target.value ? `?kind=${enc(e.target.value)}` : ''}`); },
  }, ['', 'NODE', 'WORKLOAD', 'DEPLOY', 'SCALE', 'AUDIT'].map((k) => h('option', { value: k, selected: k === kind }, k || 'all kinds')));
  return h('div', {},
    h('div', { class: 'row' }, h('h1', {}, 'Events'), h('span', { class: 'spacer' }), select),
    h('p', { class: 'sub' }, 'Node health, failures, deploys and rollouts, scaling and operator actions; newest first.'),
    eventsTable(events));
}

const EXAMPLE = `[service]
id       = "frontend-web"
image    = "docker.io/library/nginx:1.27.2-alpine-slim"
replicas = 3

[resources]
cpu    = 0.5
memory = "512MiB"

[[ports]]
name           = "http"
container_port = 80
host_port      = 8080
`;

async function deployPage() {
  const text = h('textarea', { spellcheck: 'false', 'aria-label': 'Manifest (TOML)', placeholder: EXAMPLE });
  const file = h('input', { type: 'file', accept: '.toml,text/plain', 'aria-label': 'Upload a manifest' });
  const out = h('div');
  const applyBtn = h('button', { type: 'button', disabled: true }, 'Apply');
  let planned = null;
  text.addEventListener('input', () => { applyBtn.disabled = text.value !== planned; });
  file.addEventListener('change', async () => {
    if (!file.files[0]) return;
    text.value = await file.files[0].text();
    applyBtn.disabled = true;
  });
  const showPlan = (r, applied) => {
    const header = r.change === 'unchanged' ? 'unchanged' : r.change === 'replacing'
      ? `version ${r.version - 1} → ${r.version} · rolling update, max_parallel=${r.max_parallel}`
      : `version ${r.version}`;
    out.replaceChildren(h('div', { class: `msg ${applied ? 'ok' : r.placeable < r.to_place ? 'warn' : 'ok'}` },
      h('strong', {}, applied ? 'Applied: ' : 'Plan: '), badge(r.change), ` service ${r.service_id} · ${header}`,
      h('ul', {},
        r.diff.map((d) => h('li', { class: 'mono' }, d)),
        r.to_place ? h('li', {}, `placement: ${r.placeable}/${r.to_place} workloads placeable`) : null,
        r.warnings.map((w) => h('li', {}, `warning: ${w}`))),
      applied ? h('p', {}, link(`/ui/services/${enc(r.service_id)}`, `Open ${r.service_id}`)) : null));
  };
  const planBtn = h('button', {
    type: 'button',
    class: 'secondary',
    onclick: async () => {
      try {
        const r = await api('plan', { method: 'POST', body: { manifest: text.value } });
        planned = text.value;
        applyBtn.disabled = false;
        showPlan(r, false);
      } catch (err) { planned = null; applyBtn.disabled = true; out.replaceChildren(errorBox(err)); }
    },
  }, 'Plan');
  applyBtn.addEventListener('click', async () => {
    try {
      const r = await api('apply', { method: 'POST', body: { manifest: text.value } });
      applyBtn.disabled = true;
      planned = null;
      showPlan(r, true);
    } catch (err) { out.replaceChildren(errorBox(err)); }
  });
  return h('div', {},
    h('h1', {}, 'Deploy'),
    h('p', { class: 'sub' }, 'Paste or upload a service manifest, run Plan to see the diff and placement check, then Apply.'),
    text,
    h('div', { class: 'row' }, file, h('span', { class: 'spacer' }), planBtn, applyBtn),
    out);
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

(async () => {
  try {
    const s = await api('session');
    if (s.authenticated) signedIn(s.csrf); else signedOut();
  } catch (e) {
    main.replaceChildren(errorBox(e));
  }
})();
