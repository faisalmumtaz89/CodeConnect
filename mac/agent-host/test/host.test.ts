// The host's rules on the built executable (CC_AGENT_HOST; mac/agent-host/test.sh sets it), driven over its socket as
// ccd drives it: host.json, the 30 s exit, sequence numbers, the kept messages, replay then one state, and the one
// attached connection. Each host gets an environment of its own without BUN_BE_BUN, so it runs as the host, not as Bun.
import { afterAll, expect, test } from 'bun:test';
import type { Subprocess } from 'bun';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { connect, type Socket } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const executable = process.env.CC_AGENT_HOST;
if (!executable) throw new Error('CC_AGENT_HOST must name the built host (mac/agent-host/test.sh sets it)');

const golden = readFileSync(join(import.meta.dir, '..', '..', '..', 'fixtures', 'host', 'messages.jsonl'), 'utf8')
  .split('\n')
  .filter((text) => text !== '')
  .map((text) => JSON.parse(text) as { dir: string; message: Record<string, unknown> });
const goldenLine = (dir: string, type?: string) =>
  golden.find((entry) => entry.dir === dir && (type === undefined || entry.message.type === type))!.message;

const root = mkdtempSync(join(tmpdir(), 'cch-'));
const hosts: Subprocess[] = [];
afterAll(() => {
  for (const host of hosts) host.kill('SIGKILL');
  rmSync(root, { recursive: true, force: true });
});

let runs = 0;
function runDir(): string {
  runs += 1;
  const dir = join(root, `run${runs}`);
  mkdirSync(dir, { mode: 0o700 });
  return dir;
}

function startHost(dir: string): Subprocess {
  const host = Bun.spawn([executable!, dir], { env: { TMPDIR: root }, stdin: 'ignore', stdout: 'ignore', stderr: 'ignore' });
  hosts.push(host);
  return host;
}

const exitWithin = (host: Subprocess, ms: number) => Promise.race([host.exited, Bun.sleep(ms).then(() => null)]);
const state = (seq: number) => ({ type: 'state', seq, pending: [] });

async function waitFor(what: string, ready: () => boolean, ms = 3000): Promise<void> {
  const until = Date.now() + ms;
  while (!ready()) {
    if (Date.now() > until) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(10);
  }
}

/** ccd's end of one connection. */
class Ccd {
  readonly lines: unknown[] = [];
  closed = false;
  #partial = '';

  constructor(readonly socket: Socket) {
    socket.setEncoding('utf8');
    socket.on('data', (chunk: string) => {
      const parts = (this.#partial + chunk).split('\n');
      this.#partial = parts.pop()!;
      for (const text of parts) this.lines.push(JSON.parse(text));
    });
    socket.on('error', () => {});
    socket.on('close', () => {
      this.closed = true;
    });
  }

  write(text: string): void {
    this.socket.write(text);
  }

  send(message: object): void {
    this.write(`${JSON.stringify(message)}\n`);
  }

  /** Sends `attach {after}` and returns the `count` lines it brings: the replay, then the state. */
  async attach(after: number, count: number): Promise<unknown[]> {
    const from = this.lines.length;
    this.send({ type: 'attach', after });
    await waitFor(`${count} lines after attach ${after}`, () => {
      if (this.closed) throw new Error('the host closed the connection');
      return this.lines.length >= from + count;
    });
    return this.lines.slice(from);
  }

  /** Closes this end and waits for the host's: by then the host has read every line sent before. */
  async close(): Promise<void> {
    this.socket.end();
    await waitFor('the connection to close', () => this.closed);
  }
}

async function open(dir: string): Promise<Ccd> {
  const path = join(dir, 'host.sock');
  const until = Date.now() + 3000;
  for (;;) {
    const socket = await new Promise<Socket | null>((resolve) => {
      const attempt = connect(path);
      attempt.once('connect', () => resolve(attempt));
      attempt.once('error', () => resolve(null));
    });
    if (socket) return new Ccd(socket);
    if (Date.now() > until) throw new Error(`no host listening at ${path}`);
    await Bun.sleep(20);
  }
}

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
  dead.kill('SIGKILL');
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
  ccd.send(goldenLine('to_host', 'start'));
  for (const { code, at } of await exits) {
    expect(code).toBe(4);
    expect(at).toBeGreaterThan(29_500);
    expect(at).toBeLessThan(32_000);
  }
  await Bun.sleep(35_000 - (performance.now() - began));
  expect(started.exitCode).toBeNull();
  expect(await ccd.attach(1, 1)).toStrictEqual([state(2)]);
}, 40_000);

test('sequence numbers run 1, 2, 3 across connections', async () => {
  const dir = runDir();
  startHost(dir);
  const states = [];
  for (let after = 0; after < 3; after += 1) states.push(...(await (await open(dir)).attach(after, 1)));
  expect(states).toStrictEqual([state(1), state(2), state(3)]);
}, 10_000);

test('attach {after} replays every kept message after it, in order, then one state', async () => {
  const dir = runDir();
  startHost(dir);
  const ccd = await open(dir);
  for (let count = 1; count <= 4; count += 1) await ccd.attach(0, count);
  const later = await open(dir);
  expect(await later.attach(2, 3)).toStrictEqual([state(3), state(4), state(5)]);
  await Bun.sleep(1000);
  expect(later.lines.length).toBe(3);
  // The cursor is committed: what attach {2} dropped is not replayed again.
  expect(await later.attach(0, 4)).toStrictEqual([state(3), state(4), state(5), state(6)]);
}, 10_000);

test('kept messages outlive a connection; an ack drops them', async () => {
  const dir = runDir();
  startHost(dir);
  const first = await open(dir);
  await first.attach(0, 1);
  await first.attach(0, 2);
  first.send({ type: 'ack', seq: 1 });
  await first.close();
  expect(await (await open(dir)).attach(0, 2)).toStrictEqual([state(2), state(3)]);
}, 10_000);

test('a newer attach replaces the attached connection, which is read until then and never after', async () => {
  const dir = runDir();
  startHost(dir);
  const older = await open(dir);
  await older.attach(0, 1);
  await older.attach(0, 2);
  const newer = await open(dir);
  await Bun.sleep(200);
  older.send({ type: 'ack', seq: 1 });
  expect(await older.attach(0, 2)).toStrictEqual([state(2), state(3)]);
  expect(await newer.attach(0, 3)).toStrictEqual([state(2), state(3), state(4)]);
  await waitFor('the host to close the older connection', () => older.closed);
  older.send({ type: 'ack', seq: 4 });
  await Bun.sleep(200);
  expect(await (await open(dir)).attach(0, 4)).toStrictEqual([state(2), state(3), state(4), state(5)]);
  expect(older.lines.length).toBe(5);
}, 10_000);

test('whole lines only: a line split across writes is applied, a partial line at close is not', async () => {
  const dir = runDir();
  startHost(dir);
  const first = await open(dir);
  for (let count = 1; count <= 3; count += 1) await first.attach(0, count);
  first.write('{"type":"ack",');
  await Bun.sleep(100);
  first.write('"seq":1}\n');
  expect(await first.attach(0, 3)).toStrictEqual([state(2), state(3), state(4)]);
  first.write('{"type":"ack","seq":3}');
  await first.close();
  expect(await (await open(dir)).attach(0, 4)).toStrictEqual([state(2), state(3), state(4), state(5)]);
}, 10_000);

test('a connection gone during the replay does not end the host', async () => {
  const dir = runDir();
  const host = startHost(dir);
  const ccd = await open(dir);
  for (let count = 1; count <= 50; count += 1) await ccd.attach(0, count);
  const gone = await open(dir);
  gone.send({ type: 'attach', after: 0 });
  gone.socket.destroy();
  await Bun.sleep(300);
  expect(host.exitCode).toBeNull();
  const replay = await (await open(dir)).attach(0, 52);
  expect(replay.at(-1)).toStrictEqual(state(52));
}, 10_000);

test('a line that is not JSON is dropped, and the host goes on', async () => {
  const dir = runDir();
  const host = startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  ccd.write('not json\n');
  expect(await ccd.attach(0, 2)).toStrictEqual([state(1), state(2)]);
  expect(host.exitCode).toBeNull();
}, 10_000);
