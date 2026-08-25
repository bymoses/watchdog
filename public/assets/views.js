import { colors, drawCharts } from './charts.js';
import { escapeAttr, escapeHtml, fmtDuration, fmtPct, fmtTime, fmtW } from './format.js';

const expandedGroups = new Set();
let groupsData = null;

// Advisory copy only. Options themselves always come from the kernel, so an
// unknown value still renders correctly, just without a description.
const CHARGE_TYPE_NOTES = {
  Fast: 'Charges to full at the highest rate',
  Standard: 'Charges to full at a normal rate',
  Long_Life: 'Conservation mode · holds near 80% to reduce wear',
};

let chargeControl = null;
let chargePending = null;

export function renderStatus(status) {
  renderChargeControl(status.chargeControl);
  const b = status.latestBattery;
  setText('battery', b ? fmtPct(b.capacity) : '—');
  renderBatteryStatus(b);
  setText('watts', b ? fmtW(b.power_w) : '—');
  updateGauge(b);

  const d = status.dischargeEstimate;
  setText('rateLabel', d?.mode === 'charging' ? 'Charge rate' : d?.mode === 'discharging' ? 'Drain rate' : 'Battery rate');
  setText('drain', d?.percentPerHour ? (d.mode === 'charging' ? '+' : '-') + d.percentPerHour.toFixed(2) + '%/h' : '—');
  setText('lasts', d?.hoursToFull ? 'full ~' + fmtDuration(d.hoursToFull) + ' / ' + d.detail : d?.hoursRemaining ? 'lasts ~' + fmtDuration(d.hoursRemaining) + ' / ' + d.detail : (d?.detail || 'estimating'));

  setText('poll', b ? fmtTime(b.ts) : '—');
  const db = status.dbStats;
  setText('samples', db ? formatDbStats(db) : '—');

  const e = status.latestEnvironment;
  const screenState = e ? [e.screen_locked ? 'locked' : '', e.lid_closed ? 'lid closed' : ''].filter(Boolean).join(' / ') : '';
  setText('screen', e ? (e.brightness_percent == null ? 'brightness ?' : fmtPct(e.brightness_percent)) : '—');
  setText('theme', e ? ((e.theme || 'unknown') + ' theme / ' + (e.brightness_source || 'no backlight') + (screenState ? ' / ' + screenState : '')) : '—');
  const usbPower = e?.usb_power_source == null ? '' : e.usb_power_source ? ' / USB source' : ' / USB sink';
  const usbWatts = e?.usb_power_w == null ? '' : ' ' + Number(e.usb_power_w).toFixed(2) + ' W';
  setText('media', e ? ((e.audio_playing ? 'audio' : 'silent') + ' / ' + (e.video_streaming ? 'video?' : 'no video') + usbPower) : '—');
  setText('network', e ? ('RX ' + (e.net_rx_mbps || 0).toFixed(2) + ' Mbps / TX ' + (e.net_tx_mbps || 0).toFixed(2) + usbWatts) : '—');
  setText('focusApp', e?.focused_app || '—');
  setText('focusTitle', e?.focused_title || 'optional niri helper not running');
}

export function setupChargeControl(onSelect) {
  const toggle = document.getElementById('chargeTypeToggle');
  if (!toggle) return;
  toggle.addEventListener('click', event => {
    const button = event.target.closest('button[data-charge-type]');
    if (!button || button.disabled) return;
    onSelect(button.dataset.chargeType);
  });
}

// `force` marks a payload that came straight back from a write, which is the
// only kind allowed to clear a pending state. Background polls must not, or a
// 30s refresh landing mid-write would snap the toggle back to the stale value.
export function renderChargeControl(control, { force = false } = {}) {
  const card = document.getElementById('chargeControl');
  if (!card) return;
  if (!control || !control.supported) {
    card.hidden = true;
    chargeControl = null;
    return;
  }
  if (chargePending && !force && control.current !== chargePending) return;
  if (force || control.current === chargePending) chargePending = null;
  card.hidden = false;
  chargeControl = control;
  paintChargeControl();
}

export function setChargeControlPending(value) {
  chargePending = value;
  paintChargeControl();
}

export function setChargeControlError(message) {
  chargePending = null;
  paintChargeControl({ keepNote: true });
  setNote(message || 'Could not change charge type', 'is-error');
}

function paintChargeControl({ keepNote = false } = {}) {
  const toggle = document.getElementById('chargeTypeToggle');
  if (!chargeControl || !toggle) return;
  const locked = !chargeControl.writable;
  toggle.innerHTML = (chargeControl.options || []).map(option => {
    const active = chargePending ? option === chargePending : option === chargeControl.current;
    return '<button type="button" data-charge-type="' + escapeAttr(option) + '"' +
      (active ? ' class="active"' : '') +
      (locked || chargePending ? ' disabled' : '') +
      ' aria-pressed="' + (active ? 'true' : 'false') + '"' +
      ' title="' + escapeAttr(CHARGE_TYPE_NOTES[option] || option) + '">' +
      escapeHtml(chargeTypeLabel(option)) + '</button>';
  }).join('');
  if (keepNote) return;
  if (chargePending) setNote('Applying ' + chargeTypeLabel(chargePending) + '\u2026', '');
  else if (locked) setNote(chargeControl.reason || 'Read-only on this host', 'is-warn');
  else setNote(CHARGE_TYPE_NOTES[chargeControl.current] || chargeTypeLabel(chargeControl.current) || 'unknown', '');
}

function setNote(text, kind) {
  const note = document.getElementById('chargeControlNote');
  if (!note) return;
  note.textContent = text;
  note.classList.remove('is-error', 'is-warn');
  if (kind) note.classList.add(kind);
}

function chargeTypeLabel(value) {
  return String(value || '').replace(/_/g, ' ');
}

export function renderLegend(apps) {
  document.getElementById('legend').innerHTML = (apps || []).map((a,i) => '<span class="pill"><span class="swatch" style="background:' + colors[i % colors.length] + '"></span>' + escapeHtml(a) + '</span>').join('');
}

export function renderProcessTable(rows) {
  document.getElementById('rows').innerHTML = (rows || []).map(r => '<tr>' +
    '<td>' + escapeHtml(r.app) + (r.is_self ? ' ⭐' : '') + '</td>' +
    '<td>' + r.pid + '</td>' +
    '<td>' + fmtW(r.estimated_watts) + '</td>' +
    '<td>' + (r.cpu_percent || 0).toFixed(1) + '</td>' +
    '<td>' + (r.io_mb || 0).toFixed(2) + '</td>' +
    '<td>' + (r.rss_mb || 0).toFixed(0) + '</td>' +
    '<td class="muted" title="' + escapeAttr(r.cmd || '') + '">' + escapeHtml((r.cmd || '').slice(0, 120)) + '</td>' +
    '</tr>').join('');
}

export function renderGroups(data) {
  groupsData = data;
  setText('groupHours', data.hours || document.getElementById('hours').value);
  const groups = data.groups || [];
  const max = Math.max(0.001, ...groups.map(g => g.avgWatts || 0));
  const rows = [];
  for (const g of groups) {
    const open = expandedGroups.has(g.app);
    rows.push('<tr class="expand" data-group="' + escapeAttr(g.app) + '">' +
      '<td>' + (open ? '▾ ' : '▸ ') + '<b>' + escapeHtml(g.app) + '</b></td>' +
      '<td>' + fmtW(g.avgWatts) + '</td>' +
      '<td>' + (g.wh || 0).toFixed(3) + '</td>' +
      '<td>' + (g.cpuSeconds || 0).toFixed(1) + '</td>' +
      '<td>' + (g.ioMb || 0).toFixed(1) + '</td>' +
      '<td>' + (g.rssMb || 0).toFixed(0) + '</td>' +
      '<td><div class="bar"><span style="width:' + Math.min(100, (g.avgWatts || 0) / max * 100).toFixed(1) + '%"></span></div></td>' +
      '</tr>');
    if (open) addChildren(rows, g);
  }
  document.getElementById('groupRows').innerHTML = rows.join('') || '<tr><td colspan="7" class="muted">No process samples yet.</td></tr>';
  document.querySelectorAll('#groupRows tr[data-group]').forEach(tr => tr.onclick = () => {
    const g = tr.getAttribute('data-group');
    expandedGroups.has(g) ? expandedGroups.delete(g) : expandedGroups.add(g);
    renderGroups(groupsData);
  });
}

export function setupTabs() {
  document.querySelectorAll('.tab').forEach(btn => btn.onclick = () => {
    document.querySelectorAll('.tab').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    document.querySelectorAll('#latestPanel,#groupsPanel').forEach(p => p.classList.add('hidden'));
    document.getElementById(btn.dataset.tab).classList.remove('hidden');
    drawCharts();
  });
}

function addChildren(rows, group) {
  const total = Math.max(0.001, group.wattSamples || 0);
  for (const c of (group.children || []).slice(0, 40)) {
    rows.push('<tr class="child">' +
      '<td title="' + escapeAttr(c.cmd || '') + '">' + escapeHtml(c.name) + '</td>' +
      '<td>' + fmtW(c.avgWatts) + '</td>' +
      '<td>' + (c.wh || 0).toFixed(3) + '</td>' +
      '<td>' + (c.cpuSeconds || 0).toFixed(1) + '</td>' +
      '<td>' + (c.ioMb || 0).toFixed(1) + '</td>' +
      '<td>' + (c.rssMb || 0).toFixed(0) + '</td>' +
      '<td>' + (((c.wattSamples || 0) / total) * 100).toFixed(1) + '%</td>' +
      '</tr>');
  }
}

function renderBatteryStatus(b) {
  const el = document.getElementById('status');
  if (!el) return;

  if (!b) {
    el.title = 'no sample yet';
    el.setAttribute('aria-label', 'no sample yet');
    el.innerHTML = statusIcon('unknown', 'no sample yet');
    return;
  }

  const powerLabel = b.on_battery ? 'unplugged' : 'plugged';
  const statusLabels = String(b.status || 'unknown').split(',').map(s => s.trim()).filter(Boolean);
  const labels = statusLabels.length ? statusLabels : ['unknown'];
  const fullLabel = powerLabel + ' / ' + labels.join(', ');
  el.title = fullLabel;
  el.setAttribute('aria-label', fullLabel);
  el.innerHTML = [
    statusIcon(b.on_battery ? 'unplugged' : 'plugged', powerLabel),
    ...labels.map(label => statusIcon(batteryStatusKind(label), label)),
  ].join('');
}

function batteryStatusKind(label) {
  const s = String(label || '').toLowerCase();
  if (s.includes('not charging')) return 'not-charging';
  if (s.includes('discharging')) return 'discharging';
  if (s.includes('charging')) return 'charging';
  if (s.includes('full')) return 'full';
  if (s.includes('ac')) return 'plugged';
  return 'unknown';
}

function statusIcon(kind, label) {
  return '<span class="battery-status-ico is-' + kind + '" role="img" title="' + escapeAttr(label) + '" aria-label="' + escapeAttr(label) + '"></span>';
}

function updateGauge(b) {
  const el = document.getElementById('batteryGauge');
  if (!el) return;
  const pct = b && b.capacity != null ? Math.max(0, Math.min(100, b.capacity)) : 0;
  const color = !b ? '#8f8f8f' : pct <= 15 ? '#ee0000' : b.on_battery ? '#f5a623' : '#0070f3';
  el.style.setProperty('--pct', String(pct));
  el.style.setProperty('--gauge', color);
}

function formatDbStats(db) {
  const days = db.spanDays == null ? '— days' : db.spanDays < 1 ? '<1 day' : db.spanDays.toFixed(db.spanDays < 10 ? 1 : 0) + ' days';
  return days + ' · ' + formatBytes(db.sizeBytes || 0);
}

function formatBytes(bytes) {
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = Number(bytes) || 0;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) { value /= 1024; i++; }
  return value.toFixed(i === 0 ? 0 : value < 10 ? 1 : 0) + ' ' + units[i];
}

function setText(id, value) {
  document.getElementById(id).textContent = value;
}
