import {
  addEntityAlias,
  archiveContextEntity,
  createBrowserPairingCode,
  createContextRule,
  createManualContextLabel,
  deleteContextData,
  deleteContextEntity,
  deleteContextRule,
  deleteManualContextLabel,
  fetchActivityEpisodes,
  fetchActivitySummary,
  fetchActivityTimeline,
  fetchAttentionPolicies,
  fetchBrowserIntegration,
  fetchContextEntities,
  fetchContextHealth,
  fetchContextLatest,
  fetchContextRules,
  fetchContextStorage,
  fetchContextUnknown,
  fetchInterventions,
  fetchManualContextLabels,
  mergeContextEntities,
  previewContextRule,
  requestContextExport,
  revokeBrowserClient,
  runPolicyAction,
  saveAttentionPolicy,
  splitContextEntity,
  startContextRelabel,
} from './api.js';
import { escapeAttr, escapeHtml, fmtDateTime } from './format.js';

const METRICS = {
  focus: { label: 'Focused app', detail: 'OS-observed foreground time; gaps are not filled.' },
  focused_playback: { label: 'Focused playback', detail: 'Visible media while its browser is verified in focus.' },
  media_playback: { label: 'Media playback', detail: 'Playing media, including background playback.' },
  agent_busy: { label: 'Agent busy', detail: 'Exact or inferred coding-agent busy leases.' },
};
const POLICY_METRICS = {
  focused_playback_seconds: METRICS.focused_playback,
  media_playback_seconds: METRICS.media_playback,
};
const ACTIVITY_TONES = ['blue', 'teal', 'violet', 'amber', 'rose', 'gray'];
const state = {
  range: defaultRange(7),
  metric: 'focus',
  group: 'activity',
  rules: [],
  entities: [],
  policies: [],
  unknown: [],
  manualLabels: [],
  requestGeneration: 0,
};

export function setupContextDashboard() {
  const root = typeof document === 'undefined' ? null : document.getElementById('contextDashboard');
  if (!root) return;
  root.innerHTML = dashboardMarkup();
  bindNavigation(root);
  bindAnalyticsControls(root);
  bindUnknownReview(root);
  bindRuleEditor(root);
  bindEntityControls(root);
  bindBrowserControls(root);
  bindPolicyControls(root);
  bindDataControls(root);
  refreshDashboard();
  window.setInterval(() => refreshOverview(), 60_000);
}

export function defaultRange(days = 7, now = Date.now()) {
  const end = new Date(now);
  const start = new Date(now - days * 86_400_000);
  return { from: start.getTime(), to: end.getTime() };
}

export function formatContextDuration(seconds) {
  const value = Math.max(0, Number(seconds) || 0);
  if (value < 60) return `${Math.round(value)}s`;
  if (value < 3600) return `${Math.floor(value / 60)}m`;
  const totalMinutes = Math.round(value / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  return minutes ? `${hours}h ${minutes}m` : `${hours}h`;
}

export function normalizeActivitySummary(payload = {}) {
  const rows = listFrom(payload, ['groups', 'items', 'rows', 'data']);
  const items = rows.map((row, index) => ({
    key: String(row.key ?? row.value ?? row.name ?? row.label ?? `item-${index}`),
    label: String(row.label ?? row.displayName ?? row.name ?? row.value ?? 'Unknown'),
    seconds: secondsFrom(row, ['durationSeconds', 'observedSeconds', 'seconds', 'duration'], ['durationMs', 'observedMs']),
    confidence: confidenceFrom(row),
    episodes: numberFrom(row, ['episodeCount', 'episodes', 'count']) || 0,
  }));
  const meta = payload.meta || payload.summary || payload.coverage || payload;
  return {
    items,
    observedSeconds: secondsFrom(meta, ['metricDurationSeconds', 'observedDurationSeconds', 'observedSeconds', 'durationSeconds'], ['metricDurationMs', 'observedMs', 'observedDurationMs']) || items.reduce((sum, item) => sum + item.seconds, 0),
    unknownSeconds: secondsFrom(meta, ['unknownDurationSeconds', 'unknownSeconds'], ['unknownMs', 'unknownDurationMs']),
    coverage: normalizeRatio(numberFrom(meta, ['monitoringCoverage', 'coverage', 'coverageRatio'])),
    confidence: confidenceFrom(meta),
    revision: meta.rulesRevision ?? payload.rulesRevision ?? '—',
    metricDefinition: payload.metricDefinition || meta.metricDefinition || '',
  };
}

export function buildRulePayload(values) {
  return {
    name: String(values.name || '').trim(),
    priority: Number(values.priority) || 100,
    enabled: values.enabled !== false,
    origin: 'user',
    conditions: [{ fact: values.fact, operator: values.operator, value: String(values.conditionValue || '').trim() }],
    outputs: [{ dimension: values.dimension, value: String(values.outputValue || '').trim(), confidence: 1 }],
    stopDimensions: values.stopDimension ? [values.dimension] : [],
  };
}

function dashboardMarkup() {
  return `
    <div class="context-hero panel">
      <div>
        <span class="eyebrow">Plan 002 · local activity intelligence</span>
        <h2 id="contextDashboardTitle">Context & attention</h2>
        <p>Understand what was observed, how it was classified, and which protections are active—without collecting content.</p>
      </div>
      <div class="context-hero-actions">
        <span class="context-live"><i aria-hidden="true"></i><span id="contextLiveText">Checking sources…</span></span>
        <button class="context-button" type="button" data-context-refresh>Refresh context</button>
      </div>
    </div>

    <nav class="context-nav" aria-label="Context dashboard sections">
      ${navButton('overview', 'Overview', true)}
      ${navButton('review', 'Review & rules')}
      ${navButton('entities', 'Entities')}
      ${navButton('integrations', 'Integrations')}
      ${navButton('policies', 'Policies')}
      ${navButton('data', 'Data & health')}
    </nav>

    <div class="context-view" data-context-view="overview">
      <div class="context-grid context-grid--inspector">
        ${panel('Latest context', 'What the local collectors can currently explain.', 'contextLatest', 'context-card--wide')}
        ${panel('Source health', 'Each source fails independently.', 'contextHealth')}
      </div>
      <section class="panel context-panel" aria-labelledby="activitySummaryTitle">
        <div class="context-panel-head">
          <div><span class="eyebrow">Observed durations</span><h3 id="activitySummaryTitle">Activity summary</h3><p id="contextMetricHelp">${METRICS.focus.detail}</p></div>
          <div class="context-filters">
            <label>Range<select id="contextRange"><option value="1">Today</option><option value="7" selected>7 days</option><option value="30">30 days</option><option value="90">90 days</option></select></label>
            <label>Metric<select id="contextMetric">${Object.entries(METRICS).map(([value, item]) => `<option value="${value}">${item.label}</option>`).join('')}</select></label>
            <label>Group<select id="contextGroup"><option value="activity">Activity</option><option value="project">Project</option><option value="service">Service</option><option value="tool">Tool</option><option value="agentSession">Agent session</option></select></label>
          </div>
        </div>
        <div id="activityCoverage" class="context-stat-grid" aria-live="polite"></div>
        <div id="activitySummary" class="context-load-state" role="status">Loading summary…</div>
      </section>
      <div class="context-grid context-grid--analytics">
        ${panel('Timeline', 'Metric states remain distinct; unobserved gaps stay visible.', 'activityTimeline', 'context-card--wide')}
        ${panel('Entertainment episodes', 'Continuous episodes keep identity across midnight.', 'activityEpisodes')}
      </div>
    </div>

    <div class="context-view" data-context-view="review" hidden>
      <div class="context-grid context-grid--review">
        <section class="panel context-panel context-card--wide">
          <div class="context-panel-head"><div><span class="eyebrow">Manual correction</span><h3>Unknown review queue</h3><p>Longest unknown or low-confidence intervals appear first.</p></div><span class="context-badge">No content collected</span></div>
          <div id="unknownQueue" class="context-load-state" role="status">Loading unknown time…</div>
        </section>
        <section class="panel context-panel" id="correctionPanel">
          <div class="context-panel-head"><div><span class="eyebrow">Selected interval</span><h3>Correct classification</h3></div></div>
          <form id="manualLabelForm" class="context-form">
            <p id="correctionSelection" class="context-empty">Select an interval to review.</p>
            <label>Activity<select name="activity" required>${activityOptions()}</select></label>
            <label>Apply to<select name="scope"><option value="interval">This observed interval only</option></select></label>
            <p class="context-footnote">For recurring contexts, preview and create a rule instead of silently widening a correction.</p>
            <label>Private note (optional)<input name="note" maxlength="160" autocomplete="off" placeholder="Stored locally with the correction" /></label>
            <button class="context-button context-button--primary" type="submit" disabled>Save correction</button>
          </form>
          <hr class="context-separator" /><h4>Correction history</h4><div id="manualLabelHistory" class="context-load-state">Loading corrections…</div>
        </section>
      </div>
      <div class="context-grid context-grid--rules">
        <section class="panel context-panel context-card--wide">
          <div class="context-panel-head"><div><span class="eyebrow">Data-driven labeling</span><h3>Rules</h3><p>Rules change conclusions, never source observations.</p></div><button class="context-button" type="button" data-relabel>Relabel history</button></div>
          <div id="ruleList" class="context-load-state" role="status">Loading rules…</div>
          <div id="relabelProgress" class="context-progress" hidden><span style="width:0%"></span><b>Queued</b></div>
        </section>
        <section class="panel context-panel">
          <div class="context-panel-head"><div><span class="eyebrow">Impact preview</span><h3>Create a rule</h3></div></div>
          <form id="ruleForm" class="context-form">
            <label>Name<input name="name" maxlength="80" required placeholder="Research videos" /></label>
            <div class="context-form-row"><label>Fact<select name="fact"><option value="app.id">Application ID</option><option value="window.title">Window title</option><option value="terminal.command">Terminal tool</option><option value="project.entity">Project entity</option><option value="agent.type">Agent harness</option><option value="agent.state">Agent state</option></select></label><label>Match<select name="operator"><option value="exact">Exact</option><option value="caseInsensitiveExact">Exact, ignore case</option><option value="contains">Contains</option><option value="prefix">Prefix</option><option value="suffix">Suffix</option><option value="glob">Glob</option><option value="exists">Exists</option></select></label></div>
            <label>Value<input name="conditionValue" maxlength="160" required placeholder="youtube.com" /></label>
            <div class="context-form-row"><label>Output<select name="dimension"><option value="activity.primary">Primary activity</option><option value="activity.secondary">Secondary activity</option></select></label><label>As<input name="outputValue" maxlength="80" required placeholder="research" /></label></div>
            <label>Priority<input name="priority" type="number" min="0" max="10000" value="100" /></label>
            <label class="context-check"><input name="stopDimension" type="checkbox" checked /> Stop lower-priority rules for this dimension</label>
            <div id="ruleImpact" class="context-impact">Preview before saving to estimate affected history.</div>
            <div class="context-form-actions"><button class="context-button" type="button" data-rule-preview>Preview impact</button><button class="context-button context-button--primary" type="submit">Create rule</button></div>
          </form>
        </section>
      </div>
    </div>

    <div class="context-view" data-context-view="entities" hidden>
      <div class="context-grid context-grid--entities">
        <section class="panel context-panel context-card--wide">
          <div class="context-panel-head"><div><span class="eyebrow">Dynamic registry</span><h3>Projects, services & tools</h3><p>Aliases connect observations; merges preserve source facts.</p></div><label class="context-inline-label">Kind<select id="entityKind"><option value="">All kinds</option><option value="project">Projects</option><option value="repository">Repositories</option><option value="service">Services</option><option value="tool">Tools</option><option value="coding_session">Coding sessions</option></select></label></div>
          <div id="entityList" class="context-load-state" role="status">Loading entities…</div>
        </section>
        <section class="panel context-panel">
          <div class="context-panel-head"><div><span class="eyebrow">Resolve identity</span><h3>Alias or merge</h3></div></div>
          <form id="aliasForm" class="context-form"><label>Entity ID<input name="entityId" required inputmode="numeric" placeholder="Select an entity" /></label><label>Alias kind<select name="aliasKind"><option value="user">User name</option><option value="domain">Domain</option><option value="repository">Repository</option><option value="tmux_window">tmux window</option></select></label><label>Alias value<input name="aliasValue" required maxlength="160" /></label><button class="context-button context-button--primary" type="submit">Add alias</button></form>
          <hr class="context-separator" />
          <form id="mergeForm" class="context-form"><label>Merge entity ID<input name="sourceId" required inputmode="numeric" /></label><label>Into canonical ID<input name="targetId" required inputmode="numeric" /></label><p class="context-caution">Every normalized reference is moved transactionally to the canonical target.</p><button class="context-button context-button--danger" type="submit">Merge entities</button></form><hr class="context-separator" />
          <form id="splitForm" class="context-form"><label>Source entity ID<input name="sourceId" required inputmode="numeric" /></label><label>New display name<input name="displayName" required maxlength="120" /></label><label>New stable identity<input name="identity" required maxlength="240" /></label><label>Context interval IDs (comma-separated)<input name="contextIds" required placeholder="12, 18" /></label><button class="context-button" type="submit">Split selected contexts</button></form>
        </section>
      </div>
    </div>

    <div class="context-view" data-context-view="integrations" hidden>
      <div class="context-grid context-grid--integrations">
        <section class="panel context-panel context-card--wide"><div class="context-panel-head"><div><span class="eyebrow">Authenticated loopback</span><h3>Browser pairing</h3><p>Default: domains only. Tokens and event payloads never appear here or in logs.</p></div><button class="context-button context-button--primary" type="button" data-browser-pair>Create 5-minute code</button></div><div id="pairingCode" class="pairing-code" hidden aria-live="polite"></div><div id="browserClients" class="context-load-state" role="status">Loading browser clients…</div></section>
        <section class="panel context-panel"><div class="context-panel-head"><div><span class="eyebrow">Privacy health</span><h3>What is collected</h3></div></div><div class="privacy-list"><div><i class="health-dot health-dot--good"></i><span><b>Domains</b><small>Normalized origin, no query or fragment</small></span></div><div><i class="health-dot"></i><span><b>Titles</b><small>Off unless explicitly enabled</small></span></div><div><i class="health-dot health-dot--good"></i><span><b>Media state</b><small>Selected sites, separate playback metrics</small></span></div><div><i class="health-dot health-dot--good"></i><span><b>Private windows</b><small>Discarded by default</small></span></div></div></section>
      </div>
    </div>

    <div class="context-view" data-context-view="policies" hidden>
      <div class="context-grid context-grid--policies">
        <section class="panel context-panel context-card--wide"><div class="context-panel-head"><div><span class="eyebrow">Observe → warn → block</span><h3>Attention policies</h3><p>Every policy names the metric it counts. Hard blocking is always opt-in.</p></div></div><div id="policyList" class="context-load-state" role="status">Loading policies…</div></section>
        <section class="panel context-panel"><div class="context-panel-head"><div><span class="eyebrow">Create or edit</span><h3>Policy editor</h3></div></div>${policyForm()}</section>
      </div>
      <section class="panel context-panel"><div class="context-panel-head"><div><span class="eyebrow">Audit</span><h3>Intervention timeline</h3><p>Warnings, snoozes, blocks, and overrides—without free-text reasons.</p></div></div><div id="interventionTimeline" class="context-load-state" role="status">Loading interventions…</div></section>
    </div>

    <div class="context-view" data-context-view="data" hidden>
      <div class="context-grid context-grid--data">
        <section class="panel context-panel context-card--wide"><div class="context-panel-head"><div><span class="eyebrow">Capacity guardrails</span><h3>Coverage, confidence & storage</h3><p>Compact intervals target 100–250 MiB/year; unchanged heartbeats are not retained.</p></div></div><div id="contextStorage" class="context-load-state" role="status">Loading storage health…</div></section>
        <section class="panel context-panel"><div class="context-panel-head"><div><span class="eyebrow">Local controls</span><h3>Export & deletion</h3></div></div><form id="exportForm" class="context-form"><label>Export<select name="kind"><option value="activity">Normalized activity intervals</option><option value="entities">Entities and aliases</option><option value="all">All normalized context</option></select></label><label>Format<select name="format"><option value="json">JSON</option><option value="csv">CSV</option></select></label><button class="context-button context-button--primary" type="submit">Prepare local export</button></form><hr class="context-separator" /><form id="deleteForm" class="context-form"><label>Delete<select name="kind"><option value="browser_titles">Browser titles only</option><option value="browser_client">One browser client</option><option value="derived_labels">Derived labels and intervals</option><option value="all_context">All context telemetry</option></select></label><label>Client ID (when applicable)<input name="clientId" inputmode="numeric" /></label><label>Type DELETE to confirm<input name="confirmation" required autocomplete="off" /></label><button class="context-button context-button--danger" type="submit">Delete local data</button></form></section>
      </div>
    </div>
    <div id="contextNotice" class="context-notice" role="status" aria-live="polite" hidden></div>`;
}

function navButton(view, label, active = false) {
  return `<button type="button" class="${active ? 'active' : ''}" data-context-tab="${view}" aria-selected="${active}">${label}</button>`;
}

function panel(title, description, id, classes = '') {
  return `<section class="panel context-panel ${classes}"><div class="context-panel-head"><div><span class="eyebrow">Live telemetry</span><h3>${title}</h3><p>${description}</p></div></div><div id="${id}" class="context-load-state" role="status">Loading…</div></section>`;
}

function policyForm() {
  return `<form id="policyForm" class="context-form"><input name="id" type="hidden" /><label>Name<input name="name" required maxlength="80" placeholder="Focused video limit" /></label><label>Domain scope<input name="scope" required maxlength="120" placeholder="youtube.com" /></label><label>Metric<select name="metric">${Object.entries(POLICY_METRICS).map(([value, item]) => `<option value="${value}">${item.label}</option>`).join('')}</select></label><div class="context-form-row"><label>Window<select name="windowKind"><option value="calendar-day">Calendar day</option><option value="rolling">Rolling hours</option><option value="session">Continuous session</option></select></label><label>Budget (minutes)<input name="budgetMinutes" required type="number" min="1" max="10080" value="60" /></label></div><div class="context-form-row"><label>Warn at (minutes)<input name="warningMinutes" type="number" min="0" value="50" /></label><label>Enforcement<select name="enforcement"><option value="observe">Observe</option><option value="warn">Warn</option><option value="soft-block">Soft block</option><option value="hard-block">Hard block</option></select></label></div><div class="context-form-row"><label>Grace (minutes)<input name="graceMinutes" type="number" min="0" value="5" /></label><label>Snooze (minutes)<input name="snoozeMinutes" type="number" min="0" value="10" /></label></div><label class="context-check"><input name="enabled" type="checkbox" checked /> Policy enabled</label><div class="context-form-actions"><button class="context-button" type="reset">Clear</button><button class="context-button context-button--primary" type="submit">Save policy</button></div></form>`;
}

function activityOptions() {
  return ['work', 'entertainment', 'communication', 'administration', 'system', 'idle', 'unknown'].map(value => `<option value="${value}">${titleCase(value)}</option>`).join('');
}

function bindNavigation(root) {
  root.querySelectorAll('[data-context-tab]').forEach(button => button.addEventListener('click', () => {
    const selected = button.dataset.contextTab;
    root.querySelectorAll('[data-context-tab]').forEach(item => {
      const active = item === button;
      item.classList.toggle('active', active);
      item.setAttribute('aria-selected', String(active));
    });
    root.querySelectorAll('[data-context-view]').forEach(view => { view.hidden = view.dataset.contextView !== selected; });
  }));
  root.querySelector('[data-context-refresh]').addEventListener('click', refreshDashboard);
}

function bindAnalyticsControls(root) {
  root.querySelector('#contextRange').addEventListener('change', event => {
    state.range = defaultRange(Number(event.target.value));
    refreshAnalytics();
    loadUnknown();
  });
  root.querySelector('#contextMetric').addEventListener('change', event => {
    state.metric = event.target.value;
    root.querySelector('#contextMetricHelp').textContent = METRICS[state.metric]?.detail || '';
    refreshAnalytics();
  });
  root.querySelector('#contextGroup').addEventListener('change', event => {
    state.group = event.target.value;
    loadSummary();
  });
}

function bindUnknownReview(root) {
  root.querySelector('#unknownQueue').addEventListener('click', event => {
    const button = event.target.closest('[data-review-unknown]');
    if (!button) return;
    const item = state.unknown.find(row => String(row.id ?? row.contextIntervalId) === button.dataset.reviewUnknown);
    if (!item) return;
    const form = root.querySelector('#manualLabelForm');
    form.dataset.intervalId = String(item.contextIntervalId ?? item.id ?? '');
    form.dataset.startTs = String(item.startTs ?? item.start_ts ?? '');
    form.dataset.endTs = String(item.endTs ?? item.end_ts ?? '');
    form.querySelector('button[type="submit"]').disabled = false;
    root.querySelector('#correctionSelection').innerHTML = `<b>${escapeHtml(item.displayName || item.focusedApp || item.app || 'Unknown context')}</b><small>${formatContextDuration(durationOf(item))} · ${escapeHtml(confidenceLabel(item.confidence))}</small>`;
    root.querySelector('#correctionPanel').scrollIntoView({ behavior: reducedMotion() ? 'auto' : 'smooth', block: 'nearest' });
  });
  root.querySelector('#manualLabelForm').addEventListener('submit', async event => {
    event.preventDefault();
    const form = event.currentTarget;
    const values = Object.fromEntries(new FormData(form));
    await mutation(form, 'Saving correction…', () => createManualContextLabel({
      contextIntervalId: nullableNumber(form.dataset.intervalId),
      startTs: nullableNumber(form.dataset.startTs),
      endTs: nullableNumber(form.dataset.endTs),
      dimension: 'activity.primary', value: values.activity, scope: values.scope, note: values.note || null,
    }), 'Correction saved. Affected summaries will update after relabeling.');
    loadUnknown();
    loadManualHistory();
  });
  root.querySelector('#manualLabelHistory').addEventListener('click', async event => {
    const button = event.target.closest('[data-undo-manual]');
    if (!button || !window.confirm('Undo this correction and relabel the affected interval?')) return;
    await mutation(button, 'Undoing…', () => deleteManualContextLabel(button.dataset.undoManual), 'Correction removed; relabeling queued.');
    loadManualHistory();
    loadUnknown();
  });
}

function bindRuleEditor(root) {
  const form = root.querySelector('#ruleForm');
  root.querySelector('[data-rule-preview]').addEventListener('click', async buttonEvent => {
    if (!form.reportValidity()) return;
    const impact = root.querySelector('#ruleImpact');
    impact.textContent = 'Calculating affected intervals…';
    try {
      const result = await previewContextRule(buildRulePayload(Object.fromEntries(new FormData(form))));
      impact.innerHTML = `<b>${formatContextDuration(result.affectedSeconds || result.durationSeconds || 0)}</b> across <b>${escapeHtml(result.affectedIntervals ?? result.count ?? 0)}</b> intervals · ${escapeHtml(result.warning || 'Source observations remain unchanged.')}`;
    } catch (error) { renderInlineError(impact, error, 'Impact preview is not available yet.'); }
    buttonEvent.currentTarget.blur();
  });
  form.addEventListener('submit', async event => {
    event.preventDefault();
    if (!form.reportValidity()) return;
    await mutation(form, 'Creating rule…', () => createContextRule(buildRulePayload(Object.fromEntries(new FormData(form)))), 'Rule created. Review its impact, then relabel history.');
    form.reset();
    loadRules();
  });
  root.querySelector('[data-relabel]').addEventListener('click', async event => {
    const progress = root.querySelector('#relabelProgress');
    progress.hidden = false;
    progress.querySelector('span').style.width = '12%';
    progress.querySelector('b').textContent = 'Queueing relabel…';
    try {
      const result = await startContextRelabel({ from: state.range.from, to: state.range.to, scope: 'selected_range' });
      renderRelabelProgress(progress, result);
      notify('Relabel queued. Battery and browser collection continue independently.');
    } catch (error) { renderInlineError(progress, error, 'Relabeling is not available yet.'); }
    event.currentTarget.blur();
  });
  root.querySelector('#ruleList').addEventListener('click', async event => {
    const remove = event.target.closest('[data-delete-rule]');
    if (remove) {
      if (!window.confirm('Delete this user rule and relabel affected history?')) return;
      await mutation(remove, 'Deleting…', () => deleteContextRule(remove.dataset.deleteRule), 'Rule deleted; relabeling queued.');
      loadRules();
      return;
    }
    const button = event.target.closest('[data-edit-rule]');
    if (!button) return;
    const rule = state.rules.find(item => String(item.id) === button.dataset.editRule);
    if (!rule) return;
    const condition = (rule.conditions || rule.conditionsJson || [])[0] || {};
    const output = (rule.outputs || rule.outputsJson || [])[0] || {};
    setFormValues(form, { name: rule.name, fact: condition.fact, operator: condition.operator, conditionValue: condition.value, dimension: output.dimension, outputValue: output.value, priority: rule.priority });
    form.scrollIntoView({ behavior: reducedMotion() ? 'auto' : 'smooth', block: 'nearest' });
  });
}

function bindEntityControls(root) {
  root.querySelector('#entityKind').addEventListener('change', event => loadEntities(event.target.value));
  root.querySelector('#entityList').addEventListener('click', event => {
    const button = event.target.closest('[data-select-entity]');
    if (!button) return;
    root.querySelector('#aliasForm [name="entityId"]').value = button.dataset.selectEntity;
    root.querySelector('#mergeForm [name="targetId"]').value = button.dataset.selectEntity;
  });
  root.querySelector('#aliasForm').addEventListener('submit', async event => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(event.currentTarget));
    await mutation(event.currentTarget, 'Adding alias…', () => addEntityAlias(values.entityId, { aliasKind: values.aliasKind, aliasValue: values.aliasValue, source: 'user', confidence: 1 }), 'Alias added. Preview and relabel to apply it to history.');
    loadEntities(root.querySelector('#entityKind').value);
  });
  root.querySelector('#mergeForm').addEventListener('submit', async event => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(event.currentTarget));
    if (values.sourceId === values.targetId) return notify('Choose two different entities.', true);
    if (!window.confirm('Merge this entity into the canonical target? All normalized references will move.')) return;
    await mutation(event.currentTarget, 'Merging entities…', () => mergeContextEntities(values.sourceId, values.targetId), 'Entities merged transactionally.');
    loadEntities(root.querySelector('#entityKind').value);
  });
  root.querySelector('#splitForm').addEventListener('submit', async event => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(event.currentTarget));
    const contextIntervalIds = String(values.contextIds).split(',').map(value => Number(value.trim())).filter(Number.isInteger);
    await mutation(event.currentTarget, 'Splitting…', () => splitContextEntity(values.sourceId, { displayName: values.displayName, identity: values.identity, aliasIds: [], contextIntervalIds, manualLabelIds: [] }), 'Entity split created for the selected contexts.');
    loadEntities(root.querySelector('#entityKind').value);
  });
  root.querySelector('#entityList').addEventListener('click', async event => {
    const archive = event.target.closest('[data-archive-entity]');
    const remove = event.target.closest('[data-delete-entity]');
    if (archive) {
      await mutation(archive, 'Archiving…', () => archiveContextEntity(archive.dataset.archiveEntity), 'Entity archived.');
      loadEntities(root.querySelector('#entityKind').value);
    }
    if (remove && window.confirm('Permanently delete this already-archived entity? Referencing facts will be safely cleared.')) {
      await mutation(remove, 'Deleting…', () => deleteContextEntity(remove.dataset.deleteEntity), 'Archived entity deleted.');
      loadEntities(root.querySelector('#entityKind').value);
    }
  });
}

function bindBrowserControls(root) {
  root.querySelector('[data-browser-pair]').addEventListener('click', async event => {
    const target = root.querySelector('#pairingCode');
    target.hidden = false;
    target.textContent = 'Generating one-time code…';
    try {
      const result = await createBrowserPairingCode({ privacyMode: 'domains-only', expiresInSeconds: 300 });
      target.innerHTML = `<span>Enter in the extension</span><b>${escapeHtml(result.code || result.pairingCode || '—')}</b><small>Expires ${escapeHtml(result.expiresAt ? fmtDateTime(result.expiresAt) : 'in 5 minutes')} · shown once</small>`;
    } catch (error) { renderInlineError(target, error, 'Pairing is not available yet.'); }
    event.currentTarget.blur();
  });
  root.querySelector('#browserClients').addEventListener('click', async event => {
    const button = event.target.closest('[data-revoke-client]');
    if (!button || !window.confirm('Revoke this browser client immediately?')) return;
    await mutation(button, 'Revoking…', () => revokeBrowserClient(button.dataset.revokeClient), 'Browser client revoked.');
    loadBrowser();
  });
}

export function buildPolicyPayload(values, enabled = true) {
  const id = nullableNumber(values.id);
  return {
    ...(id ? { id } : {}), schemaVersion: 1, name: values.name, enabled,
    scope: { domains: [values.scope], excludedDomains: Array.isArray(values.excludedDomains) ? values.excludedDomains : [] }, metric: values.metric, windowKind: values.windowKind,
    budgetSeconds: Number(values.budgetMinutes) * 60, warningSeconds: Number(values.warningMinutes || 0) * 60,
    enforcement: values.enforcement, schedule: { weekdays: [1,2,3,4,5,6,7], startMinute: 0, endMinute: 1440 },
    grace: { graceSeconds: Number(values.graceMinutes || 0) * 60, snoozeSeconds: Number(values.snoozeMinutes || 0) * 60, maxDailySnoozes: 2, sessionGapSeconds: 60, rollingSeconds: Number(values.rollingSeconds || 86400) },
  };
}

function bindPolicyControls(root) {
  const form = root.querySelector('#policyForm');
  form.addEventListener('reset', () => window.setTimeout(() => { form.elements.id.value = ''; }, 0));
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(form));
    const existing = state.policies.find(item => String(item.id) === String(values.id));
    const payload = buildPolicyPayload({ ...values, excludedDomains: existing?.scope?.excludedDomains || [] }, form.elements.enabled.checked);
    await mutation(form, 'Saving policy…', () => saveAttentionPolicy(payload), 'Policy saved. New enforcement starts only at the selected level.');
    form.reset();
    loadPolicies();
  });
  root.querySelector('#policyList').addEventListener('click', async event => {
    const edit = event.target.closest('[data-edit-policy]');
    const action = event.target.closest('[data-policy-action]');
    if (edit) {
      const policy = state.policies.find(item => String(item.id) === edit.dataset.editPolicy);
      if (!policy) return;
      setFormValues(form, {
        id: policy.id, name: policy.name, scope: policy.scope?.domains?.[0] || policy.scopeLabel || policy.domain || '', metric: policy.metric,
        windowKind: policy.windowKind, budgetMinutes: (policy.budgetSeconds || 0) / 60, warningMinutes: (policy.warningSeconds || 0) / 60,
        enforcement: policy.enforcement, graceMinutes: (policy.grace?.graceSeconds || 0) / 60, snoozeMinutes: (policy.grace?.snoozeSeconds || 0) / 60,
      });
      form.elements.enabled.checked = policy.enabled !== false;
      form.scrollIntoView({ behavior: reducedMotion() ? 'auto' : 'smooth', block: 'nearest' });
    }
    if (action) {
      const seconds = action.dataset.policyAction === 'snooze' ? 600 : 3600;
      await mutation(action, `${titleCase(action.dataset.policyAction)}…`, () => runPolicyAction(action.dataset.policyId, action.dataset.policyAction, seconds), `${titleCase(action.dataset.policyAction)} recorded.`);
      loadPolicies();
      loadInterventions();
    }
  });
}

function bindDataControls(root) {
  root.querySelector('#exportForm').addEventListener('submit', async event => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(event.currentTarget));
    await mutation(event.currentTarget, 'Preparing export…', async () => {
      const result = await requestContextExport({ ...values, from: state.range.from, to: state.range.to });
      const content = result.format === 'csv' ? result.content : JSON.stringify(result.data, null, 2);
      const url = URL.createObjectURL(new Blob([content], { type: result.mediaType }));
      const link = document.createElement('a');
      link.href = url;
      link.download = result.fileName;
      link.click();
      URL.revokeObjectURL(url);
      return result;
    }, 'Export prepared locally.');
  });
  root.querySelector('#deleteForm').addEventListener('submit', async event => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(event.currentTarget));
    if (values.confirmation !== 'DELETE') return notify('Type DELETE exactly to confirm.', true);
    const request = { kind: values.kind, clientId: nullableNumber(values.clientId), rebuildDerived: true };
    let preview;
    try { preview = await deleteContextData({ ...request, preview: true }); }
    catch (error) { return notify(safeError(error), true); }
    const detail = Object.entries(preview.counts || {}).map(([table, count]) => `${table}: ${count}`).join('\n');
    if (!window.confirm(`Permanently delete ${preview.totalRows || 0} affected rows/fields?\n\n${detail}`)) return;
    await mutation(event.currentTarget, 'Deleting local data…', () => deleteContextData(request), 'Selected local data deleted; derived views are rebuilding.');
    event.currentTarget.reset();
    refreshDashboard();
  });
}

function refreshDashboard() {
  state.requestGeneration++;
  refreshOverview();
  refreshAnalytics();
  loadUnknown();
  loadManualHistory();
  loadRules();
  loadEntities();
  loadBrowser();
  loadPolicies();
  loadInterventions();
  loadStorage();
}

function refreshOverview() {
  loadInto('contextLatest', fetchContextLatest(), renderLatest, 'Latest context endpoint is not available yet.');
  loadInto('contextHealth', fetchContextHealth(), renderHealth, 'Source health endpoint is not available yet.');
}

function refreshAnalytics() { loadSummary(); loadTimeline(); loadEpisodes(); }

function loadSummary() {
  loadInto('activitySummary', fetchActivitySummary({ ...state.range, group: state.group, metric: state.metric }), renderSummary, 'Activity summary is not available yet.');
}
function loadTimeline() {
  loadInto('activityTimeline', fetchActivityTimeline({ ...state.range, resolution: '15m', metric: state.metric }), renderTimeline, 'Activity timeline is not available yet.');
}
function loadEpisodes() {
  loadInto('activityEpisodes', fetchActivityEpisodes({ ...state.range, kind: 'entertainment', minSeconds: 300, metric: state.metric }), renderEpisodes, 'Episode analytics are not available yet.');
}
function loadUnknown() {
  loadInto('unknownQueue', fetchContextUnknown({ ...state.range, limit: 24 }), payload => {
    state.unknown = listFrom(payload, ['items', 'unknown', 'rows', 'intervals']);
    return renderUnknown(state.unknown);
  }, 'Unknown review API is not available yet.');
}
function loadManualHistory() {
  loadInto('manualLabelHistory', fetchManualContextLabels({ limit: 20 }), payload => {
    state.manualLabels = listFrom(payload, ['labels', 'items', 'data']);
    return renderManualHistory(state.manualLabels);
  }, 'Correction history is not available.');
}
function loadRules() {
  loadInto('ruleList', fetchContextRules(), payload => {
    state.rules = listFrom(payload, ['rules', 'items', 'data']);
    const progress = payload.relabel || payload.relabelProgress;
    if (progress) renderRelabelProgress(document.getElementById('relabelProgress'), progress);
    return renderRules(state.rules);
  }, 'Rule management API is not available yet.');
}
function loadEntities(kind = '') {
  loadInto('entityList', fetchContextEntities({ kind }), payload => {
    state.entities = listFrom(payload, ['entities', 'items', 'data']);
    return renderEntities(state.entities);
  }, 'Entity management API is not available yet.');
}
function loadBrowser() {
  loadInto('browserClients', fetchBrowserIntegration(), renderBrowser, 'Browser integration is not configured yet.');
}
function loadPolicies() {
  loadInto('policyList', fetchAttentionPolicies(), payload => {
    state.policies = listFrom(payload, ['policies', 'items', 'data']);
    return renderPolicies(state.policies);
  }, 'Policy API is not available yet. Observe-only telemetry can still run.');
}
function loadInterventions() {
  loadInto('interventionTimeline', fetchInterventions(state.range), renderInterventions, 'No intervention audit endpoint is available yet.');
}
function loadStorage() {
  loadInto('contextStorage', fetchContextStorage(), renderStorage, 'Context storage metrics are not available yet.');
}

async function loadInto(id, promise, renderer, fallback) {
  const target = document.getElementById(id);
  if (!target) return;
  target.className = 'context-load-state';
  target.textContent = 'Loading…';
  try {
    const payload = await promise;
    if (!target.isConnected) return;
    target.className = '';
    target.innerHTML = renderer(payload);
  } catch (error) {
    if (!target.isConnected) return;
    renderInlineError(target, error, fallback);
  }
}

function renderLatest(payload) {
  const tracking = payload?.trackingEnabled;
  const latest = payload?.latest ?? payload;
  const labels = Array.isArray(latest?.labels) ? latest.labels : [];
  const sourceFacts = [
    ['Application', latest?.focusedApp || latest?.app],
    ['Window', latest?.focusedTitle || latest?.title],
    ['Browser', latest?.browserDomain || latest?.domain || latest?.serviceName],
    ['Terminal', joinParts([latest?.tmuxSession, latest?.tmuxWindow, latest?.tmuxPane])],
    ['Repository / project', latest?.projectName || latest?.repositoryName || latest?.terminalCwdDisplay],
    ['Foreground tool', latest?.foregroundExecutable || latest?.toolName],
    ['Agent session', joinParts([latest?.agentType, latest?.agentSessionName || latest?.nativeAgentSessionDisplay, latest?.agentState])],
  ];
  const ambiguity = latest?.ambiguity || latest?.ambiguityWarnings || [];
  document.getElementById('contextLiveText').textContent = tracking === false ? 'Tracking off' : latest ? 'Context observed' : 'Waiting for context';
  return `<div class="inspector-status"><span class="context-badge ${tracking === false ? 'context-badge--warn' : 'context-badge--good'}">${tracking === false ? 'Tracking disabled' : latest?.closedReason ? 'Last known' : 'Live interval'}</span><span>${escapeHtml(latest?.lastObservedTs ? `Observed ${fmtDateTime(latest.lastObservedTs)}` : 'No observation yet')}</span></div>
    <dl class="context-inspector">${sourceFacts.map(([key, value]) => `<div><dt>${key}</dt><dd>${escapeHtml(value || 'Not available')}</dd></div>`).join('')}</dl>
    <div class="context-labels">${labels.length ? labels.map(label => `<span><b>${escapeHtml(label.dimension)}</b>${escapeHtml(label.entityName || label.value || '—')}<small>${Math.round(normalizeRatio(label.confidence) * 100)}% · ${escapeHtml(label.source || 'unknown source')}</small></span>`).join('') : '<p class="context-empty">No computed labels for this interval.</p>'}</div>
    ${ambiguity.length ? `<div class="context-warning"><b>Ambiguous evidence</b>${escapeHtml(ambiguity.map(item => item.message || item).join(' · '))}</div>` : ''}`;
}

function renderHealth(payload) {
  const sources = listFrom(payload, ['sources', 'items', 'health']);
  if (!sources.length) {
    const entries = Object.entries(payload || {}).filter(([, value]) => value && typeof value === 'object');
    sources.push(...entries.map(([name, value]) => ({ name, ...value })));
  }
  if (!sources.length) return empty('No source health has been reported.');
  return `<div class="source-health">${sources.map(source => {
    const status = source.status || (source.enabled === false ? 'off' : source.errorCategory ? 'degraded' : 'healthy');
    const detail = source.detail && typeof source.detail === 'object'
      ? Object.entries(source.detail).filter(([,value]) => value != null && typeof value !== 'object').slice(0, 8).map(([key,value]) => `${titleCase(key)} ${value}`).join(' · ')
      : source.detail || source.errorCategory || (source.lastSuccess ? healthDetail(source) : status);
    return `<div><i class="health-dot ${status === 'healthy' || status === 'ok' ? 'health-dot--good' : status === 'off' ? '' : 'health-dot--warn'}"></i><span><b>${escapeHtml(titleCase(source.name || source.source || 'source'))}</b><small>${escapeHtml(detail)}</small></span><em>${escapeHtml(source.version ? `v${source.version}` : status)}</em></div>`;
  }).join('')}</div>`;
}

function renderSummary(payload) {
  const summary = normalizeActivitySummary(payload);
  const max = Math.max(1, ...summary.items.map(item => item.seconds));
  const coverage = document.getElementById('activityCoverage');
  coverage.innerHTML = [
    stat('Observed', formatContextDuration(summary.observedSeconds), 'Duration with source evidence'),
    stat('Unknown', formatContextDuration(summary.unknownSeconds), summary.observedSeconds ? `${Math.round(summary.unknownSeconds / summary.observedSeconds * 100)}% of observed` : 'No observed duration'),
    stat('Coverage', percent(summary.coverage), 'Monitoring coverage; gaps excluded'),
    stat('Confidence', percent(summary.confidence), `Rules revision ${summary.revision}`),
  ].join('');
  if (!summary.items.length) return empty('No observed activity in this range.');
  return `<div class="summary-bars">${summary.items.map((item, index) => `<div><span class="summary-name"><i class="tone-${ACTIVITY_TONES[index % ACTIVITY_TONES.length]}"></i><b>${escapeHtml(item.label)}</b><small>${item.episodes ? `${item.episodes} episodes` : confidenceLabel(item.confidence)}</small></span><span class="summary-track"><i class="tone-${ACTIVITY_TONES[index % ACTIVITY_TONES.length]}" style="width:${Math.max(1, item.seconds / max * 100).toFixed(1)}%"></i></span><strong>${formatContextDuration(item.seconds)}</strong></div>`).join('')}</div><p class="context-footnote">${escapeHtml(summary.metricDefinition || METRICS[state.metric]?.detail || '')}</p>`;
}

function renderTimeline(payload) {
  const buckets = listFrom(payload, ['buckets']);
  const intervals = buckets.length
    ? buckets.flatMap(bucket => Object.entries(bucket.groupsMs || {}).filter(([, duration]) => Number(duration) > 0).map(([activity]) => ({ startTs: bucket.startTs, endTs: bucket.endTs, activity: payload.groupLabels?.[activity] || activity })))
    : listFrom(payload, ['intervals', 'items', 'segments', 'points']);
  if (!intervals.length) return empty('No timeline intervals in this range. Unobserved time is never inferred.');
  const minTs = Math.min(...intervals.map(item => Number(item.startTs ?? item.ts ?? state.range.from)));
  const maxTs = Math.max(...intervals.map(item => Number(item.endTs ?? item.ts ?? state.range.to)));
  const span = Math.max(1, maxTs - minTs);
  const grouped = new Map();
  for (const item of intervals) {
    const label = item.activity || item.label || item.value || item.metricState || 'Unknown';
    if (!grouped.has(label)) grouped.set(label, []);
    grouped.get(label).push(item);
  }
  return `<div class="activity-timeline" aria-label="Activity timeline">${[...grouped.entries()].slice(0, 8).map(([label, rows], rowIndex) => `<div><span>${escapeHtml(label)}</span><div class="timeline-track">${rows.map(item => {
    const start = Number(item.startTs ?? item.ts ?? minTs);
    const end = Number(item.endTs ?? item.ts ?? start + span / Math.max(intervals.length, 1));
    const left = Math.max(0, (start - minTs) / span * 100);
    const width = Math.max(.7, (end - start) / span * 100);
    return `<i class="tone-${ACTIVITY_TONES[rowIndex % ACTIVITY_TONES.length]}" style="left:${left.toFixed(2)}%;width:${Math.min(100 - left, width).toFixed(2)}%" title="${escapeAttr(`${label} · ${formatContextDuration((end - start) / 1000)}`)}"></i>`;
  }).join('')}</div></div>`).join('')}<footer><span>${escapeHtml(fmtDateTime(minTs))}</span><span>${escapeHtml(fmtDateTime(maxTs))}</span></footer></div>`;
}

function renderEpisodes(payload) {
  const rows = listFrom(payload, ['episodes', 'items', 'rows']);
  if (!rows.length) return empty('No qualifying episodes in this range.');
  return `<div class="episode-list">${rows.slice(0, 12).map(row => {
    const start = row.startTs ?? row.start_ts;
    const end = row.endTs ?? row.end_ts;
    const crossDay = start && end && new Date(start).toDateString() !== new Date(end).toDateString();
    return `<article><span><b>${escapeHtml(row.serviceName || row.service || row.activity || 'Entertainment')}</b><small>${escapeHtml(start ? fmtDateTime(start) : 'Time unavailable')}</small></span><strong>${formatContextDuration(durationOf(row))}</strong>${crossDay ? '<em>Cross-midnight</em>' : ''}<small>${escapeHtml(row.metric || state.metric)} · ${confidenceLabel(row.confidence)}</small></article>`;
  }).join('')}</div>`;
}

function renderUnknown(rows) {
  if (!rows.length) return empty('No unknown or low-confidence intervals need review.');
  return `<div class="unknown-list">${rows.map(row => {
    const id = row.id ?? row.contextIntervalId;
    return `<article><div><span class="context-badge ${normalizeRatio(row.confidence) < .5 ? 'context-badge--warn' : ''}">${escapeHtml(row.reason || confidenceLabel(row.confidence))}</span><h4>${escapeHtml(row.displayName || row.focusedApp || row.app || row.domain || 'Unknown context')}</h4><p>${escapeHtml(row.safeDetail || row.serviceName || row.projectName || 'No additional privacy-safe detail')}</p></div><span><strong>${formatContextDuration(durationOf(row))}</strong><small>${escapeHtml(row.startTs ? fmtDateTime(row.startTs) : `${row.intervalCount || 1} interval(s)`)}</small><button class="context-button" type="button" data-review-unknown="${escapeAttr(id)}">Review</button></span></article>`;
  }).join('')}</div>`;
}

function renderManualHistory(rows) {
  if (!rows.length) return empty('No manual corrections yet.');
  return `<div class="rule-list">${rows.map(row => `<article><div><h4>${escapeHtml(`${row.dimension}: ${row.value || row.entityName || `Entity ${row.entityId}`}`)}</h4><small>${escapeHtml(fmtDateTime(row.createdTs))} · ${formatContextDuration(durationOf(row))}</small></div><button class="context-link context-link--danger" type="button" data-undo-manual="${escapeAttr(row.id)}">Undo</button></article>`).join('')}</div>`;
}

function renderRules(rules) {
  if (!rules.length) return empty('No user rules yet. Built-in generic rules may still classify exact facts.');
  return `<div class="rule-list">${rules.map(rule => `<article><span class="rule-priority">${escapeHtml(rule.priority ?? '—')}</span><div><h4>${escapeHtml(rule.name || 'Unnamed rule')}</h4><p>${escapeHtml(rule.summary || summarizeRule(rule))}</p><small>${escapeHtml(rule.origin || 'user')} · revision ${escapeHtml(rule.revision ?? '—')}</small></div><span><i class="health-dot ${rule.enabled !== false ? 'health-dot--good' : ''}"></i>${rule.enabled !== false ? 'Enabled' : 'Paused'}<button class="context-link" type="button" data-edit-rule="${escapeAttr(rule.id)}">Use as template</button>${rule.origin === 'user' ? `<button class="context-link context-link--danger" type="button" data-delete-rule="${escapeAttr(rule.id)}">Undo rule</button>` : ''}</span></article>`).join('')}</div>`;
}

function renderEntities(entities) {
  if (!entities.length) return empty('No entities discovered for this filter.');
  return `<div class="entity-table"><div class="entity-table-head"><span>Entity</span><span>Evidence</span><span>Last seen</span><span></span></div>${entities.map(entity => `<div><span><i class="entity-icon">${escapeHtml((entity.kind || '?').slice(0, 1).toUpperCase())}</i><b>${escapeHtml(entity.displayName || entity.name || 'Unnamed')}</b><small>${escapeHtml(entity.kind || 'entity')} · #${escapeHtml(entity.id)}</small></span><span>${escapeHtml(entity.aliasCount ?? entity.aliases?.length ?? 0)} aliases<br><small>${entity.userConfirmed ? 'User confirmed' : confidenceLabel(entity.confidence)}</small></span><span>${escapeHtml(entity.lastSeenTs ? fmtDateTime(entity.lastSeenTs) : '—')}</span><span><button class="context-link" type="button" data-select-entity="${escapeAttr(entity.id)}">Select</button>${entity.archived ? `<button class="context-link context-link--danger" type="button" data-delete-entity="${escapeAttr(entity.id)}">Delete</button>` : `<button class="context-link" type="button" data-archive-entity="${escapeAttr(entity.id)}">Archive</button>`}</span></div>`).join('')}</div>`;
}

function renderBrowser(payload) {
  const clients = listFrom(payload, ['clients', 'browserClients', 'items']);
  const integration = payload.integration || payload;
  const health = `<div class="integration-summary">${stat('Policy revision', integration.policyRevision ?? '—', integration.degraded ? 'Enforcement degraded' : 'Configuration synced')}${stat('Accepted', integration.acceptedCount ?? '—', `${integration.duplicateCount ?? 0} duplicate · ${integration.rejectedCount ?? 0} rejected`)}</div>`;
  if (!clients.length) return health + empty('No paired browser profiles. Pairing is optional and battery collection is unaffected.');
  return health + `<div class="client-list">${clients.map(client => `<article><i class="health-dot ${client.revokedTimestamp ? '' : 'health-dot--good'}"></i><span><b>${escapeHtml(client.displayName || client.browserFamily || 'Browser profile')}</b><small>${escapeHtml(client.browserFamily || 'browser')} · extension ${escapeHtml(client.extensionVersion || '—')} · ${escapeHtml(client.privacyMode || 'domains-only')}</small></span><span><b>${escapeHtml(client.lastSeenTimestamp ? fmtDateTime(client.lastSeenTimestamp) : 'Never synced')}</b><small>${escapeHtml(client.queueHealth ? `${client.queueHealth.depth} queued · ${formatBytes(client.queueHealth.bytes)} · ${client.queueHealth.dropped} dropped` : 'Queue health unavailable')}</small></span><button class="context-button context-button--danger" type="button" data-revoke-client="${escapeAttr(client.id)}" ${client.revokedTimestamp ? 'disabled' : ''}>Revoke</button></article>`).join('')}</div>`;
}

function renderPolicies(policies) {
  if (!policies.length) return empty('No policies. Start in observe mode after validating your selected metric.');
  return `<div class="policy-list">${policies.map(policy => {
    const used = Number(policy.usedSeconds || policy.consumedSeconds || 0);
    const budget = Math.max(1, Number(policy.budgetSeconds || 1));
    return `<article><div class="policy-top"><span><b>${escapeHtml(policy.name || 'Unnamed policy')}</b><small>${escapeHtml(policy.scopeLabel || policy.scope?.domains?.join(', ') || 'Custom scope')} · ${escapeHtml(POLICY_METRICS[policy.metric]?.label || policy.metric || 'metric')}</small></span><span class="context-badge ${policy.enforcement === 'hard-block' ? 'context-badge--warn' : ''}">${escapeHtml(policy.enforcement || 'observe')}</span></div><div class="policy-meter"><i style="width:${Math.min(100, used / budget * 100).toFixed(1)}%"></i></div><div class="policy-bottom"><span><b>${formatContextDuration(used)}</b> of ${formatContextDuration(budget)} · ${formatContextDuration(Math.max(0, budget - used))} left</span><span><button class="context-link" type="button" data-edit-policy="${escapeAttr(policy.id)}">Edit</button><button class="context-link" type="button" data-policy-id="${escapeAttr(policy.id)}" data-policy-action="snooze">Snooze 10m</button><button class="context-link context-link--danger" type="button" data-policy-id="${escapeAttr(policy.id)}" data-policy-action="override">Override 1h</button></span></div></article>`;
  }).join('')}</div>`;
}

function renderInterventions(payload) {
  const rows = listFrom(payload, ['events', 'interventions', 'items']);
  if (!rows.length) return empty('No interventions in this range.');
  return `<ol class="intervention-list">${rows.slice(0, 30).map(row => `<li><i class="${row.action === 'hard-block' ? 'is-danger' : ''}"></i><span><b>${escapeHtml(titleCase(String(row.action || 'decision').replaceAll('_', ' ')))}</b><small>${escapeHtml(row.policyName || `Policy ${row.policyId || '—'}`)} · revision ${escapeHtml(row.decisionRevision ?? '—')}</small></span><span>${escapeHtml(row.eventTs ? fmtDateTime(row.eventTs) : '—')}<small>${row.remainingSeconds != null ? `${formatContextDuration(row.remainingSeconds)} remaining` : ''}</small></span></li>`).join('')}</ol>`;
}

function renderStorage(payload) {
  const tables = listFrom(payload, ['tables', 'items', 'storage']);
  const coverage = payload.coverage || payload.classification || {};
  const growth = payload.annualizedGrowthBytes ?? payload.estimatedAnnualBytes;
  const cards = `<div class="context-stat-grid context-stat-grid--storage">${stat('Observed coverage', percent(normalizeRatio(coverage.monitoring ?? coverage.coverage)), 'Gaps excluded from durations')}${stat('Classified', percent(normalizeRatio(coverage.classified ?? coverage.classification)), 'Across the selected dimension')}${stat('High confidence', percent(normalizeRatio(coverage.highConfidence)), 'At or above configured threshold')}${stat('Annualized growth', growth == null ? '—' : formatBytes(growth), 'Target: 100–250 MiB/year')}</div>`;
  if (!tables.length) return cards + empty('No table-level storage metrics reported.');
  return cards + `<div class="storage-table"><div><b>Table or index</b><b>Rows</b><b>Allocated</b><b>Status</b></div>${tables.map(row => `<div><span>${escapeHtml(row.name || row.table || 'Unknown')}</span><span>${escapeHtml(formatNumber(row.rows ?? row.rowCount))}</span><span>${escapeHtml(formatBytes(row.bytes ?? row.allocatedBytes ?? 0))}</span><span>${escapeHtml(row.lagSeconds != null ? `${formatContextDuration(row.lagSeconds)} lag` : row.status || 'current')}</span></div>`).join('')}</div><p class="context-footnote">${escapeHtml(payload.reusablePages != null ? `${formatNumber(payload.reusablePages)} reusable SQLite pages · ` : '')}${escapeHtml(payload.compactionLagSeconds != null ? `${formatContextDuration(payload.compactionLagSeconds)} compaction lag` : 'Compaction lag unavailable')}</p>`;
}

function stat(label, value, detail) {
  return `<div><span>${escapeHtml(label)}</span><b>${escapeHtml(value)}</b><small>${escapeHtml(detail)}</small></div>`;
}

function empty(message) { return `<div class="context-empty">${escapeHtml(message)}</div>`; }

function renderInlineError(target, error, fallback) {
  target.className = 'context-state context-state--unavailable';
  target.innerHTML = `<b>Not available</b><span>${escapeHtml(fallback)}</span><small>${escapeHtml(safeError(error))}</small>`;
}

function renderRelabelProgress(target, result = {}) {
  target.hidden = false;
  const progress = Math.max(0, Math.min(100, Number(result.percent ?? result.progressPercent ?? (result.status === 'completed' ? 100 : 12))));
  target.querySelector('span')?.style.setProperty('width', `${progress}%`);
  const label = target.querySelector('b');
  if (label) label.textContent = result.status === 'completed' ? 'Relabel complete' : `${titleCase(result.status || 'queued')} · ${Math.round(progress)}%`;
}

async function mutation(control, pendingText, operation, successText) {
  const button = control.matches?.('button') ? control : control.querySelector('button[type="submit"]');
  const previous = button?.textContent;
  if (button) { button.disabled = true; button.textContent = pendingText; }
  try {
    await operation();
    notify(successText);
  } catch (error) {
    notify(safeError(error), true);
  } finally {
    if (button) { button.disabled = false; button.textContent = previous; }
  }
}

function notify(message, error = false) {
  const notice = document.getElementById('contextNotice');
  if (!notice) return;
  notice.hidden = false;
  notice.classList.toggle('is-error', error);
  notice.textContent = message;
  window.clearTimeout(notify.timeout);
  notify.timeout = window.setTimeout(() => { notice.hidden = true; }, 5200);
}

function setFormValues(form, values) {
  for (const [name, value] of Object.entries(values)) if (form.elements[name] && value != null) form.elements[name].value = value;
}

function listFrom(payload, keys) {
  if (Array.isArray(payload)) return payload;
  for (const key of keys) {
    if (Array.isArray(payload?.[key])) return payload[key];
    if (payload?.[key] && typeof payload[key] === 'object') return Object.entries(payload[key]).map(([name, value]) => typeof value === 'object' ? { name, ...value } : { name, value });
  }
  return [];
}

function numberFrom(object, keys) {
  for (const key of keys) if (object?.[key] != null && Number.isFinite(Number(object[key]))) return Number(object[key]);
  return null;
}
function secondsFrom(object, secondKeys, millisecondKeys) {
  const seconds = numberFrom(object, secondKeys);
  if (seconds != null) return Math.max(0, seconds);
  const milliseconds = numberFrom(object, millisecondKeys);
  return milliseconds == null ? 0 : Math.max(0, milliseconds / 1000);
}
function confidenceFrom(object) {
  const direct = numberFrom(object, ['averageConfidence', 'confidenceRatio', 'attributionConfidence']);
  if (direct != null) return normalizeRatio(direct);
  if (typeof object?.confidence === 'number') return normalizeRatio(object.confidence);
  const distribution = object?.confidence || object?.confidenceDistribution;
  const low = Number(distribution?.lowMs || 0);
  const medium = Number(distribution?.mediumMs || 0);
  const high = Number(distribution?.highMs || 0);
  const total = low + medium + high;
  return total ? (low * .25 + medium * .65 + high * .9) / total : 0;
}
function normalizeRatio(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return 0;
  return Math.max(0, Math.min(1, number > 1 ? number / 100 : number));
}
function durationOf(row) {
  const direct = secondsFrom(row, ['durationSeconds', 'observedSeconds', 'seconds', 'duration'], ['durationMs', 'observedMs']);
  if (direct) return direct;
  return Math.max(0, (Number(row.endTs ?? row.end_ts ?? 0) - Number(row.startTs ?? row.start_ts ?? 0)) / 1000);
}
function confidenceLabel(value) {
  if (value == null) return 'confidence unavailable';
  const ratio = normalizeRatio(value);
  return `${ratio >= .8 ? 'high' : ratio >= .5 ? 'medium' : 'low'} confidence · ${Math.round(ratio * 100)}%`;
}
function percent(value) { return value ? `${Math.round(value * 100)}%` : '0%'; }
function titleCase(value) { return String(value || '').replaceAll(/[-_.]/g, ' ').replace(/\b\w/g, character => character.toUpperCase()); }
function joinParts(parts) { return parts.filter(Boolean).join(' · '); }
function nullableNumber(value) { return value === '' || value == null ? null : Number(value); }
function formatNumber(value) { return Number(value || 0).toLocaleString(); }
function formatBytes(bytes) {
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let value = Math.max(0, Number(bytes) || 0);
  let index = 0;
  while (value >= 1024 && index < units.length - 1) { value /= 1024; index++; }
  return `${value.toFixed(index && value < 10 ? 1 : 0)} ${units[index]}`;
}
function healthDetail(source) {
  const parts = [];
  if (source.lastSuccess) parts.push(`Last success ${fmtDateTime(source.lastSuccess)}`);
  if (source.matchRate != null) parts.push(`${Math.round(normalizeRatio(source.matchRate) * 100)}% match rate`);
  if (source.ambiguousCount != null) parts.push(`${source.ambiguousCount} ambiguous`);
  return parts.join(' · ') || source.status || 'unknown';
}
function summarizeRule(rule) {
  const conditions = Array.isArray(rule.conditions) ? rule.conditions : [];
  const outputs = Array.isArray(rule.outputs) ? rule.outputs : [];
  return `${conditions.map(item => `${item.fact} ${item.operator} ${item.value ?? ''}`).join(', ') || 'Conditions unavailable'} → ${outputs.map(item => `${item.dimension}=${item.value ?? item.entity ?? ''}`).join(', ') || 'outputs unavailable'}`;
}
function safeError(error) {
  const message = error instanceof Error ? error.message : 'Request failed';
  return /^Request failed \(\d+\)$/.test(message) ? message : 'The local service rejected or could not complete this request.';
}
function reducedMotion() { return window.matchMedia?.('(prefers-reduced-motion: reduce)').matches; }
