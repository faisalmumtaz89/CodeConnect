// The run's start on the built executable, with the stand-in claude (stub-claude.ts) as its claude, seen from both
// sides: what reaches claude (its arguments, its environment, its input) and what reaches ccd (the outcome, the SDK's
// messages, the state, claude's end). The initialize answer is a recorded claude's own (2.1.319), with a marker email
// and organization added here, which must never leave the host.
import { afterAll, expect, test } from 'bun:test';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { cleanup, exitWithin, fixture, goldenLine, goldenLines, open, recordedInit, runDir, sessionInit, sessionQueued, startHost, type StubScript, stubLog, waitFor, writeStub } from './rig.ts';

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
/** The folders claude was given beside its own, in either spelling of the flag. */
const addDirs = (argv: string[]) =>
  argv.flatMap((arg, i) => (arg === '--add-dir' ? [argv[i + 1]] : arg.startsWith('--add-dir=') ? [arg.slice('--add-dir='.length)] : []));
const noUnexpected = (log: any[]) => expect(log.filter((event) => event.unexpected)).toStrictEqual([]);

test("a trusted folder's options reach claude: no mode when none is named, the SDK's guard for Bypass permissions, the worktree, the mail server, its two reading tools allowed, the echo, and the SDK's own setting sources", async () => {
  const { start, stub } = await run(trustedStart);
  const argv = argvOf(stub());
  expect(start.options.permission_mode).toBeUndefined();
  expect(argv.filter((arg) => arg.startsWith('--permission-mode'))).toStrictEqual([]);
  expect(argv).toContain('--allow-dangerously-skip-permissions');
  expect(argv[argv.indexOf('--worktree') + 1]).toBe(start.options.worktree);
  // The run's uploads folder, and only it, is claude's to read beside its own.
  expect(addDirs(argv)).toStrictEqual(start.options.additional_directories);
  expect(argv).toContain('--replay-user-messages');
  const mcp = argv[argv.indexOf('--mcp-config') + 1]!;
  expect(JSON.parse(mcp)).toStrictEqual({ mcpServers: { codeconnect: { type: 'stdio', ...start.options.mail } } });
  expect(argv.filter((arg) => arg.startsWith('--setting-sources') || arg === '--strict-mcp-config')).toStrictEqual([]);
  expect(argv.filter((arg) => arg.startsWith('--resume') || arg.startsWith('--model') || arg.startsWith('--effort'))).toStrictEqual([]);
  // CodeConnect's two reading tools need no card in a trusted folder (R-3); sending one still asks.
  expect(argv.filter((arg) => arg.startsWith('--allowedTools'))).toStrictEqual([
    '--allowedTools=mcp__codeconnect__msg_sessions,mcp__codeconnect__msg_read',
  ]);
  noUnexpected(stub());
}, 10_000);

test("a folder not trusted reads only the person's settings and only CodeConnect's MCP server, allowing none of its tools; resume, model and effort reach claude", async () => {
  const { start, stub } = await run(untrustedStart);
  const argv = argvOf(stub());
  expect(argv).toContain('--setting-sources=user');
  expect(argv).toContain('--strict-mcp-config');
  expect(argv).toContain(`--resume=${start.options.resume}`);
  expect(argv).toContain(`--model=${start.options.model}`);
  // A relaunch's effort, the one the run ended with, is claude's from its creation.
  expect(argv).toContain(`--effort=${start.options.effort}`);
  expect(argv).toContain(`--permission-mode=${start.options.permission_mode}`);
  expect(argv.filter((arg) => arg.startsWith('--permission-mode'))).toHaveLength(1);
  expect(argv).toContain('--allow-dangerously-skip-permissions');
  expect(argv).not.toContain('--worktree');
  expect(addDirs(argv)).toStrictEqual([]);
  // A folder not trusted keeps every card, CodeConnect's own tools' included.
  expect(argv.filter((arg) => arg.startsWith('--allowedTools'))).toStrictEqual([]);
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

test("a start with an empty prompt gives claude no message and is answered done; the first say is claude's first message", async () => {
  const { ccd, outcome, stub } = await run({ ...trustedStart, prompt: '' });
  expect(outcome.status).toBe('done');
  ccd.send({ type: 'say', uuid: 'c-first', text: 'the first message' });
  await ccd.line('the say taken', (line) => line.type === 'outcome' && line.uuid === 'c-first');
  await waitFor('claude to read it', () => stdinOf(stub()).some((m) => m.type === 'user'));
  const users = stdinOf(stub()).filter((message) => message.type === 'user');
  expect(users.map((m) => [m.uuid, m.message.content])).toStrictEqual([['c-first', 'the first message']]);
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

test("the start's outcome carries the commands, the models, claude's version and only the account's provider: the email and organization never leave the host", async () => {
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
      claude_code_version: recorded.response.response.claude_code_version,
    },
  });
  await ccd.line("the stand-in's init", (line) => line.type === 'sdk');
  for (const text of [JSON.stringify(ccd.lines), readFileSync(join(dir, 'host.json'), 'utf8'), readFileSync(join(dir, 'host.log'), 'utf8')]) {
    expect(text).not.toContain(marker.email);
    expect(text).not.toContain(marker.organization);
  }
}, 10_000);

test("the start's outcome leaves out the claude.ai connectors' commands, which name the account's connected services, and keeps every other command", async () => {
  // Made up: no recorded answer comes from an account with connectors. The rows take the form claude 2.1.322's shipped
  // text gives them (the SDK's pinned build, checked against bun.lock, never run), not a live answer's: a person's own
  // command, a claude.ai connector's prompt and an MCP server's prompt.
  const own = { name: 'review-diff', description: 'Review the diff (user)', argumentHint: '' };
  const connector = { name: 'claude.ai Marker Connector 7f3a:search (MCP)', description: 'A connector command', argumentHint: '' };
  const prompt = { name: 'notes:summarize (MCP)', description: 'Summarize the notes', argumentHint: '' };
  const answer = JSON.parse(markedInit);
  answer.response.response.commands = [...answer.response.response.commands, own, connector, prompt];
  const { dir, outcome, ccd } = await run(trustedStart, { init: JSON.stringify(answer), after_user: [sessionInit] });
  expect(outcome.initialize.commands).toStrictEqual([...recorded.response.response.commands, own, prompt]);
  await ccd.line("the stand-in's init", (line) => line.type === 'sdk');
  for (const text of [JSON.stringify(ccd.lines), readFileSync(join(dir, 'host.json'), 'utf8'), readFileSync(join(dir, 'host.log'), 'utf8')]) {
    expect(text).not.toContain(connector.name);
  }
}, 10_000);

test("a commands_changed reaches ccd without the claude.ai connectors' commands, and with every other command as claude wrote it", async () => {
  // The recorded session's own commands_changed, with the made-up rows of the outcome's test added.
  const own = { name: 'review-diff', description: 'Review the diff (user)', argumentHint: '' };
  const connector = { name: 'claude.ai Marker Connector 7f3a:search (MCP)', description: 'A connector command', argumentHint: '' };
  const prompt = { name: 'notes:summarize (MCP)', description: 'Summarize the notes', argumentHint: '' };
  const changed = JSON.parse(fixture('claude/sdk-session-0.3.322.jsonl').split('\n')[4]!);
  expect(changed.subtype).toBe('commands_changed');
  const marked = { ...changed, commands: [...changed.commands, own, connector, prompt] };
  const { dir, ccd } = await run(trustedStart, { after_user: [sessionInit, JSON.stringify(marked)] });
  const sdk = await ccd.line('the commands_changed', (line) => line.type === 'sdk' && line.message.subtype === 'commands_changed');
  expect(sdk.message).toStrictEqual({ ...changed, commands: [...changed.commands, own, prompt] });
  for (const text of [JSON.stringify(ccd.lines), readFileSync(join(dir, 'host.log'), 'utf8')]) {
    expect(text).not.toContain(connector.name);
  }
}, 10_000);

test("every other message the query yields reaches ccd untouched, and the state then names the init's session, model and mode", async () => {
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

test("each say reaches claude's input after the first message, in order, marked as the person's and stamped with its uuid, and is answered done", async () => {
  const { ccd, start, stub } = await run(trustedStart);
  ccd.send({ type: 'say', uuid: 'c-say-1', text: 'the second message' });
  ccd.send({ type: 'say', uuid: 'c-say-2', text: 'the third message' });
  await ccd.line('the second say taken', (line) => line.type === 'outcome' && line.uuid === 'c-say-2');
  await waitFor('claude to read both', () => stdinOf(stub()).filter((m) => m.type === 'user').length === 3);
  const users = stdinOf(stub()).filter((message) => message.type === 'user');
  expect(users.map((m) => [m.uuid, m.message.content])).toStrictEqual([
    [start.uuid, start.prompt],
    ['c-say-1', 'the second message'],
    ['c-say-2', 'the third message'],
  ]);
  for (const message of users) expect(message.origin).toStrictEqual({ kind: 'human' });
  const outcomes = ccd.lines.filter((line) => line.type === 'outcome' && line.uuid.startsWith('c-say'));
  expect(outcomes.map((line) => [line.uuid, line.status])).toStrictEqual([['c-say-1', 'done'], ['c-say-2', 'done']]);
}, 10_000);

test("a wake reaches claude's input in its place among the says, stamped with its uuid and marked as a background notification, not the person's words, and is answered done", async () => {
  const { ccd, start, stub } = await run(trustedStart);
  const wake = goldenLine('to_host', 'wake');
  ccd.send({ type: 'say', uuid: 'c-say-1', text: 'the second message' });
  ccd.send(wake);
  await ccd.line('the wake taken', (line) => line.type === 'outcome' && line.uuid === wake.uuid);
  await waitFor('claude to read it', () => stdinOf(stub()).filter((m) => m.type === 'user').length === 3);
  const users = stdinOf(stub()).filter((message) => message.type === 'user');
  expect(users.map((m) => [m.uuid, m.message.content, m.origin])).toStrictEqual([
    [start.uuid, start.prompt, { kind: 'human' }],
    ['c-say-1', 'the second message', { kind: 'human' }],
    [wake.uuid, wake.text, { kind: 'task-notification' }],
  ]);
  const outcome = ccd.lines.find((line) => line.type === 'outcome' && line.uuid === wake.uuid);
  expect(outcome.status).toBe('done');
}, 10_000);

test('a say whose multibyte character is split across two writes on the socket arrives whole', async () => {
  const { ccd, stub } = await run(trustedStart);
  const text = 'déjà vu — 日本語 ✓';
  const bytes = new TextEncoder().encode(`${JSON.stringify({ type: 'say', uuid: 'c-split', text })}\n`);
  // Inside the first character of 日本語, whose UTF-8 is three bytes.
  const cut = bytes.indexOf(0xe6) + 1;
  ccd.socket.write(bytes.subarray(0, cut));
  await Bun.sleep(100);
  ccd.socket.write(bytes.subarray(cut));
  await ccd.line('the split say taken', (line) => line.type === 'outcome' && line.uuid === 'c-split');
  await waitFor('claude to read it', () => stdinOf(stub()).some((m) => m.uuid === 'c-split'));
  expect(stdinOf(stub()).find((m) => m.uuid === 'c-split').message.content).toBe(text);
}, 10_000);

test('a say still queued when the stop comes is refused, and never reaches claude', async () => {
  const { ccd, stub } = await run(trustedStart);
  // One write: the host reads the say and the stop together, so the say is still queued when the stop ends the input.
  const say = JSON.stringify({ type: 'say', uuid: 'c-queued', text: 'queued' });
  ccd.socket.write(`${say}\n${JSON.stringify({ type: 'stop', uuid: 'c-stop' })}\n`);
  const outcome = await ccd.line('the queued say answered', (line) => line.type === 'outcome' && line.uuid === 'c-queued');
  expect(outcome.status).toBe('refused');
  await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(stdinOf(stub()).some((message) => message.uuid === 'c-queued')).toBe(false);
}, 10_000);

test("a say after claude's own end is refused, and never reaches claude", async () => {
  const { ccd, stub } = await run(trustedStart, { exit_on_user: 3 });
  await ccd.line("claude's end", (line) => line.type === 'exited');
  ccd.send({ type: 'say', uuid: 'c-after-end', text: 'too late' });
  const outcome = await ccd.line('the late say answered', (line) => line.type === 'outcome' && line.uuid === 'c-after-end');
  expect(outcome.status).toBe('refused');
  expect(stdinOf(stub()).some((message) => message.uuid === 'c-after-end')).toBe(false);
}, 10_000);

test('a say once the run is stopped is refused, and never reaches claude', async () => {
  const { ccd, stub } = await run(trustedStart);
  ccd.send({ type: 'stop', uuid: 'c-stop' });
  ccd.send({ type: 'say', uuid: 'c-late', text: 'too late' });
  const outcome = await ccd.line('the late say answered', (line) => line.type === 'outcome' && line.uuid === 'c-late');
  expect(outcome.status).toBe('refused');
  expect(stdinOf(stub()).some((message) => message.uuid === 'c-late')).toBe(false);
}, 10_000);

/** claude asking permission for a tool call, as claude writes it: the SDK hands it to the host's canUseTool. */
const ask = (toolUseID: string, toolName = 'Bash', input: object = { command: 'ls' }, extra: object = {}) =>
  JSON.stringify({
    type: 'control_request',
    request_id: `req-${toolUseID}`,
    request: { subtype: 'can_use_tool', tool_name: toolName, input, tool_use_id: toolUseID, ...extra },
  });
/** The control responses claude read for the request `request_id`. */
const answersRead = (log: any[], requestID: string) =>
  stdinOf(log).filter((m) => m.type === 'control_response' && m.response?.request_id === requestID);

test("claude's permission request reaches ccd once, with its tool, input and the SDK's options, and the state lists it while it is open", async () => {
  const { ccd } = await run(trustedStart, {
    after_user: [ask('toolu_p1', 'Bash', { command: 'ls' }, { permission_suggestions: [{ type: 'setMode', mode: 'acceptEdits', destination: 'session' }], decision_reason: 'a reason' })],
  });
  const permission = await ccd.line('the permission', (line) => line.type === 'permission');
  expect(permission.request.tool_use_id).toBe('toolu_p1');
  expect(permission.request.tool_name).toBe('Bash');
  expect(permission.request.input).toStrictEqual({ command: 'ls' });
  expect(permission.request.options.suggestions).toStrictEqual([{ type: 'setMode', mode: 'acceptEdits', destination: 'session' }]);
  expect(permission.request.options.decisionReason).toBe('a reason');
  for (const key of ['signal', 'toolUseID', 'requestId']) expect(permission.request.options).not.toHaveProperty(key);
  const [state] = await ccd.attach(permission.seq, 1);
  expect(state.type).toBe('state');
  expect(state.pending.map((request: any) => request.tool_use_id)).toStrictEqual(['toolu_p1']);
}, 10_000);

test("an allow reaches claude's input with the request's input, and is answered done", async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [ask('toolu_a1')] });
  await ccd.line('the permission', (line) => line.type === 'permission');
  ccd.send({ type: 'answer', uuid: 'c-allow', tool_use_id: 'toolu_a1', decision: { behavior: 'allow' } });
  const outcome = await ccd.line('the allow answered', (line) => line.type === 'outcome' && line.uuid === 'c-allow');
  expect(outcome.status).toBe('done');
  await waitFor('claude to read the answer', () => answersRead(stub(), 'req-toolu_a1').length === 1);
  const [read] = answersRead(stub(), 'req-toolu_a1');
  expect(read.response.response).toMatchObject({ behavior: 'allow', updatedInput: { command: 'ls' } });
  const [state] = await ccd.attach(outcome.seq, 1);
  expect(state.pending).toStrictEqual([]);
}, 10_000);

test("an allow for the session reaches claude with ccd's rules as its updatedPermissions, and a plain allow with none", async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [ask('toolu_s1'), ask('toolu_s2')] });
  for (const id of ['toolu_s1', 'toolu_s2']) await ccd.line(`the permission ${id}`, (line) => line.type === 'permission' && line.request.tool_use_id === id);
  const rules = [
    { type: 'addRules', rules: [{ toolName: 'Bash', ruleContent: 'ls' }], behavior: 'allow', destination: 'session' },
    { type: 'addRules', rules: [{ toolName: 'Read' }], behavior: 'allow', destination: 'session' },
  ];
  ccd.send({ type: 'answer', uuid: 'c-session', tool_use_id: 'toolu_s1', decision: { behavior: 'allow', updated_permissions: rules } });
  ccd.send({ type: 'answer', uuid: 'c-plain', tool_use_id: 'toolu_s2', decision: { behavior: 'allow' } });
  for (const uuid of ['c-session', 'c-plain']) {
    expect((await ccd.line(`${uuid} answered`, (line) => line.type === 'outcome' && line.uuid === uuid)).status).toBe('done');
  }
  await waitFor('claude to read both allows', () => answersRead(stub(), 'req-toolu_s1').length === 1 && answersRead(stub(), 'req-toolu_s2').length === 1);
  const session = answersRead(stub(), 'req-toolu_s1')[0].response.response;
  expect(session).toMatchObject({ behavior: 'allow', updatedInput: { command: 'ls' } });
  expect(session.updatedPermissions).toStrictEqual(rules);
  expect(answersRead(stub(), 'req-toolu_s2')[0].response.response).not.toHaveProperty('updatedPermissions');
}, 10_000);

test('a deny reaches claude with its message, and interrupts only when ccd says so', async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [ask('toolu_d1'), ask('toolu_d2')] });
  for (const id of ['toolu_d1', 'toolu_d2']) await ccd.line(`the permission ${id}`, (line) => line.type === 'permission' && line.request.tool_use_id === id);
  ccd.send({ type: 'answer', uuid: 'c-deny', tool_use_id: 'toolu_d1', decision: { behavior: 'deny', message: 'not that', interrupt: true } });
  // A subagent's deny, or one with the person's reason: ccd sends no `interrupt`.
  ccd.send({ type: 'answer', uuid: 'c-deny-on', tool_use_id: 'toolu_d2', decision: { behavior: 'deny', message: 'not this one' } });
  for (const uuid of ['c-deny', 'c-deny-on']) {
    expect((await ccd.line(`${uuid} answered`, (line) => line.type === 'outcome' && line.uuid === uuid)).status).toBe('done');
  }
  await waitFor('claude to read both denies', () => answersRead(stub(), 'req-toolu_d1').length === 1 && answersRead(stub(), 'req-toolu_d2').length === 1);
  expect(answersRead(stub(), 'req-toolu_d1')[0].response.response).toMatchObject({ behavior: 'deny', message: 'not that', interrupt: true });
  expect(answersRead(stub(), 'req-toolu_d2')[0].response.response).toMatchObject({ behavior: 'deny', message: 'not this one', interrupt: false });
}, 10_000);

test('an answer to a request that is not open is refused, and nothing reaches claude', async () => {
  const { ccd, stub } = await run(trustedStart);
  ccd.send({ type: 'answer', uuid: 'c-none', tool_use_id: 'toolu_none', decision: { behavior: 'allow' } });
  const outcome = await ccd.line('the answer refused', (line) => line.type === 'outcome' && line.uuid === 'c-none');
  expect(outcome).toMatchObject({ status: 'refused', detail: 'the request is no longer open' });
  expect(stdinOf(stub()).filter((m) => m.type === 'control_response')).toStrictEqual([]);
}, 10_000);

test('a request still open when the run is stopped is withdrawn, and a later answer to it is refused', async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [ask('toolu_w1')] });
  await ccd.line('the permission', (line) => line.type === 'permission');
  ccd.send({ type: 'stop', uuid: 'c-stop' });
  const withdrawn = await ccd.line('the request withdrawn', (line) => line.type === 'withdrawn');
  expect(withdrawn.tool_use_id).toBe('toolu_w1');
  ccd.send({ type: 'answer', uuid: 'c-late', tool_use_id: 'toolu_w1', decision: { behavior: 'allow' } });
  expect((await ccd.line('the late answer', (line) => line.type === 'outcome' && line.uuid === 'c-late')).status).toBe('refused');
  await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(answersRead(stub(), 'req-toolu_w1')).toStrictEqual([]);
}, 10_000);

test('an allow and a stop read together: the answer never reaches claude, so it is refused, never done', async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [ask('toolu_r1')] });
  await ccd.line('the permission', (line) => line.type === 'permission');
  // One write: the host resolves the request and closes the query before the SDK writes the answer.
  const allow = JSON.stringify({ type: 'answer', uuid: 'c-race', tool_use_id: 'toolu_r1', decision: { behavior: 'allow' } });
  ccd.socket.write(`${allow}\n${JSON.stringify({ type: 'stop', uuid: 'c-stop' })}\n`);
  const outcome = await ccd.line('the raced answer', (line) => line.type === 'outcome' && line.uuid === 'c-race');
  expect(outcome).toMatchObject({ status: 'refused', detail: 'the session ended before the answer reached Claude' });
  await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(answersRead(stub(), 'req-toolu_r1')).toStrictEqual([]);
}, 10_000);

test("a question claude asks reaches ccd as a permission, and its answers reach claude as the call's input", async () => {
  const questions = [{ question: 'Tea or coffee?', header: 'Drink', options: [{ label: 'Tea', description: '' }, { label: 'Coffee', description: '' }], multiSelect: false }];
  const { ccd, stub } = await run(trustedStart, { after_user: [ask('toolu_q1', 'AskUserQuestion', { questions })] });
  const permission = await ccd.line('the question', (line) => line.type === 'permission');
  expect(permission.request).toMatchObject({ tool_use_id: 'toolu_q1', tool_name: 'AskUserQuestion', input: { questions } });
  const answered = { questions, answers: { 'Tea or coffee?': 'Tea' }, annotations: {} };
  ccd.send({ type: 'answer', uuid: 'c-question', tool_use_id: 'toolu_q1', decision: { behavior: 'allow', updated_input: answered } });
  expect((await ccd.line('the answers answered', (line) => line.type === 'outcome' && line.uuid === 'c-question')).status).toBe('done');
  await waitFor('claude to read the answers', () => answersRead(stub(), 'req-toolu_q1').length === 1);
  expect(answersRead(stub(), 'req-toolu_q1')[0].response.response).toMatchObject({ behavior: 'allow', updatedInput: answered });
}, 10_000);

/** The turn the recorded session's first `system/init` opens. */
const runningTurn = JSON.parse(sessionInit).uuid as string;
/** claude's own `result` lines, recorded: a turn an app stopped (`aborted_tools`), and one that completed. */
const recordedLines = fixture('claude/sdk-session-0.3.322.jsonl').split('\n');
const turnResult = (reason = 'aborted_tools') => recordedLines[reason === 'completed' ? 10 : 95]!;
/** The interrupts claude read. */
const interruptsRead = (log: any[]) => stdinOf(log).filter((m) => m.type === 'control_request' && m.request?.subtype === 'interrupt');

test("an app's Stop of the running turn interrupts with cancel_queued, and its outcome follows the turn's result with its reason and the cancelled sends", async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [sessionInit], interrupt: { cancelled: ['c-say'], lines: [turnResult()] } });
  await ccd.line('the turn', (line) => line.type === 'sdk' && line.message.type === 'system');
  ccd.send({ type: 'interrupt', uuid: 'c-stop', turn_id: runningTurn });
  const result = await ccd.line('the turn ended', (line) => line.type === 'sdk' && line.message.type === 'result');
  const outcome = await ccd.line('the stop answered', (line) => line.type === 'outcome' && line.uuid === 'c-stop');
  expect(outcome).toMatchObject({ status: 'done', terminal_reason: 'aborted_tools', cancelled: ['c-say'] });
  expect(outcome.seq).toBeGreaterThan(result.seq);
  expect(interruptsRead(stub()).map((m) => m.request.cancel_queued)).toStrictEqual([true]);
}, 10_000);

test('a turn that ends as the Stop arrives writes its result first: the outcome still waits for the receipt and carries its cancelled sends', async () => {
  const { ccd } = await run(trustedStart, {
    after_user: [sessionInit],
    interrupt: { cancelled: ['c-say'], lines: [turnResult('completed')], result_first: true },
  });
  await ccd.line('the turn', (line) => line.type === 'sdk' && line.message.type === 'system');
  ccd.send({ type: 'interrupt', uuid: 'c-stop', turn_id: runningTurn });
  const outcome = await ccd.line('the stop answered', (line) => line.type === 'outcome' && line.uuid === 'c-stop');
  expect(outcome).toMatchObject({ status: 'done', terminal_reason: 'completed', cancelled: ['c-say'] });
}, 10_000);

test('a Stop of a turn the host is not running is refused and reaches claude nothing: another turn, or one whose result has come', async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [sessionInit], interrupt: { lines: [turnResult()] } });
  await ccd.line('the turn', (line) => line.type === 'sdk' && line.message.type === 'system');
  ccd.send({ type: 'interrupt', uuid: 'c-other', turn_id: 'another-turn' });
  expect((await ccd.line('another turn refused', (line) => line.type === 'outcome' && line.uuid === 'c-other')).status).toBe('refused');
  expect(interruptsRead(stub())).toStrictEqual([]);
  ccd.send({ type: 'interrupt', uuid: 'c-stop', turn_id: runningTurn });
  expect((await ccd.line('the stop answered', (line) => line.type === 'outcome' && line.uuid === 'c-stop')).status).toBe('done');
  ccd.send({ type: 'interrupt', uuid: 'c-late', turn_id: runningTurn });
  expect((await ccd.line('a stop after the result refused', (line) => line.type === 'outcome' && line.uuid === 'c-late')).status).toBe('refused');
  expect(interruptsRead(stub()).length).toBe(1);
}, 10_000);

/** The settings requests claude read, and when each was answered, in order. */
const settingsSeen = (log: any[]) =>
  log.filter((event) => event.setting || event.answered).map((event) => (event.setting ? event.setting : { answered: event.answered }));

test('a model, an effort and a mode reach claude through the SDK, each answered done once claude has taken it', async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [sessionInit] });
  await ccd.line('the turn', (line) => line.type === 'sdk' && line.message.type === 'system');
  ccd.send({ type: 'set_model', uuid: 'c-model', model: 'sonnet' });
  expect((await ccd.line('the model answered', (line) => line.type === 'outcome' && line.uuid === 'c-model')).status).toBe('done');
  ccd.send({ type: 'set_model', uuid: 'c-effort', effort: 'low' });
  expect((await ccd.line('the effort answered', (line) => line.type === 'outcome' && line.uuid === 'c-effort')).status).toBe('done');
  ccd.send({ type: 'set_mode', uuid: 'c-mode', mode: 'plan' });
  expect((await ccd.line('the mode answered', (line) => line.type === 'outcome' && line.uuid === 'c-mode')).status).toBe('done');
  expect(settingsSeen(stub()).filter((seen) => !('answered' in seen))).toStrictEqual([
    { subtype: 'set_model', model: 'sonnet' },
    { subtype: 'apply_flag_settings', settings: { effortLevel: 'low' } },
    { subtype: 'set_permission_mode', mode: 'plan' },
  ]);
  noUnexpected(stub());
}, 10_000);

test("an effort alone never calls setModel, which with no model would reset it; claude's refusal is the outcome's reason", async () => {
  const { ccd, stub } = await run(trustedStart, {
    after_user: [sessionInit],
    settings: { refuse: { set_permission_mode: 'Invalid permission mode: plann' } },
  });
  await ccd.line('the turn', (line) => line.type === 'sdk' && line.message.type === 'system');
  ccd.send({ type: 'set_model', uuid: 'c-effort', effort: 'high' });
  await ccd.line('the effort answered', (line) => line.type === 'outcome' && line.uuid === 'c-effort');
  ccd.send({ type: 'set_mode', uuid: 'c-mode', mode: 'plann' });
  const refused = await ccd.line('the mode answered', (line) => line.type === 'outcome' && line.uuid === 'c-mode');
  expect(refused).toMatchObject({ status: 'refused', detail: 'Invalid permission mode: plann' });
  expect(settingsSeen(stub()).filter((seen) => 'subtype' in seen && seen.subtype === 'set_model')).toStrictEqual([]);
}, 10_000);

test('two settings lines reach claude one at a time: the second waits for the first to be answered', async () => {
  const { ccd, stub } = await run(trustedStart, { after_user: [sessionInit], settings: { delay_ms: 500 } });
  await ccd.line('the turn', (line) => line.type === 'sdk' && line.message.type === 'system');
  ccd.send({ type: 'set_model', uuid: 'c-model', model: 'sonnet' });
  ccd.send({ type: 'set_model', uuid: 'c-effort', effort: 'low' });
  await ccd.line('the effort answered', (line) => line.type === 'outcome' && line.uuid === 'c-effort');
  expect(settingsSeen(stub())).toStrictEqual([
    { subtype: 'set_model', model: 'sonnet' },
    { answered: 'set_model' },
    { subtype: 'apply_flag_settings', settings: { effortLevel: 'low' } },
    { answered: 'apply_flag_settings' },
  ]);
}, 10_000);

test("a Stop with a permission open: claude's cancel of it withdraws the request before the Stop's outcome", async () => {
  const { ccd } = await run(trustedStart, {
    after_user: [sessionInit, ask('toolu_s')],
    interrupt: { lines: [JSON.stringify({ type: 'control_cancel_request', request_id: 'req-toolu_s' }), turnResult()] },
  });
  await ccd.line('the permission', (line) => line.type === 'permission');
  ccd.send({ type: 'interrupt', uuid: 'c-stop', turn_id: runningTurn });
  const withdrawn = await ccd.line('the request withdrawn', (line) => line.type === 'withdrawn');
  const outcome = await ccd.line('the stop answered', (line) => line.type === 'outcome' && line.uuid === 'c-stop');
  expect(withdrawn.tool_use_id).toBe('toolu_s');
  expect(outcome.seq).toBeGreaterThan(withdrawn.seq);
}, 10_000);

test('stop ends the run: claude is asked to interrupt and ends, its end is the last line, and the host exits once ccd has it; a second stop does nothing', async () => {
  const { host, ccd, stub } = await run(trustedStart);
  ccd.send({ type: 'stop', uuid: 'c-stop' });
  ccd.send({ type: 'stop', uuid: 'c-stop-again' });
  await ccd.line("claude's end", (line) => line.type === 'exited');
  expect(stdinOf(stub()).some((message) => message.request?.subtype === 'interrupt')).toBe(true);
  await Bun.sleep(500);
  expect(ccd.lines.filter((line) => line.type === 'exited').length).toBe(1);
  expect(ccd.lines.at(-1).type).toBe('exited');
  ccd.ackAll();
  expect(await exitWithin(host, 3000)).toBe(0);
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
