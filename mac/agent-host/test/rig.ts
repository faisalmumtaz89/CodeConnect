// What the host's tests share: the golden lines, the built executable started as ccd starts it, ccd's end of a
// connection, and the stand-in claude (stub-claude.ts) a run can be given as its claude.
import { spawn } from 'node:child_process';
import { chmodSync, closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { connect, type Socket } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export const executable = process.env.CC_AGENT_HOST!;
if (!executable) throw new Error('CC_AGENT_HOST must name the built host (mac/agent-host/test.sh sets it)');

const repo = join(import.meta.dir, '..', '..', '..');
export const fixture = (path: string) => readFileSync(join(repo, 'fixtures', path), 'utf8');
export const golden = fixture('host/messages.jsonl')
  .split('\n')
  .filter((text) => text !== '')
  .map((text) => JSON.parse(text) as { dir: string; message: Record<string, any> });
export const goldenLines = (dir: string, type?: string) =>
  golden.filter((entry) => entry.dir === dir && (type === undefined || entry.message.type === type)).map((e) => e.message);
export const goldenLine = (dir: string, type?: string) => goldenLines(dir, type)[0]!;

// One folder for the run dirs of the test file running now: bun runs every file in one process, and each file's
// cleanup removes its own.
let root: string | undefined;
let runs = 0;
export function runDir(): string {
  root ??= mkdtempSync(join(tmpdir(), 'cch-'));
  runs += 1;
  const dir = join(root, `run${runs}`);
  mkdirSync(dir, { mode: 0o700 });
  return dir;
}

export interface Host {
  pid: number;
  readonly exitCode: number | null;
  exited: Promise<number | null>;
}
const hosts: Host[] = [];

/**
 * The executable started as ccd starts it: in a session of its own, its stdout and stderr in host.log (0600) in the
 * run dir, with an environment of its own that holds no BUN_BE_BUN.
 */
export function startHost(dir: string, env: Record<string, string> = {}, cwd: string = dir): Host {
  const log = openSync(join(dir, 'host.log'), 'a', 0o600);
  const child = spawn(executable, [dir], { cwd, env: { TMPDIR: root!, ...env }, detached: true, stdio: ['ignore', log, log] });
  closeSync(log);
  let code: number | null = null;
  let done = false;
  const host: Host = {
    pid: child.pid!,
    get exitCode() {
      return done ? code : null;
    },
    exited: new Promise((resolve) => child.once('exit', (exitCode) => {
      code = exitCode;
      done = true;
      resolve(exitCode);
    })),
  };
  hosts.push(host);
  return host;
}

/** Ends every host the file started, with claude and anything else in its process group, and removes its folder. */
export function cleanup(): void {
  for (const host of hosts.splice(0)) {
    try {
      process.kill(-host.pid, 'SIGKILL');
    } catch {
      // Already gone.
    }
  }
  if (root) rmSync(root, { recursive: true, force: true });
  root = undefined;
}

export const exitWithin = (host: Host, ms: number) => Promise.race([host.exited, Bun.sleep(ms).then(() => null)]);
export const state = (seq: number, facts: Record<string, unknown> = {}) => ({ type: 'state', seq, ...facts, pending: [] });

export async function waitFor(what: string, ready: () => boolean, ms = 3000): Promise<void> {
  const until = Date.now() + ms;
  while (!ready()) {
    if (Date.now() > until) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(10);
  }
}

/** ccd's end of one connection. */
export class Ccd {
  readonly lines: any[] = [];
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
  async attach(after: number, count: number): Promise<any[]> {
    const from = this.lines.length;
    this.send({ type: 'attach', after });
    await waitFor(`${count} lines after attach ${after}`, () => {
      if (this.closed) throw new Error('the host closed the connection');
      return this.lines.length >= from + count;
    });
    return this.lines.slice(from);
  }

  /** The first line that `test` accepts, once it has come. */
  async line(what: string, test: (line: any) => boolean, ms = 5000): Promise<any> {
    await waitFor(what, () => this.lines.some(test), ms);
    return this.lines.find(test);
  }

  /** Acknowledges everything received so far. */
  ackAll(): void {
    const last = this.lines.at(-1);
    if (last) this.send({ type: 'ack', seq: last.seq });
  }

  /** Closes this end and waits for the host's: by then the host has read every line sent before. */
  async close(): Promise<void> {
    this.socket.end();
    await waitFor('the connection to close', () => this.closed);
  }
}

export async function open(dir: string): Promise<Ccd> {
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

/** What the stand-in claude does (stub-claude.ts). */
export interface StubScript {
  /** The initialize answer, one control_response line. */
  init?: string;
  /** Lines written at the initialize request, before its answer. */
  before_init?: string[];
  /** Lines written after the first user message. */
  after_user?: string[];
  /** Exit with this code once the first user message has come. */
  exit_on_user?: number;
  /** Exit 1 at the initialize request, unanswered. */
  die_on_init?: boolean;
  /** Answer the initialize request with an error, and stay. */
  init_error?: boolean;
  /** Answer an interrupt with a receipt listing `cancelled`, then write `lines` (or the lines first). */
  interrupt?: { cancelled?: string[]; lines?: string[]; result_first?: boolean };
  /** A get_usage request's answer. */
  usage?: Record<string, unknown>;
  /** Settings requests: refused with the error given for their subtype; set_model answered after `delay_ms`. */
  settings?: { refuse?: Record<string, string>; delay_ms?: number };
}

/** Writes the stand-in claude for run `dir` (a sh wrapper running stub-claude.ts on the built host as Bun). */
export function writeStub(dir: string, script: StubScript): string {
  const file = join(dir, 'stub.json');
  writeFileSync(file, JSON.stringify({ ...script, log: join(dir, 'stub.log') }));
  const wrapper = join(dir, 'claude');
  writeFileSync(wrapper, `#!/bin/sh\nBUN_BE_BUN=1 exec '${executable}' '${join(import.meta.dir, 'stub-claude.ts')}' '${file}' "$@"\n`);
  chmodSync(wrapper, 0o755);
  return wrapper;
}

/** A recorded claude's own initialize answer (2.1.319), and the system/init line of a recorded SDK session. */
export const recordedInit = fixture('claude/initialize-api-key-2.1.319.jsonl').split('\n')[0]!;
/** The recorded SDK session's first line: its first message's `command_lifecycle` "queued", ahead of system/init. */
export const sessionQueued = fixture('claude/sdk-session-0.3.322.jsonl').split('\n')[0]!;
export const sessionInit = fixture('claude/sdk-session-0.3.322.jsonl')
  .split('\n')
  .find((text) => text.includes('"type":"system"') && text.includes('"subtype":"init"'))!;

/**
 * A host for run `dir` with a run started: ccd attaches (state 1), sends the golden start, and the stand-in claude
 * writes `count` SDK lines after the first message. Resolves once the outcome and every line have come, in the order
 * state, outcome, then the SDK lines; the stand-in then stays until its input ends.
 */
export async function runWith(dir: string, count: number): Promise<Ccd> {
  const start = goldenLine('to_host', 'start');
  const claude = writeStub(dir, { init: recordedInit, after_user: Array(count).fill(sessionInit) });
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  ccd.send({ ...start, options: { ...start.options, cwd: dir, claude_path: claude } });
  await waitFor(`the outcome and ${count} SDK lines`, () => ccd.lines.length >= 2 + count);
  const types = ccd.lines.map((line) => line.type);
  if (types.join() !== ['state', 'outcome', ...Array(count).fill('sdk')].join()) throw new Error(`lines came as ${types}`);
  return ccd;
}

/** What the stand-in claude of run `dir` recorded, one object per event. */
export function stubLog(dir: string): any[] {
  try {
    return readFileSync(join(dir, 'stub.log'), 'utf8').split('\n').filter((t) => t !== '').map((t) => JSON.parse(t));
  } catch {
    return [];
  }
}
