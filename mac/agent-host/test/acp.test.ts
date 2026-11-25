// An Antigravity run on the built executable, with the stand-in agy (stub-agy.ts) as its agy, seen from both sides:
// what reaches agy (its arguments, its folder, every line) and what reaches ccd (the outcome, agy's messages as `acp`,
// its permission requests, its end).
import { afterAll, expect, test } from 'bun:test';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { type Ccd, cleanup, goldenLines, open, runDir, startHost, stubLog, waitFor, writeAgy } from './rig.ts';

afterAll(cleanup);

const antigravityStart = goldenLines('to_host', 'start').find((m) => m.options.agent === 'antigravity')!;

/** A host with an Antigravity start sent, its agy the stand-in. Resolves once the start's outcome has come. */
async function run(script: { version?: number; resume?: boolean } = {}, prompt = 'read the note', resume?: string) {
  const dir = runDir();
  const work = join(dir, 'work');
  mkdirSync(work);
  const agy = writeAgy(dir, script);
  startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  const options = { ...antigravityStart.options, cwd: work, claude_path: agy, ...(resume ? { resume } : {}) };
  ccd.send({ ...antigravityStart, prompt, options });
  const outcome = await ccd.line('the start outcome', (line) => line.type === 'outcome');
  return { dir, work, ccd, outcome };
}

const acpLines = (ccd: Ccd) => ccd.lines.filter((line) => line.type === 'acp').map((line) => line.message);
const updates = (ccd: Ccd) => acpLines(ccd).map((message) => message.params?.update?.sessionUpdate ?? 'reply');
const stdin = (dir: string) => stubLog(dir).filter((event) => event.stdin).map((event) => event.stdin);

test('a start initializes agy at version 1, opens a session in the folder with the mail server, then prompts', async () => {
  const { dir, work, ccd, outcome } = await run();
  expect(outcome).toMatchObject({ type: 'outcome', uuid: antigravityStart.uuid, status: 'done' });
  expect(outcome.initialize).toBeUndefined();
  const permission = await ccd.line('the permission request', (line) => line.type === 'permission');
  const [first] = stubLog(dir);
  expect(first).toStrictEqual({ argv: ['acp'], cwd: work });
  const [initialize, session, prompt] = stdin(dir);
  expect(initialize.method).toBe('initialize');
  expect(initialize.params).toStrictEqual({
    protocolVersion: 1,
    clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
  });
  expect(session.params).toStrictEqual({
    cwd: work,
    mcpServers: [{ name: 'codeconnect', ...antigravityStart.options.mail, env: [] }],
  });
  expect(prompt.params).toStrictEqual({ sessionId: 'sess-1', prompt: [{ type: 'text', text: 'read the note' }] });
  // The commands agy sent while the session opened came after the outcome, never before it.
  const types = ccd.lines.map((line) => line.type);
  expect(types.indexOf('outcome')).toBeLessThan(types.indexOf('acp'));
  expect(updates(ccd)).toStrictEqual(['available_commands_update', 'agent_message_chunk', 'agent_message_chunk', 'tool_call']);
  expect(permission.request).toMatchObject({
    tool_use_id: `${antigravityStart.uuid}/101`,
    tool_name: 'cat NOTE.txt',
    input: { command: 'cat NOTE.txt' },
  });
}, 15_000);

test('Allow answers with the request\'s own allow_once option; the turn goes on to its end', async () => {
  const { dir, ccd } = await run();
  const permission = await ccd.line('the permission request', (line) => line.type === 'permission');
  ccd.send({ type: 'answer', uuid: 'c-allow', tool_use_id: permission.request.tool_use_id, decision: { behavior: 'allow' } });
  await ccd.line('the prompt reply', (line) => line.type === 'acp' && line.message.result?.stopReason === 'end_turn');
  expect(ccd.lines.find((line) => line.uuid === 'c-allow')).toMatchObject({ type: 'outcome', status: 'done' });
  expect(stdin(dir).find((message) => message.id === 101 && message.method === undefined)).toStrictEqual({
    jsonrpc: '2.0',
    id: 101,
    result: { outcome: { outcome: 'selected', optionId: 'allow-once' } },
  });
  expect(updates(ccd).slice(-3)).toStrictEqual(['tool_call_update', 'agent_message_chunk', 'reply']);
  // A second answer to the same request finds it closed.
  ccd.send({ type: 'answer', uuid: 'c-again', tool_use_id: `${antigravityStart.uuid}/101`, decision: { behavior: 'allow' } });
  expect(await ccd.line('the refusal', (line) => line.uuid === 'c-again')).toMatchObject({ status: 'refused' });
}, 15_000);

test('Deny answers with reject_once; a say is done at once and is the next prompt, after the turn before it', async () => {
  const { dir, ccd } = await run();
  const permission = await ccd.line('the permission request', (line) => line.type === 'permission');
  ccd.send({ type: 'say', uuid: 'c-say', text: 'and the docs' });
  expect(await ccd.line('the say\'s outcome', (line) => line.uuid === 'c-say')).toMatchObject({ status: 'done' });
  ccd.send({
    type: 'answer', uuid: 'c-deny', tool_use_id: permission.request.tool_use_id,
    decision: { behavior: 'deny', message: 'Not now' },
  });
  await ccd.line('the second permission request', (line) => line.type === 'permission' && line.request.tool_use_id === `${antigravityStart.uuid}/102`);
  const lines = stdin(dir);
  expect(lines.find((message) => message.id === 101 && message.method === undefined).result).toStrictEqual({
    outcome: { outcome: 'selected', optionId: 'reject-once' },
  });
  // The say was sent to agy only after the first prompt was answered, and its lines carry its own turn.
  const prompts = lines.filter((message) => message.method === 'session/prompt');
  expect(prompts.map((message) => message.params.prompt[0].text)).toStrictEqual(['read the note', 'and the docs']);
  expect(lines.indexOf(prompts[1])).toBeGreaterThan(lines.findIndex((message) => message.id === 101 && !message.method));
  const turns = ccd.lines.filter((line) => line.type === 'acp' && line.turn !== undefined).map((line) => line.turn);
  expect([...new Set(turns)]).toStrictEqual([antigravityStart.uuid, 'c-say']);
}, 15_000);

test('an interrupt of the running turn cancels it, drops the prompts queued after it, and is done at its reply', async () => {
  const { dir, ccd } = await run();
  await ccd.line('the permission request', (line) => line.type === 'permission');
  ccd.send({ type: 'interrupt', uuid: 'c-other', turn_id: 'not-the-turn' });
  expect(await ccd.line('the refusal', (line) => line.uuid === 'c-other')).toMatchObject({ status: 'refused' });
  ccd.send({ type: 'say', uuid: 'c-queued', text: 'and the docs' });
  ccd.send({ type: 'interrupt', uuid: 'c-stop-turn', turn_id: antigravityStart.uuid });
  const outcome = await ccd.line('the interrupt\'s outcome', (line) => line.uuid === 'c-stop-turn');
  expect(outcome).toMatchObject({ status: 'done', terminal_reason: 'cancelled', cancelled: ['c-queued'] });
  const reply = ccd.lines.findIndex((line) => line.type === 'acp' && line.message.result?.stopReason === 'cancelled');
  expect(reply).toBeGreaterThan(-1);
  expect(reply).toBeLessThan(ccd.lines.indexOf(outcome));
  expect(ccd.lines[reply].turn).toBe(antigravityStart.uuid);
  const lines = stdin(dir);
  expect(lines.find((message) => message.id === 101 && !message.method).result).toStrictEqual({
    outcome: { outcome: 'cancelled' },
  });
  expect(lines.find((message) => message.method === 'session/cancel').params).toStrictEqual({ sessionId: 'sess-1' });
  // Its card is withdrawn, so ccd closes it.
  expect(ccd.lines.find((line) => line.type === 'withdrawn')).toMatchObject({ tool_use_id: `${antigravityStart.uuid}/101` });
  // The queued say never reached agy.
  await Bun.sleep(300);
  expect(stdin(dir).filter((message) => message.method === 'session/prompt')).toHaveLength(1);
}, 15_000);

test('stop answers an open request cancelled, cancels the session and ends agy; its end is exited', async () => {
  const { dir, ccd } = await run();
  await ccd.line('the permission request', (line) => line.type === 'permission');
  ccd.send({ type: 'stop', uuid: 'c-stop' });
  await ccd.line('the run\'s end', (line) => line.type === 'exited');
  expect(ccd.lines.find((line) => line.type === 'withdrawn')).toMatchObject({ tool_use_id: `${antigravityStart.uuid}/101` });
  const lines = stdin(dir);
  expect(lines.find((message) => message.id === 101 && message.method === undefined).result).toStrictEqual({
    outcome: { outcome: 'cancelled' },
  });
  expect(lines.find((message) => message.method === 'session/cancel').params).toStrictEqual({ sessionId: 'sess-1' });
  expect(ccd.lines.find((line) => line.type === 'exited')).toMatchObject({ code: 0 });
}, 15_000);

test('a relaunch resumes the run\'s session with the mail server again, then prompts it', async () => {
  const { dir, work, ccd, outcome } = await run({}, 'carry on', 'sess-9');
  expect(outcome).toMatchObject({ status: 'done' });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  const lines = stdin(dir);
  expect(lines.some((message) => message.method === 'session/new')).toBe(false);
  expect(lines.find((message) => message.method === 'session/resume').params).toStrictEqual({
    sessionId: 'sess-9',
    cwd: work,
    mcpServers: [{ name: 'codeconnect', ...antigravityStart.options.mail, env: [] }],
  });
  expect(lines.find((message) => message.method === 'session/prompt').params).toStrictEqual({
    sessionId: 'sess-9',
    prompt: [{ type: 'text', text: 'carry on' }],
  });
}, 15_000);

test('an agy that does not advertise session/resume refuses a relaunch, and nothing is opened', async () => {
  const { dir, ccd, outcome } = await run({ resume: false }, 'carry on', 'sess-9');
  expect(outcome).toMatchObject({
    status: 'refused',
    detail: 'this agy does not offer session/resume, so the session cannot go on',
  });
  await waitFor('the run\'s end', () => ccd.lines.some((line) => line.type === 'exited'), 10_000);
  expect(stdin(dir).map((message) => message.method)).toStrictEqual(['initialize']);
}, 15_000);

test('an agy at another ACP version is refused at the start, with the version it speaks', async () => {
  const { ccd, outcome } = await run({ version: 2 });
  expect(outcome).toMatchObject({ status: 'refused', detail: 'agy speaks ACP version 2, not 1' });
  await waitFor('the run\'s end', () => ccd.lines.some((line) => line.type === 'exited'), 10_000);
  expect(ccd.lines.some((line) => line.type === 'acp')).toBe(false);
}, 15_000);

test('a wake is one more prompt: done at once, and its lines carry its turn', async () => {
  const { dir, ccd } = await run({}, '');
  const nudge = 'You have CodeConnect messages: call the codeconnect msg_read tool, then handle each message as its sender asks.';
  ccd.send({ type: 'wake', uuid: 'c-wake', text: nudge });
  expect(await ccd.line('the wake\'s outcome', (line) => line.uuid === 'c-wake')).toMatchObject({ status: 'done' });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  expect(stdin(dir).filter((message) => message.method === 'session/prompt').map((m) => m.params.prompt))
    .toStrictEqual([[{ type: 'text', text: nudge }]]);
  expect(ccd.lines.filter((line) => line.type === 'acp' && line.turn !== undefined).map((line) => line.turn))
    .toContain('c-wake');
}, 15_000);
