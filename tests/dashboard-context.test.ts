import { describe, expect, test } from 'bun:test';
import { fetchActivitySummary } from '../public/assets/api.js';

Object.defineProperty(globalThis, 'localStorage', {
  configurable: true,
  value: { getItem: () => null, setItem: () => undefined },
});
const {
  buildPolicyPayload,
  buildRulePayload,
  defaultRange,
  formatContextDuration,
  normalizeActivitySummary,
} = await import('../public/assets/context-dashboard.js');

describe('Plan 002 context dashboard', () => {
  test('uses bounded timestamp ranges', () => {
    const range = defaultRange(7, 1_800_000_000_000);
    expect(range.to).toBe(1_800_000_000_000);
    expect(range.to - range.from).toBe(7 * 86_400_000);
  });

  test('formats observed durations without implying missing coverage', () => {
    expect(formatContextDuration(42)).toBe('42s');
    expect(formatContextDuration(125)).toBe('2m');
    expect(formatContextDuration(7_500)).toBe('2h 5m');
  });

  test('normalizes aggregate shapes and confidence percentages', () => {
    const summary = normalizeActivitySummary({
      groups: [
        { value: 'work', durationSeconds: 3600, averageConfidence: 0.9, episodeCount: 2 },
        { value: 'unknown', durationSeconds: 600, averageConfidence: 40 },
      ],
      summary: {
        observedDurationSeconds: 5000,
        unknownDurationSeconds: 1000,
        monitoringCoverage: 80,
        averageConfidence: 0.75,
      },
      rulesRevision: 4,
    });
    expect(summary.items.map(item => item.label)).toEqual(['work', 'unknown']);
    expect(summary.items[1].confidence).toBe(0.4);
    expect(summary.coverage).toBe(0.8);
    expect(summary.unknownSeconds).toBe(1000);
    expect(summary.revision).toBe(4);
  });

  test('prefers metric duration over observation coverage in aggregate metadata', () => {
    const summary = normalizeActivitySummary({ groups: [], meta: { metricDurationMs: 60_000, observedMs: 3_600_000, unknownMs: 0, monitoringCoverage: 1 } });
    expect(summary.observedSeconds).toBe(60);
  });

  test('normalizes the production ActivityRepository millisecond payload', () => {
    const summary = normalizeActivitySummary({
      groups: [{ key: 'work', label: 'work', durationMs: 3_600_000, episodeCount: 1, confidence: { lowMs: 0, mediumMs: 0, highMs: 3_600_000 } }],
      meta: { observedMs: 3_600_000, unknownMs: 60_000, monitoringCoverage: 0.5, confidence: { lowMs: 0, mediumMs: 600_000, highMs: 3_000_000 }, rulesRevision: 7 },
      metricDefinition: 'Overlap-reconciled foreground attention.',
    });
    expect(summary.items[0]).toMatchObject({ label: 'work', seconds: 3600, episodes: 1 });
    expect(summary.observedSeconds).toBe(3600);
    expect(summary.unknownSeconds).toBe(60);
    expect(summary.items[0].confidence).toBeCloseTo(0.9);
    expect(summary.revision).toBe(7);
  });

  test('creates data-driven rule payloads with explicit stop dimensions', () => {
    expect(buildRulePayload({
      name: 'Editor work', priority: '250', fact: 'app.id', operator: 'exact',
      conditionValue: 'org.example.Editor', dimension: 'activity.primary', outputValue: 'work', stopDimension: 'on',
    })).toEqual({
      name: 'Editor work', priority: 250, enabled: true, origin: 'user',
      conditions: [{ fact: 'app.id', operator: 'exact', value: 'org.example.Editor' }],
      outputs: [{ dimension: 'activity.primary', value: 'work', confidence: 1 }],
      stopDimensions: ['activity.primary'],
    });
  });

  test('serializes the canonical shared policy-create fixture', async () => {
    const fixture = await Bun.file(new URL('../contracts/browser/v1/policy-create.json', import.meta.url)).json();
    expect(buildPolicyPayload({
      id: '', name: fixture.name, scope: fixture.scope.domains[0], excludedDomains: fixture.scope.excludedDomains, metric: fixture.metric, windowKind: fixture.windowKind,
      budgetMinutes: String(fixture.budgetSeconds / 60), warningMinutes: String(fixture.warningSeconds / 60), enforcement: fixture.enforcement,
      graceMinutes: String(fixture.grace.graceSeconds / 60), snoozeMinutes: String(fixture.grace.snoozeSeconds / 60), rollingSeconds: fixture.grace.rollingSeconds,
    }, fixture.enabled)).toEqual(fixture);
  });

  test('sends metric distinction and range to the planned summary endpoint', async () => {
    const previousFetch = globalThis.fetch;
    let requested = '';
    let requestInit: RequestInit | undefined;
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      requested = String(input);
      requestInit = init;
      return new Response(JSON.stringify({ groups: [] }), { status: 200, headers: { 'content-type': 'application/json' } });
    }) as typeof fetch;
    try {
      await fetchActivitySummary({ from: 100, to: 200, group: 'service', metric: 'focused_playback' });
      const url = new URL(requested, 'http://localhost');
      expect(url.pathname).toBe('/api/activity/summary');
      expect(Object.fromEntries(url.searchParams)).toEqual({ from: '100', to: '200', group: 'service', metric: 'focused_playback' });
      expect((requestInit?.headers as Record<string, string>)['X-Watchdog-CSRF']).toBe('1');
    } finally {
      globalThis.fetch = previousFetch;
    }
  });
});
