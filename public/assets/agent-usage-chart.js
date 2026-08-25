import { escapeHtml, formatTimestamp } from './format.js';

const PALETTE = ['#0070f3', '#50e3c2', '#7928ca', '#ff0080', '#f5a623', '#eb367f', '#00a8a8', '#ff4d4d', '#8f8f8f'];
const PAD = { l: 64, r: 18, t: 22, b: 34 };
let data = null;
let phase = 'loading';
let message = '';
let hoverIndex = null;
let onChange = null;

const $ = id => document.getElementById(id);
const css = (name, fallback) => getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;

export function setupAgentUsageChart(changeHandler) {
  onChange = changeHandler;
  const canvas = $('agentUsageChart');
  canvas?.addEventListener('mousemove', onMove);
  canvas?.addEventListener('mouseleave', () => { hoverIndex = null; hideTip(); drawAgentUsageChart(); });
  $('agentUsageRange').onchange = notifyChange;
  $('agentUsageAgent').onchange = notifyChange;
  document.querySelectorAll('[data-agent-group]').forEach(button => {
    button.onclick = () => {
      document.querySelectorAll('[data-agent-group]').forEach(item => item.classList.toggle('active', item === button));
      notifyChange();
    };
  });
}

export function agentUsageSelection() {
  return {
    range: $('agentUsageRange')?.value || '7d',
    group: document.querySelector('[data-agent-group].active')?.dataset.agentGroup || 'harness',
    agent: $('agentUsageAgent')?.value || 'all',
    top: 8,
  };
}

export function setAgentUsageLoading() {
  phase = 'loading'; message = 'Loading agent usage…'; render();
}

export function setAgentUsageError(error) {
  phase = 'error'; message = error?.message || String(error); render();
}

export function setAgentUsageData(next) {
  data = next;
  phase = (next?.points?.length || 0) <= 2 && !next?.summary?.responses ? 'empty' : 'ready';
  message = '';
  syncControls();
  render();
}

function notifyChange() {
  setAgentUsageLoading();
  onChange?.(agentUsageSelection());
}

function syncControls() {
  const select = $('agentUsageAgent');
  const selected = select.value;
  const agents = uniqueAgents(data?.agents || []);
  select.innerHTML = '<option value="all">All harnesses</option>' + agents.map(agent => `<option value="${escapeHtml(agent.type)}">${escapeHtml(agent.displayName)}</option>`).join('');
  select.value = [...select.options].some(option => option.value === selected) ? selected : (data?.agentFilter || 'all');
}

function uniqueAgents(agents) {
  const seen = new Set();
  return agents.filter(agent => !seen.has(agent.type) && seen.add(agent.type));
}

function render() {
  renderStatus();
  renderSummary();
  renderLegend();
  renderNotes();
  drawAgentUsageChart();
}

function renderStatus() {
  const status = $('agentUsageStatus');
  if (phase === 'error') { status.textContent = `Agent telemetry unavailable: ${message}`; status.className = 'agent-state error'; return; }
  if (phase === 'loading') { status.textContent = message; status.className = 'agent-state'; return; }
  const agents = data?.agents || [];
  if (!agents.length) { status.textContent = 'No agent sources discovered.'; status.className = 'agent-state'; return; }
  const indexing = agents.some(agent => !agent.indexer?.initialScanComplete);
  const errors = agents.filter(agent => agent.indexer?.error);
  if (indexing) status.textContent = 'Indexing agent sessions… totals update in the background.';
  else if (errors.length) status.textContent = `Partial data · ${errors.length} source${errors.length === 1 ? '' : 's'} reported an indexing error.`;
  else {
    const freshest = Math.max(0, ...agents.map(agent => Number(agent.indexer?.lastScanTs || 0)));
    status.textContent = freshest ? `Updated ${formatTimestamp(freshest, { date: true })}` : 'Waiting for the first index scan…';
  }
  status.className = errors.length ? 'agent-state warning' : 'agent-state';
}

function renderSummary() {
  const summary = data?.summary;
  if (!summary || phase === 'error') { $('agentUsageSummary').innerHTML = ''; return; }
  const coverage = data.coverage || {};
  const costNote = coverage.costEvents === 0 ? 'unavailable' : coverage.costComplete ? summary.costKind || 'available' : `${coverage.costEvents}/${coverage.tokenEvents} responses`;
  const items = [
    ['Total tokens', compactTokens(summary.totalTokens), tokenBreakdown(summary)],
    ['Cost', formatCost(summary.totalCost, summary.costCurrency), costNote],
    ['Responses', Number(summary.responses || 0).toLocaleString(), 'canonical events'],
    ['Cache hit rate', summary.cacheHitRate == null ? '—' : `${(summary.cacheHitRate * 100).toFixed(1)}%`, coverage.cacheEvents ? `${coverage.cacheEvents} responses covered` : 'unavailable'],
  ];
  $('agentUsageSummary').innerHTML = items.map(([label, value, note]) => `<div><span>${escapeHtml(label)}</span><b>${escapeHtml(value)}</b><small>${escapeHtml(note)}</small></div>`).join('');
}

function tokenBreakdown(summary) {
  const pieces = [];
  if (summary.inputTokens != null) pieces.push(`${compactTokens(summary.inputTokens)} input`);
  if (summary.outputTokens != null) pieces.push(`${compactTokens(summary.outputTokens)} output`);
  if (summary.cacheReadTokens != null) pieces.push(`${compactTokens(summary.cacheReadTokens)} cache read`);
  return pieces.join(' · ') || 'component breakdown unavailable';
}

function renderLegend() {
  if (!data || phase === 'error') { $('agentUsageLegend').innerHTML = ''; return; }
  const groups = data.groups || [];
  const hasOther = (data.points || []).some(point => point.groups?.__other__);
  const rows = groups.map(group => ({ ...group, color: colorFor(group.id) }));
  if (hasOther) rows.push({ id: '__other__', label: 'Other', badge: groupBadge(), color: css('--chart-muted', '#8f8f8f') });
  $('agentUsageLegend').innerHTML = rows.map(row => `<span class="agent-legend-item"><i style="background:${row.color}"></i><em>${escapeHtml(row.badge)}</em>${escapeHtml(row.label)}</span>`).join('');
}

function renderNotes() {
  if (!data) return;
  const limitations = uniqueAgents(data.agents || []).flatMap(agent => (agent.limitations || []).map(note => `${agent.displayName}: ${note}`));
  const currencies = data.coverage?.currencies || [];
  if (currencies.some(currency => currency !== 'USD')) limitations.unshift(`Only USD cost is aggregated; available currencies: ${currencies.join(', ')}.`);
  if (data.coverage?.costEvents > 0 && !data.coverage?.costComplete) limitations.unshift('Cost coverage is partial; token totals remain complete for indexed events.');
  $('agentUsageNotes').innerHTML = limitations.map(note => `<p>${escapeHtml(note)}</p>`).join('');
}

export function drawAgentUsageChart() {
  const canvas = $('agentUsageChart');
  if (!canvas) return;
  const rect = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.max(1, Math.round(rect.width * dpr));
  canvas.height = Math.max(1, Math.round(rect.height * dpr));
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, rect.width, rect.height);
  ctx.fillStyle = css('--chart-bg', '#fff'); ctx.fillRect(0, 0, rect.width, rect.height);
  const state = $('agentUsageChartState');
  if (phase !== 'ready') {
    state.hidden = false;
    state.textContent = phase === 'error' ? `Could not load agent usage: ${message}` : phase === 'empty' ? emptyMessage() : message || 'Loading agent usage…';
    return;
  }
  state.hidden = true;
  const points = data.points || [];
  if (points.length < 2) { state.hidden = false; state.textContent = emptyMessage(); return; }
  const definitions = stackDefinitions(points);
  const maxTokens = Math.max(1, ...points.map(point => Number(point.totalTokens || 0)));
  const plotW = Math.max(1, rect.width - PAD.l - PAD.r);
  const plotH = Math.max(1, rect.height - PAD.t - PAD.b);
  const x = index => PAD.l + index / Math.max(1, points.length - 1) * plotW;
  const y = value => PAD.t + plotH - value / maxTokens * plotH;
  drawGrid(ctx, rect, maxTokens);
  let base = new Array(points.length).fill(0);
  for (const group of definitions) {
    const top = points.map((point, index) => base[index] + groupValue(point, group.id));
    ctx.beginPath();
    top.forEach((value, index) => index ? ctx.lineTo(x(index), y(value)) : ctx.moveTo(x(index), y(value)));
    for (let index = points.length - 1; index >= 0; index--) ctx.lineTo(x(index), y(base[index]));
    ctx.closePath(); ctx.fillStyle = withAlpha(group.color, .72); ctx.fill();
    base = top;
  }
  drawTimeLabels(ctx, points, x, rect.height);
  if (hoverIndex != null) {
    ctx.beginPath(); ctx.moveTo(x(hoverIndex), PAD.t); ctx.lineTo(x(hoverIndex), rect.height - PAD.b);
    ctx.strokeStyle = css('--accent', '#0070f3'); ctx.setLineDash([3, 3]); ctx.stroke(); ctx.setLineDash([]);
  }
}

function drawGrid(ctx, rect, maxTokens) {
  const plotH = rect.height - PAD.t - PAD.b;
  ctx.font = '11px ui-monospace, monospace'; ctx.textBaseline = 'middle';
  for (let i = 0; i <= 4; i++) {
    const yy = PAD.t + plotH - i / 4 * plotH;
    ctx.beginPath(); ctx.moveTo(PAD.l, yy); ctx.lineTo(rect.width - PAD.r, yy);
    ctx.strokeStyle = css('--chart-grid', '#ebebeb'); ctx.lineWidth = 1; ctx.stroke();
    ctx.fillStyle = css('--chart-muted', '#8f8f8f'); ctx.textAlign = 'right';
    ctx.fillText(compactTokens(maxTokens * i / 4), PAD.l - 8, yy);
  }
  ctx.textAlign = 'left'; ctx.textBaseline = 'alphabetic'; ctx.fillStyle = css('--chart-body', '#4d4d4d');
  ctx.fillText('cumulative tokens', PAD.l, 13);
}

function drawTimeLabels(ctx, points, x, height) {
  const indices = [0, Math.floor((points.length - 1) / 2), points.length - 1];
  ctx.font = '11px system-ui'; ctx.fillStyle = css('--chart-muted', '#8f8f8f'); ctx.textBaseline = 'alphabetic';
  indices.forEach((index, position) => {
    const label = formatTimestamp(points[index].ts, { date: true }).replace(/:\d\d\s/, ' ');
    ctx.textAlign = position === 0 ? 'left' : position === 2 ? 'right' : 'center';
    ctx.fillText(label, x(index), height - 9);
  });
}

function stackDefinitions(points) {
  const definitions = (data.groups || []).map(group => ({ id: group.id, color: colorFor(group.id), group }));
  if (points.some(point => point.groups?.__other__)) definitions.push({ id: '__other__', color: css('--chart-muted', '#8f8f8f'), group: { label: 'Other', badge: groupBadge() } });
  return definitions;
}

function groupBadge() {
  if (data?.groupBy === 'model') return 'Model';
  if (data?.groupBy === 'session') return 'Session';
  return 'Harness';
}

function groupValue(point, id) {
  return Number(point.groups?.[id]?.tokens) || 0;
}

function onMove(event) {
  if (phase !== 'ready' || !data?.points?.length) return;
  const canvas = $('agentUsageChart');
  const rect = canvas.getBoundingClientRect();
  const ratio = Math.max(0, Math.min(1, (event.clientX - rect.left - PAD.l) / Math.max(1, rect.width - PAD.l - PAD.r)));
  const target = data.fromTs + ratio * (data.toTs - data.fromTs);
  hoverIndex = nearestPoint(data.points, target);
  drawAgentUsageChart();
  showTip(event, data.points[hoverIndex]);
}

function nearestPoint(points, ts) {
  let lo = 0, hi = points.length - 1;
  while (lo < hi) { const mid = (lo + hi) >> 1; points[mid].ts < ts ? lo = mid + 1 : hi = mid; }
  return lo > 0 && Math.abs(points[lo - 1].ts - ts) < Math.abs(points[lo].ts - ts) ? lo - 1 : lo;
}

function showTip(event, point) {
  const tip = $('agentUsageTip');
  const definitions = stackDefinitions(data.points)
    .map(def => ({ ...def, tokens: groupValue(point, def.id) }))
    .filter(item => item.tokens > 0)
    .sort((a, b) => b.tokens - a.tokens);
  tip.innerHTML = `<div class="agent-tip-title">${escapeHtml(formatTimestamp(point.ts, { date: true }))}</div>
    <div class="agent-tip-totals"><b>${compactTokens(point.totalTokens)} tokens</b></div>
    ${definitions.map(item => `<div class="agent-tip-row"><span><i style="background:${item.color}"></i><em>${escapeHtml(item.group.badge)}</em>${escapeHtml(item.group.label)}</span><b>${compactTokens(item.tokens)}</b></div>`).join('')}`;
  tip.style.display = 'block';
  positionTipAtCursor(tip, event);
}

function positionTipAtCursor(tip, event) {
  const wrap = tip.parentElement;
  const rect = wrap.getBoundingClientRect();
  const cursorX = event.clientX - rect.left;
  const cursorY = event.clientY - rect.top;
  const width = tip.offsetWidth;
  const height = tip.offsetHeight;
  const gap = 14;
  let left = cursorX + gap;
  let top = cursorY + gap;
  if (left + width > rect.width - 8) left = cursorX - width - gap;
  if (top + height > rect.height - 8) top = cursorY - height - gap;
  tip.style.left = `${Math.max(8, Math.min(rect.width - width - 8, left))}px`;
  tip.style.top = `${Math.max(8, Math.min(rect.height - height - 8, top))}px`;
}

function hideTip() { const tip = $('agentUsageTip'); if (tip) tip.style.display = 'none'; }
function emptyMessage() { return (data?.agents || []).some(agent => !agent.indexer?.initialScanComplete) ? 'Indexing agent sessions…' : 'No agent usage in this range.'; }
function colorFor(value) { let hash = 2166136261; for (const char of String(value)) { hash ^= char.charCodeAt(0); hash = Math.imul(hash, 16777619); } return PALETTE[Math.abs(hash) % PALETTE.length]; }
function withAlpha(color, alpha) { if (/^#[0-9a-f]{6}$/i.test(color)) return color + Math.round(alpha * 255).toString(16).padStart(2, '0'); return color; }
function compactTokens(value) { const n = Number(value); if (!Number.isFinite(n)) return '—'; if (Math.abs(n) >= 1e9) return `${(n / 1e9).toFixed(2)}B`; if (Math.abs(n) >= 1e6) return `${(n / 1e6).toFixed(2)}M`; if (Math.abs(n) >= 1e3) return `${(n / 1e3).toFixed(1)}K`; return Math.round(n).toLocaleString(); }
function formatCost(value, currency = 'USD') { if (value == null || !Number.isFinite(Number(value))) return '—'; return new Intl.NumberFormat(undefined, { style: 'currency', currency: currency || 'USD', maximumFractionDigits: Number(value) < 1 ? 4 : 2 }).format(Number(value)); }
