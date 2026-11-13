// The host's rules on the built executable (CC_AGENT_HOST; mac/agent-host/test.sh sets it), driven over its socket as
// ccd drives it: host.json, the 30 s exit, sequence numbers, the kept messages, replay then one state, and the one
// attached connection. Each host is started as ccd starts it (rig.ts).
import { afterAll, expect, test } from 'bun:test';
import { existsSync, readFileSync, rmSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { cleanup, exitWithin, fixture, goldenLine, open, runDir, runWith, startHost, state, waitFor, writeStub } from './rig.ts';

afterAll(cleanup);

test('host.json is created exclusively, mode 600, before the socket listens; a second host exits 3', async () => {
  const dir = runDir();
  const first = startHost(dir);
  await open(dir);
  const recordPath = join(dir, 'host.json');
  const bytes = readFileSync(recordPath, 'utf8');
  expect(JSON.parse(bytes)).toStrictEqual({ protocol_version: goldenLine('host_json').protocol_version, host_pid: first.pid });
  expect(statSync(recordPath).mode & 0o777).toBe(0o600);
  expect(await exitWithin(startHost(dir), 2000)).toBe(3);
  expect(readFileSync(recordPath, 'utf8')).toBe(bytes);
  // A new connection, as ccd's next one: the second host left the first one's socket alone.
  expect(await (await open(dir)).attach(0, 1)).toStrictEqual([state(1)]);
}, 10_000);

test('a socket file a dead host left does not stop the next host', async () => {
  const dir = runDir();
  const dead = startHost(dir);
  await open(dir);
  process.kill(dead.pid, 'SIGKILL');
  await dead.exited;
  // ccd removes host.json once it has found the host gone (H7); here the socket file stays behind.
  rmSync(join(dir, 'host.json'));
  expect(existsSync(join(dir, 'host.sock'))).toBe(true);
  startHost(dir);
  expect(await (await open(dir)).attach(0, 1)).toStrictEqual([state(1)]);
}, 10_000);

test('a host with no start within 30 s exits 4, attached or not; a start keeps it', async () => {
  const began = performance.now();
  const alone = startHost(runDir());
  const attachedDir = runDir();
  const attached = startHost(attachedDir);
  const startedDir = runDir();
  const started = startHost(startedDir);
  // Each exit is timed when it happens, so an early one cannot hide behind a later one.
  const exits = Promise.all([alone, attached].map((host) =>
    host.exited.then((code) => ({ code, at: performance.now() - began }))));
  await (await open(attachedDir)).attach(0, 1);
  const ccd = await open(startedDir);
  await ccd.attach(0, 1);
  await Bun.sleep(1000);
  // A start that runs: its claude is the stand-in, which stays until its input ends.
  const start = goldenLine('to_host', 'start');
  const claude = writeStub(startedDir, { init: fixture('claude/initialize-api-key-2.1.319.jsonl').split('\n')[0] });
  ccd.send({ ...start, options: { ...start.options, cwd: startedDir, claude_path: claude } });
  for (const { code, at } of await exits) {
    expect(code).toBe(4);
    expect(at).toBeGreaterThan(29_500);
    expect(at).toBeLessThan(32_000);
  }
  await Bun.sleep(35_000 - (performance.now() - began));
  expect(started.exitCode).toBeNull();
  const replay = await ccd.attach(1, 2);
  expect(replay.map((line) => [line.type, line.status ?? null])).toStrictEqual([['outcome', 'done'], ['state', null]]);
}, 40_000);

test('sequence numbers run 1, 2, 3 across connections', async () => {
  const dir = runDir();
  startHost(dir);
  const states = [];
  for (let after = 0; after < 3; after += 1) states.push(...(await (await open(dir)).attach(after, 1)));
  expect(states).toStrictEqual([state(1), state(2), state(3)]);
}, 10_000);

// Each line as its type and seq, so a replay reads as one list.
const tags = (lines: any[]) => lines.map((line) => `${line.type}${line.seq}`);

test('attach {after} replays every kept message after it, in order, then one state', async () => {
  const dir = runDir();
  startHost(dir);
  await runWith(dir, 4);
  const later = await open(dir);
  expect(tags(await later.attach(3, 4))).toStrictEqual(['sdk4', 'sdk5', 'sdk6', 'state7']);
  await Bun.sleep(1000);
  expect(later.lines.length).toBe(4);
  // The cursor is committed: what attach {3} dropped is not replayed again.
  expect(tags(await later.attach(0, 4))).toStrictEqual(['sdk4', 'sdk5', 'sdk6', 'state8']);
}, 10_000);

test('a state supersedes the one before: after two unacknowledged attaches the replay holds none, and one follows', async () => {
  const dir = runDir();
  startHost(dir);
  await runWith(dir, 2);
  expect(tags(await (await open(dir)).attach(0, 4))).toStrictEqual(['outcome2', 'sdk3', 'sdk4', 'state5']);
  expect(tags(await (await open(dir)).attach(0, 4))).toStrictEqual(['outcome2', 'sdk3', 'sdk4', 'state6']);
}, 10_000);

test('kept messages outlive a connection; an ack drops them', async () => {
  const dir = runDir();
  startHost(dir);
  const first = await runWith(dir, 2);
  first.send({ type: 'ack', seq: 3 });
  await first.close();
  expect(tags(await (await open(dir)).attach(0, 2))).toStrictEqual(['sdk4', 'state5']);
}, 10_000);

test('a newer attach replaces the attached connection, which is read until then and never after', async () => {
  const dir = runDir();
  startHost(dir);
  const older = await runWith(dir, 2);
  const newer = await open(dir);
  await Bun.sleep(200);
  older.send({ type: 'ack', seq: 2 });
  expect(tags(await older.attach(0, 3))).toStrictEqual(['sdk3', 'sdk4', 'state5']);
  expect(tags(await newer.attach(0, 3))).toStrictEqual(['sdk3', 'sdk4', 'state6']);
  await waitFor('the host to close the older connection', () => older.closed);
  older.send({ type: 'ack', seq: 4 });
  await Bun.sleep(200);
  expect(tags(await (await open(dir)).attach(0, 3))).toStrictEqual(['sdk3', 'sdk4', 'state7']);
  expect(older.lines.length).toBe(7);
}, 10_000);

test('whole lines only: a line split across writes is applied, a partial line at close is not', async () => {
  const dir = runDir();
  startHost(dir);
  const first = await runWith(dir, 3);
  first.write('{"type":"ack",');
  await Bun.sleep(100);
  first.write('"seq":2}\n');
  expect(tags(await first.attach(0, 4))).toStrictEqual(['sdk3', 'sdk4', 'sdk5', 'state6']);
  first.write('{"type":"ack","seq":4}');
  await first.close();
  expect(tags(await (await open(dir)).attach(0, 4))).toStrictEqual(['sdk3', 'sdk4', 'sdk5', 'state7']);
}, 10_000);

test('a connection gone during the replay does not end the host', async () => {
  const dir = runDir();
  const host = startHost(dir);
  await runWith(dir, 50);
  const gone = await open(dir);
  gone.send({ type: 'attach', after: 0 });
  gone.socket.destroy();
  await Bun.sleep(300);
  expect(host.exitCode).toBeNull();
  // The outcome and the 50 SDK lines, then one state.
  const replay = await (await open(dir)).attach(0, 52);
  expect(tags(replay.slice(-1))).toStrictEqual(['state54']);
}, 10_000);

test('a line that is not JSON is dropped, and the host goes on', async () => {
  const dir = runDir();
  const host = startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  ccd.write('not json\n');
  expect(await ccd.attach(0, 1)).toStrictEqual([state(2)]);
  expect(host.exitCode).toBeNull();
}, 10_000);
