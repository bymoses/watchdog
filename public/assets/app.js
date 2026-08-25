import { fetchAgentUsage, fetchGroups, fetchProcesses, fetchSeries, fetchStatus, setChargeType } from './api.js';
import { agentUsageSelection, drawAgentUsageChart, setAgentUsageData, setAgentUsageError, setAgentUsageLoading, setupAgentUsageChart } from './agent-usage-chart.js';
import { drawCharts, setSeries, setupTimelineHover } from './charts.js';
import { setupContextDashboard } from './context-dashboard.js';
import { getTimezoneOffsetHours, setTimezoneOffsetHours, timezoneLabel } from './format.js';
import { renderChargeControl, renderGroups, renderLegend, renderProcessTable, renderStatus, setChargeControlError, setChargeControlPending, setupChargeControl, setupTabs } from './views.js';

let currentSeries = null;
let currentKey = '';
let lastHeavyPanelsAt = 0;
let requestSeq = 0;
let agentRequestSeq = 0;

async function refresh({ full = false } = {}) {
  const requestId = ++requestSeq;
  const hours = document.getElementById('hours').value;
  const top = document.getElementById('top').value;
  const key = hours + ':' + top;
  if (key !== currentKey) {
    full = true;
    currentKey = key;
  }

  const afterTs = !full && currentSeries?.points?.length ? currentSeries.points.at(-1).ts : null;
  const shouldRefreshHeavyPanels = full || !currentSeries || Date.now() - lastHeavyPanelsAt > 5 * 60 * 1000;
  const tasks = [];

  tasks.push(fetchStatus()
    .then(status => {
      if (isStale(requestId)) return;
      renderStatus(status);
    }));

  tasks.push(fetchSeries({ hours, top, afterTs })
    .then(nextSeries => {
      if (isStale(requestId)) return;
      currentSeries = mergeSeries(currentSeries, nextSeries, { hours, full });
      setSeries(currentSeries);
      drawCharts();
      renderLegend(currentSeries.apps || []);
    }));

  if (shouldRefreshHeavyPanels) {
    setHeavyLoadingState();

    tasks.push(fetchProcesses()
      .then(latest => {
        if (isStale(requestId)) return;
        renderProcessTable(latest.rows || []);
      }));

    tasks.push(fetchGroups({ hours })
      .then(groups => {
        if (isStale(requestId)) return;
        renderGroups(groups);
        lastHeavyPanelsAt = Date.now();
      }));
  }

  const results = await Promise.allSettled(tasks);
  const rejected = results.find(r => r.status === 'rejected');
  if (rejected) throw rejected.reason;
}

async function applyChargeType(value) {
  setChargeControlPending(value);
  try {
    renderChargeControl(await setChargeType(value), { force: true });
  } catch (error) {
    // Surface the server's reason, then resync so the toggle reflects the
    // kernel rather than the failed intent.
    setChargeControlError(error.message);
    refresh().catch(console.error);
  }
}

function setHeavyLoadingState() {
  document.getElementById('rows').innerHTML = '<tr><td colspan="7" class="muted">Loading latest processes…</td></tr>';
  document.getElementById('groupRows').innerHTML = '<tr><td colspan="7" class="muted">Loading groups…</td></tr>';
}

function mergeSeries(existing, incoming, { hours, full }) {
  if (full || !existing || !incoming.incremental) return incoming;
  if (!sameList(existing.apps || [], incoming.apps || [])) {
    // Top app set changed; request a full window so historical stacks stay correct.
    setTimeout(() => refresh({ full: true }).catch(console.error), 0);
  }
  const cutoff = Date.now() - Number(hours) * 60 * 60 * 1000;
  const pointsByTs = new Map((existing.points || []).filter(p => p.ts >= cutoff).map(p => [p.ts, p]));
  for (const p of incoming.points || []) pointsByTs.set(p.ts, p);
  const sleepByKey = new Map((existing.sleepEvents || []).filter(e => Number(e.end_ts) >= cutoff).map(e => [Number(e.start_ts) + ':' + Number(e.end_ts), e]));
  for (const e of incoming.sleepEvents || []) sleepByKey.set(Number(e.start_ts) + ':' + Number(e.end_ts), e);
  return {
    ...incoming,
    apps: incoming.apps || existing.apps || [],
    points: [...pointsByTs.values()].sort((a, b) => a.ts - b.ts),
    sleepEvents: [...sleepByKey.values()].sort((a, b) => Number(a.start_ts) - Number(b.start_ts)),
  };
}

function sameList(a, b) {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

function isStale(requestId) {
  return requestId !== requestSeq;
}

async function refreshAgentUsage(selection = agentUsageSelection(), { loading = false } = {}) {
  const requestId = ++agentRequestSeq;
  if (loading) setAgentUsageLoading();
  try {
    const usage = await fetchAgentUsage(selection);
    if (requestId === agentRequestSeq) setAgentUsageData(usage);
  } catch (error) {
    if (requestId === agentRequestSeq) setAgentUsageError(error);
  }
}

function boot() {
  setupTimezoneSelector();
  setupTabs();
  setupTimelineHover();
  setupContextDashboard();
  setupChargeControl(value => applyChargeType(value));
  setupAgentUsageChart(selection => refreshAgentUsage(selection));
  window.addEventListener('resize', () => { drawCharts(); drawAgentUsageChart(); });
  window.matchMedia?.('(prefers-color-scheme: dark)').addEventListener?.('change', () => { drawCharts(); drawAgentUsageChart(); });
  document.getElementById('refresh').onclick = () => refresh({ full: true }).catch(console.error);
  document.getElementById('hours').onchange = () => refresh({ full: true }).catch(console.error);
  document.getElementById('top').onchange = () => refresh({ full: true }).catch(console.error);
  document.getElementById('timezone').onchange = (event) => {
    setTimezoneOffsetHours(event.target.value);
    renderExistingWithNewTimezone();
  };
  setInterval(() => refresh().catch(console.error), 30000);
  setInterval(() => refreshAgentUsage(), 30000);
  refreshAgentUsage(agentUsageSelection(), { loading: true });
  refresh({ full: true }).catch(e => { console.error(e); alert(e.message); });
}

function setupTimezoneSelector() {
  const select = document.getElementById('timezone');
  const offsets = ['local', ...Array.from({ length: 27 }, (_, i) => i - 12)];
  select.innerHTML = offsets.map(offset => {
    const value = offset === 'local' ? 'NaN' : String(offset);
    return '<option value="' + value + '">' + (offset === 'local' ? 'Local' : timezoneLabel(offset)) + '</option>';
  }).join('');
  select.value = String(getTimezoneOffsetHours());
}

function renderExistingWithNewTimezone() {
  if (currentSeries) {
    setSeries(currentSeries);
    drawCharts();
  }
  refresh().catch(console.error);
}

boot();
