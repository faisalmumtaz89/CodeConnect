// The host's `--usage <claude>` launch on the built executable, its claude the stand-in (stub-claude.ts) answering
// get_usage with an answer of the shape the SDK declares and claude 2.1.330 sent on a real account (app-qa's k306
// record, 2026-11-23: key names only; every value here is made up).
import { afterAll, expect, test } from 'bun:test';
import { spawnSync } from 'node:child_process';
import { cleanup, executable, recordedInit, runDir, stubLog, writeStub } from './rig.ts';

afterAll(cleanup);

const answer = {
  session: { total_cost_usd: 1.25, total_api_duration_ms: 1, total_duration_ms: 1, total_lines_added: 3, total_lines_removed: 1, model_usage: {} },
  subscription_type: 'max',
  rate_limits_available: true,
  rate_limits: {
    five_hour: null,
    seven_day: { utilization: 42, resets_at: '2026-11-30T05:00:00.123456+00:00', limit_dollars: null, used_dollars: null },
    seven_day_opus: { utilization: 7, resets_at: '2026-11-30T05:00:00+00:00' },
    seven_day_sonnet: null,
    model_scoped: [{ display_name: 'Marker Model 7f3a', resets_at: '2026-11-30T05:00:00+00:00', utilization: 9 }],
    limits: [{ kind: 'weekly_all', group: 'g', percent: 42, resets_at: '2026-11-30T05:00:00+00:00', scope: null, severity: 'ok', is_active: true }],
  },
  seven_day_breakdown: { marker: 1 },
  spend: { marker_spend: 12 },
  extra_usage: { marker_extra: true },
  organization_uuid: 'org-marker-7f3a',
  behaviors: null,
};

test("--usage prints only whether plan limits apply, the plan and each plan-wide window named, and asks without the transcript scan", () => {
  const dir = runDir();
  const claude = writeStub(dir, { init: recordedInit, usage: answer });
  const run = spawnSync(executable, ['--usage', claude], { cwd: dir, encoding: 'utf8', timeout: 20_000, env: { TMPDIR: dir } });
  expect(run.status).toBe(0);
  expect(JSON.parse(run.stdout.trim().split('\n').at(-1)!)).toStrictEqual({
    available: true,
    plan: 'max',
    windows: [{ name: 'seven_day', utilization: 42, resets_at: Math.floor(Date.parse('2026-11-30T05:00:00.123456+00:00') / 1000) }],
  });
  for (const dropped of ['cost', 'spend', 'extra', 'opus', 'Marker Model', 'weekly_all', 'org-marker', 'breakdown', 'dollars']) {
    expect(run.stdout).not.toContain(dropped);
  }
  const asked = stubLog(dir).filter((event) => event.usage).map((event) => event.usage);
  expect(asked).toStrictEqual([{ subtype: 'get_usage', skip_behaviors: true }]);
  expect(stubLog(dir).filter((event) => event.stdin?.type === 'user')).toStrictEqual([]);
}, 30_000);

// A restricted launch (Claude Code's own --restricted) opens no cross-session messaging socket, so this short helper
// never shows in a person's own Claude sessions' ListAgents as a peer they could message (B-1).
test('--usage launches claude restricted, so it opens no messaging socket', () => {
  const dir = runDir();
  const claude = writeStub(dir, { init: recordedInit, usage: answer });
  const run = spawnSync(executable, ['--usage', claude], { cwd: dir, encoding: 'utf8', timeout: 20_000, env: { TMPDIR: dir } });
  expect(run.status).toBe(0);
  const argv = stubLog(dir).find((event) => event.argv)!.argv as string[];
  expect(argv.filter((arg) => arg === '--restricted' || arg.startsWith('--restricted='))).toStrictEqual(['--restricted']);
}, 30_000);

test('--usage of a sign-in without plan limits prints that, with no window', () => {
  const dir = runDir();
  const claude = writeStub(dir, { init: recordedInit, usage: { subscription_type: null, rate_limits_available: false, rate_limits: null } });
  const run = spawnSync(executable, ['--usage', claude], { cwd: dir, encoding: 'utf8', timeout: 20_000, env: { TMPDIR: dir } });
  expect(run.status).toBe(0);
  expect(JSON.parse(run.stdout.trim())).toStrictEqual({ available: false, plan: null, windows: [] });
}, 30_000);
