// The run's start on the built executable, with the stand-in claude (stub-claude.ts) as its claude, seen from both
// sides: what reaches claude (its arguments, its environment, its input) and what reaches ccd (the outcome, the SDK's
// messages, the state, claude's end). The initialize answer is a recorded claude's own (2.1.319), with a marker email
// and organization added here, which must never leave the host.
import { afterAll, expect, test } from 'bun:test';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { cleanup, exitWithin, goldenLine, goldenLines, open, recordedInit, runDir, sessionInit, sessionQueued, startHost, type StubScript, stubLog, waitFor, writeStub } from './rig.ts';

afterAll(cleanup);

const recorded = JSON.parse(recordedInit);
const marker = { email: 'marker-email@example.invalid', organization: 'Marker Organization 7f3a' };
const markedInit = JSON.stringify({
  ...recorded,
  response: { ...recorded.response, response: { ...recorded.response.response, account: { ...recorded.response.response.account, ...marker } } },
});
const [trustedStart, untrustedStart] = [goldenLines('to_host', 'start').find((m) => m.options.trust === 'trusted')!,
  goldenLines('to_host', 'start').find((m) => m.options.trust === 'not_trusted')!];

/** A host with a start sent: its claude is the stand-in following `script`, or `claude` when given. */
async function run(base: any, script: StubScript = {}, opts: { env?: Record<string, string>; hostCwd?: string; claude?: string } = {}) {
  const dir = runDir();
  const work = join(dir, 'work');
  mkdirSync(work);
  const claude = opts.claude ?? writeStub(dir, { init: markedInit, ...script });
  const start = { ...base, options: { ...base.options, cwd: work, claude_path: claude } };
  const host = startHost(dir, opts.env, opts.hostCwd);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  ccd.send(start);
  const outcome = await ccd.line('the start outcome', (line) => line.type === 'outcome');
  return { dir, host, ccd, start, outcome, stub: () => stubLog(dir) };
}

const argvOf = (log: any[]) => log.find((event) => event.argv)!.argv as string[];
const stdinOf = (log: any[]) => log.filter((event) => event.stdin).map((event) => event.stdin);
const noUnexpected = (log: any[]) => expect(log.filter((event) => event.unexpected)).toStrictEqual([]);

test("a trusted folder's options reach claude: no mode when none is named, the worktree, the mail server, the echo, and the SDK's own setting sources", async () => {
  const { start, stub } = await run(trustedStart);
  const argv = argvOf(stub());
  expect(start.options.permission_mode).toBeUndefined();
  expect(argv.filter((arg) => arg.startsWith('--permission-mode'))).toStrictEqual([]);
  expect(argv[argv.indexOf('--worktree') + 1]).toBe(start.options.worktree);
  expect(argv).toContain('--replay-user-messages');
  const mcp = argv[argv.indexOf('--mcp-config') + 1]!;
  expect(JSON.parse(mcp)).toStrictEqual({ mcpServers: { codeconnect: { type: 'stdio', ...start.options.mail } } });
  expect(argv.filter((arg) => arg.startsWith('--setting-sources') || arg === '--strict-mcp-config')).toStrictEqual([]);
  expect(argv.filter((arg) => arg.startsWith('--resume') || arg.startsWith('--model'))).toStrictEqual([]);
  noUnexpected(stub());
}, 10_000);

test("a folder not trusted reads only the person's settings and only CodeConnect's MCP server; resume and model reach claude", async () => {
  const { start, stub } = await run(untrustedStart);
  const argv = argvOf(stub());
  expect(argv).toContain('--setting-sources=user');
  expect(argv).toContain('--strict-mcp-config');
  expect(argv).toContain(`--resume=${start.options.resume}`);
  expect(argv).toContain(`--model=${start.options.model}`);
  expect(argv).toContain(`--permission-mode=${start.options.permission_mode}`);
  expect(argv).not.toContain('--worktree');
}, 10_000);

test("the initialize request asks for Claude Code's own system prompt, not an empty one", async () => {
  const { stub } = await run(trustedStart);
  const initialize = stdinOf(stub()).find((message) => message.request?.subtype === 'initialize');
  expect(initialize).toBeDefined();
  expect(Object.keys(initialize.request)).not.toContain('systemPrompt');
}, 10_000);

test("the first message is the start's prompt, stamped with the start's uuid", async () => {
  const { start, stub } = await run(trustedStart);
  await waitFor('claude to read the first message', () => stdinOf(stub()).some((m) => m.type === 'user'));
  const first = stdinOf(stub()).find((message) => message.type === 'user');
  expect(first.uuid).toBe(start.uuid);
  expect(first.message).toStrictEqual({ role: 'user', content: start.prompt });
}, 10_000);

test("claude is the host's own child, in the host's process group, and host.json names the host alone", async () => {
  const { dir, host, stub } = await run(trustedStart);
  const pid = stub().find((event) => event.argv).pid;
  const [ppid, pgid] = execFileSync('/bin/ps', ['-o', 'ppid=,pgid=', '-p', String(pid)], { encoding: 'utf8' }).trim().split(/\s+/).map(Number);
  expect(ppid).toBe(host.pid);
  expect(pgid).toBe(host.pid);
  expect(JSON.parse(readFileSync(join(dir, 'host.json'), 'utf8'))).toStrictEqual({
    protocol_version: goldenLine('host_json').protocol_version,
    host_pid: host.pid,
  });
  expect(statSync(join(dir, 'host.json')).mode & 0o777).toBe(0o600);
}, 10_000);

test("the start's outcome carries the commands, the models and only the account's provider: the email and organization never leave the host", async () => {
  const { dir, start, outcome, ccd } = await run(trustedStart, { after_user: [sessionInit] });
  expect(outcome).toStrictEqual({
    type: 'outcome',
    seq: outcome.seq,
    uuid: start.uuid,
    status: 'done',
    initialize: {
      commands: recorded.response.response.commands,
      models: recorded.response.response.models,
      account: { api_provider: recorded.response.response.account.apiProvider },
    },
  });
  await ccd.line("the stand-in's init", (line) => line.type === 'sdk');
  for (const text of [JSON.stringify(ccd.lines), readFileSync(join(dir, 'host.json'), 'utf8'), readFileSync(join(dir, 'host.log'), 'utf8')]) {
    expect(text).not.toContain(marker.email);
    expect(text).not.toContain(marker.organization);
  }
}, 10_000);

test("every message the query yields reaches ccd untouched, and the state then names the init's session, model and mode", async () => {
  const { ccd } = await run(trustedStart, { after_user: [sessionInit] });
  const sdk = await ccd.line("the stand-in's init", (line) => line.type === 'sdk');
  const init = JSON.parse(sessionInit);
  expect(sdk.message).toStrictEqual(init);
  const [state] = (await ccd.attach(ccd.lines.at(-1).seq, 1)).slice(-1);
  expect(state).toStrictEqual({
    type: 'state',
    seq: state.seq,
    session_id: init.session_id,
    model: init.model,
    permission_mode: init.permissionMode,
    pending: [],
  });
}, 10_000);

test("the start's outcome comes before every SDK line, even one claude writes before it answers initialize", async () => {
  const { ccd } = await run(trustedStart, { before_init: [sessionQueued] });
  const sdk = await ccd.line('the line claude wrote first', (line) => line.type === 'sdk');
  expect(sdk.message).toStrictEqual(JSON.parse(sessionQueued));
  expect(ccd.lines.map((line) => line.type).slice(0, 3)).toStrictEqual(['state', 'outcome', 'sdk']);
}, 10_000);

test("claude's end is the last message; the host exits 0 once ccd has acknowledged it, and not before", async () => {
  const { host, ccd } = await run(trustedStart, { after_user: [sessionInit], exit_on_user: 3 });
  const exited = await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(exited).toStrictEqual({ type: 'exited', seq: exited.seq, code: 3 });
  expect(ccd.lines.map((line) => line.type).slice(1)).toStrictEqual(['outcome', 'sdk', 'exited']);
  await Bun.sleep(500);
  expect(host.exitCode).toBeNull();
  // An attach whose cursor stops short of the end gets the end again, then a state, and the host stays.
  expect((await ccd.attach(exited.seq - 1, 2)).map((line) => line.type)).toStrictEqual(['exited', 'state']);
  expect(host.exitCode).toBeNull();
  ccd.ackAll();
  expect(await exitWithin(host, 3000)).toBe(0);
}, 10_000);

test("an attach whose cursor is at claude's end is the end's acknowledgement: the host exits 0", async () => {
  const { dir, host, ccd } = await run(trustedStart, { exit_on_user: 3 });
  const exited = await ccd.line("claude's end", (line) => line.type === 'exited');
  await Bun.sleep(500);
  expect(host.exitCode).toBeNull();
  // As a ccd that committed the end and died before its ack: it re-attaches with that cursor.
  (await open(dir)).send({ type: 'attach', after: exited.seq });
  expect(await exitWithin(host, 3000)).toBe(0);
}, 10_000);

test('an initialize claude answers with an error: refused, the query closed, its end, then the host exits', async () => {
  const { host, ccd, outcome, stub } = await run(trustedStart, { init_error: true });
  expect(outcome.status).toBe('refused');
  const exited = await ccd.line("claude's end once its input is closed", (line) => line.type === 'exited');
  expect(exited.seq).toBeGreaterThan(outcome.seq);
  expect(stub().some((event) => event.eof)).toBe(true);
  ccd.ackAll();
  expect(await exitWithin(host, 3000)).toBe(0);
}, 10_000);

test('a refused start sends no SDK line, even one claude wrote before refusing: only its end, then the host exits', async () => {
  const { host, ccd, outcome } = await run(trustedStart, { before_init: [sessionQueued], init_error: true });
  expect(outcome.status).toBe('refused');
  await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(ccd.lines.map((line) => line.type)).toStrictEqual(['state', 'outcome', 'exited']);
  ccd.ackAll();
  expect(await exitWithin(host, 3000)).toBe(0);
}, 10_000);

test('a claude that is not there: the start is refused with the reason, and the host exits once that is acknowledged', async () => {
  const { host, ccd, outcome } = await run(trustedStart, {}, { claude: '/nonexistent/claude' });
  expect(outcome.status).toBe('refused');
  expect(outcome.detail).toContain('/nonexistent/claude');
  await Bun.sleep(500);
  expect(ccd.lines.filter((line) => line.type === 'exited')).toStrictEqual([]);
  ccd.ackAll();
  expect(await exitWithin(host, 3000)).toBe(0);
}, 10_000);

test('a claude that dies at initialize: refused, then its end, then the host exits once both are acknowledged', async () => {
  const { host, ccd, outcome } = await run(trustedStart, { die_on_init: true });
  expect(outcome.status).toBe('refused');
  const exited = await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(exited.code).toBe(1);
  expect(exited.seq).toBeGreaterThan(outcome.seq);
  ccd.ackAll();
  expect(await exitWithin(host, 3000)).toBe(0);
}, 10_000);

test("claude gets the host's environment: ccd's session uid, no CLAUDE_CONFIG_DIR, and nothing from a .env where the host starts", async () => {
  const hostCwd = runDir();
  writeFileSync(join(hostCwd, '.env'), 'CC_DOTENV_LEAK=leaked\n');
  const { stub } = await run(trustedStart, {}, { env: { CODECONNECT_SESSION_UID: 'uid-c2-test' }, hostCwd });
  const env = stub().find((event) => event.argv).env;
  expect(env.CODECONNECT_SESSION_UID).toBe('uid-c2-test');
  expect(env.CC_DOTENV_LEAK).toBeUndefined();
  expect(env.CLAUDE_CONFIG_DIR).toBeUndefined();
  // The SDK's own mark, so the environment above is the one the SDK passed.
  expect(env.CLAUDE_CODE_ENTRYPOINT).toBeDefined();
}, 10_000);

test('a start after the first is ignored, while its initialize is outstanding and after its outcome: one claude, one outcome', async () => {
  const dir = runDir();
  const work = join(dir, 'work');
  mkdirSync(work);
  const start = { ...trustedStart, options: { ...trustedStart.options, cwd: work, claude_path: writeStub(dir, { init: markedInit }) } };
  startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  // The second arrives with the first, before claude has even been asked to initialize.
  ccd.write(`${JSON.stringify(start)}\n${JSON.stringify(start)}\n`);
  await ccd.line('the start outcome', (line) => line.type === 'outcome');
  ccd.send({ ...start, uuid: '0b8f3e2a-5c41-4d7e-9a6b-2e1f0c9d8a7b' });
  await Bun.sleep(1500);
  expect(ccd.lines.filter((line) => line.type === 'outcome').length).toBe(1);
  expect(stubLog(dir).filter((event) => event.argv).length).toBe(1);
}, 10_000);
