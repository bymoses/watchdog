async function json(response) {
  const contentType = response.headers.get('content-type') || '';
  const body = contentType.includes('application/json') ? await response.json() : null;
  if (!response.ok) throw new Error(body?.error || `Request failed (${response.status})`);
  return body;
}

async function request(path, { method = 'GET', body, signal } = {}) {
  const options = { method, signal, headers: { Accept: 'application/json', 'X-Watchdog-CSRF': '1' } };
  if (body !== undefined) {
    options.headers['Content-Type'] = 'application/json';
    options.body = JSON.stringify(body);
  }
  return fetch(path, options).then(json);
}

function query(path, values = {}) {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined && value !== null && value !== '') params.set(key, String(value));
  }
  const suffix = params.size ? `?${params}` : '';
  return path + suffix;
}

export async function fetchStatus() {
  return fetch('/api/status').then(json);
}

export async function setChargeType(value) {
  return request('/api/battery/charge-type', { method: 'POST', body: { value } });
}

export async function fetchSeries({ hours, top, afterTs = null }) {
  const params = new URLSearchParams({ hours: String(hours), top: String(top) });
  if (afterTs) params.set('after_ts', String(afterTs));
  return fetch('/api/series?' + params).then(r => r.json());
}

export async function fetchProcesses() {
  return fetch('/api/processes').then(r => r.json());
}

export async function fetchGroups({ hours }) {
  return fetch('/api/groups?hours=' + hours).then(r => r.json());
}

export async function fetchAgentUsage({ range = '7d', group = 'harness', agent = 'all', top = 8 } = {}) {
  const params = new URLSearchParams({ range, group, agent, top: String(top) });
  return fetch('/api/agents/usage?' + params).then(json);
}

// Plan 002 context, activity, browser, policy, and data-management contracts.
export const fetchContextLatest = ({ signal } = {}) => request('/api/context/latest', { signal });
export const fetchContextHealth = ({ signal } = {}) => request('/api/context/health', { signal });
export const fetchContextStorage = ({ signal } = {}) => request('/api/context/storage', { signal });
export const fetchContextEntities = ({ kind, signal } = {}) => request(query('/api/context/entities', { kind }), { signal });
export const fetchContextRules = ({ signal } = {}) => request('/api/context/rules', { signal });
export const fetchContextUnknown = ({ from, to, limit = 20, signal } = {}) => request(query('/api/context/unknown', { from, to, limit }), { signal });
export const fetchActivitySummary = ({ from, to, group = 'activity', metric = 'focus', signal } = {}) => request(query('/api/activity/summary', { from, to, group, metric }), { signal });
export const fetchActivityTimeline = ({ from, to, resolution = '15m', metric = 'focus', signal } = {}) => request(query('/api/activity/timeline', { from, to, resolution, metric }), { signal });
export const fetchActivityEpisodes = ({ from, to, kind = 'entertainment', minSeconds = 300, metric = 'focused_playback', signal } = {}) => request(query('/api/activity/episodes', { from, to, kind, minSeconds, metric }), { signal });

export const createContextRule = (rule, { signal } = {}) => request('/api/context/rules', { method: 'POST', body: rule, signal });
export const deleteContextRule = (ruleId, { signal } = {}) => request(`/api/context/rules/${encodeURIComponent(ruleId)}`, { method: 'DELETE', signal });
export const previewContextRule = (rule, { signal } = {}) => request('/api/context/rules/preview', { method: 'POST', body: rule, signal });
export const fetchManualContextLabels = ({ limit = 100, signal } = {}) => request(query('/api/context/manual-labels', { limit }), { signal });
export const createManualContextLabel = (label, { signal } = {}) => request('/api/context/manual-labels', { method: 'POST', body: label, signal });
export const deleteManualContextLabel = (labelId, { signal } = {}) => request(`/api/context/manual-labels/${encodeURIComponent(labelId)}`, { method: 'DELETE', signal });
export const startContextRelabel = (scope, { signal } = {}) => request('/api/context/relabel', { method: 'POST', body: scope, signal });
export const addEntityAlias = (entityId, alias, { signal } = {}) => request(`/api/context/entities/${encodeURIComponent(entityId)}/aliases`, { method: 'POST', body: alias, signal });
export const mergeContextEntities = (sourceId, targetId, { signal } = {}) => request('/api/context/entities/merge', { method: 'POST', body: { sourceId, targetId }, signal });
export const splitContextEntity = (sourceId, details, { signal } = {}) => request(`/api/context/entities/${encodeURIComponent(sourceId)}/split`, { method: 'POST', body: details, signal });
export const archiveContextEntity = (entityId, { signal } = {}) => request(`/api/context/entities/${encodeURIComponent(entityId)}/archive`, { method: 'POST', signal });
export const deleteContextEntity = (entityId, { signal } = {}) => request(`/api/context/entities/${encodeURIComponent(entityId)}`, { method: 'DELETE', signal });

export const fetchBrowserIntegration = ({ signal } = {}) => request('/api/integrations/browser/v1/clients', { signal });
export const createBrowserPairingCode = (details = {}, { signal } = {}) => request('/api/integrations/browser/v1/pair', { method: 'POST', body: { action: 'create_code', ...details }, signal });
export const revokeBrowserClient = (clientId, { signal } = {}) => request('/api/integrations/browser/v1/client', { method: 'DELETE', body: { clientId }, signal });

export const fetchAttentionPolicies = ({ signal } = {}) => request('/api/attention/policies', { signal });
export const saveAttentionPolicy = (policy, { signal } = {}) => { const { id, ...body } = policy; return request(id ? `/api/attention/policies/${encodeURIComponent(id)}` : '/api/attention/policies', { method: id ? 'PUT' : 'POST', body, signal }); };
export const runPolicyAction = (policyId, action, seconds, { signal } = {}) => request(`/api/attention/policies/${encodeURIComponent(policyId)}/actions`, { method: 'POST', body: { action, seconds }, signal });
export const fetchInterventions = ({ from, to, signal } = {}) => request(query('/api/interventions', { from, to }), { signal });

export const requestContextExport = (options, { signal } = {}) => request('/api/context/export', { method: 'POST', body: options, signal });
export const deleteContextData = (options, { signal } = {}) => request('/api/context/deletions', { method: 'POST', body: options, signal });
