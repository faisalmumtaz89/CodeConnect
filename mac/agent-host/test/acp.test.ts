// An Antigravity run on the built executable, with the stand-in agy (stub-agy.ts) as its agy, seen from both sides:
// what reaches agy (its arguments, its folder, every line) and what reaches ccd (the outcome, agy's messages as `acp`,
// its permission requests, its end).
import { afterAll, expect, test } from 'bun:test';
import { execFileSync } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { type Ccd, cleanup, goldenLines, open, runDir, startHost, stubLog, waitFor, writeAgy } from './rig.ts';

afterAll(cleanup);

const antigravityStart = goldenLines('to_host', 'start').find((m) => m.options.agent === 'antigravity')!;

/** A host with an Antigravity start sent, its agy the stand-in. Resolves once the start's outcome has come. */
async function run(
  script: { version?: number; resume?: boolean; quiet_on_cancel?: boolean; leave_child?: boolean; tool?: string } = {},
  prompt = 'read the note',
  resume?: string,
  more: object = {},
) {
  const dir = runDir();
  const work = join(dir, 'work');
  mkdirSync(work);
  const agy = writeAgy(dir, script);
  startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  const options = { ...antigravityStart.options, cwd: work, claude_path: agy, ...(resume ? { resume } : {}), ...more };
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
  expect(outcome.initialize?.models.length).toBe(5);
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
  // The commands agy sent while the session opened, and the opening's reply, came after the outcome, never before it.
  const types = ccd.lines.map((line) => line.type);
  expect(types.indexOf('outcome')).toBeLessThan(types.indexOf('acp'));
  expect(updates(ccd)).toStrictEqual([
    'available_commands_update', 'reply', 'agent_message_chunk', 'agent_message_chunk', 'tool_call',
  ]);
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

test('CodeConnect\'s own reading tools, msg_read and msg_sessions, run without a card, as on a Claude run', async () => {
  for (const tool of ['mcp_codeconnect_msg_read', 'mcp_codeconnect_msg_sessions']) {
    const { dir, ccd } = await run({ tool });
    await ccd.line('the prompt reply', (line) => line.type === 'acp' && line.message.result?.stopReason === 'end_turn');
    expect(ccd.lines.filter((line) => line.type === 'permission')).toStrictEqual([]);
    expect(stdin(dir).find((message) => message.id === 101 && message.method === undefined)?.result).toStrictEqual({
      outcome: { outcome: 'selected', optionId: 'allow-once' },
    });
  }
}, 30_000);

test('sending a message, and a tool of any other server, keep their cards', async () => {
  for (const tool of ['mcp_codeconnect_msg_send', 'mcp_other_msg_read']) {
    const { ccd } = await run({ tool });
    const permission = await ccd.line('the permission request', (line) => line.type === 'permission');
    expect(permission.request.tool_name).toBe(`Run ${tool}`);
  }
}, 30_000);

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

test('the run\'s uploads folder reaches agy as a folder it may read, at a start and at a relaunch', async () => {
  const uploads = { additional_directories: ['/tmp/uploads/the-run'] };
  const started = await run({}, 'read the note', undefined, uploads);
  await started.ccd.line('the permission request', (line) => line.type === 'permission');
  expect(stdin(started.dir).find((message) => message.method === 'session/new').params.additionalDirectories)
    .toStrictEqual(['/tmp/uploads/the-run']);
  const resumed = await run({}, 'again', 'sess-9', uploads);
  await resumed.ccd.line('the permission request', (line) => line.type === 'permission');
  expect(stdin(resumed.dir).find((message) => message.method === 'session/resume').params.additionalDirectories)
    .toStrictEqual(['/tmp/uploads/the-run']);
}, 30_000);

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

test('a start that names a model sets it on the session before its prompt; a model agy refuses refuses the start', async () => {
  const dir = runDir();
  const work = join(dir, 'work');
  mkdirSync(work);
  startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  ccd.send({
    ...antigravityStart, prompt: 'read the note',
    options: { ...antigravityStart.options, cwd: work, claude_path: writeAgy(dir), model: 'gemini-3.8-flash' },
  });
  expect(await ccd.line('the start outcome', (line) => line.type === 'outcome')).toMatchObject({ status: 'done' });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  const methods = stdin(dir).map((message) => message.method);
  expect(methods.slice(0, 4)).toStrictEqual(['initialize', 'session/new', 'session/set_config_option', 'session/prompt']);
  expect(stdin(dir)[2].params).toStrictEqual({ sessionId: 'sess-1', configId: 'model', value: 'gemini-3.8-flash' });

  const refused = runDir();
  const there = join(refused, 'work');
  mkdirSync(there);
  startHost(refused);
  const other = await open(refused);
  await other.attach(0, 1);
  other.send({
    ...antigravityStart,
    options: { ...antigravityStart.options, cwd: there, claude_path: writeAgy(refused), model: 'no-such-model' },
  });
  expect(await other.line('the start outcome', (line) => line.type === 'outcome')).toMatchObject({
    status: 'refused', detail: 'agy has no model no-such-model',
  });
}, 15_000);

test('the start\'s outcome carries agy\'s model families, each with its efforts from the lowest up, and its modes; the opening reply follows it', async () => {
  const { ccd, outcome } = await run();
  expect(outcome).toMatchObject({ status: 'done' });
  expect(outcome.initialize).toStrictEqual({
    commands: [],
    account: {},
    models: [
      { value: 'gemini-4-neon', displayName: 'Gemini 4 Neon', supportedEffortLevels: ['low', 'medium', 'high', 'xhigh'] },
      { value: 'gemini-3.8-flash', displayName: 'Gemini 3.8 Flash', supportedEffortLevels: ['low', 'medium', 'high'] },
      { value: 'gpt-oss-120b-medium', displayName: 'GPT-OSS 120B (Medium)', supportedEffortLevels: [] },
      { value: 'gemini-pro-orchestrator', displayName: 'Orchestrator (Carbon)', supportedEffortLevels: [] },
      { value: 'gemini-pro-orchestrator-neon', displayName: 'Orchestrator (Neon)', supportedEffortLevels: [] },
    ],
    modes: [
      { value: 'default', label: 'Default' },
      { value: 'accept-edits', label: 'Accept edits' },
      { value: 'auto', label: 'Auto' },
      { value: 'plan', label: 'Plan' },
    ],
  });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  const opening = acpLines(ccd).find((message) => message.result?.configOptions);
  expect(opening?.result.sessionId).toBe('sess-1');
}, 15_000);

test('a model, an effort and a mode reach agy as its own settings; one agy refuses is refused with its words', async () => {
  const { dir, ccd } = await run();
  await ccd.line('the permission request', (line) => line.type === 'permission');
  const outcome = (uuid: string) => ccd.line(`${uuid}'s outcome`, (line) => line.type === 'outcome' && line.uuid === uuid);
  ccd.send({ type: 'set_model', uuid: 'c-model', model: 'gemini-3.8-flash' });
  expect(await outcome('c-model')).toMatchObject({ status: 'done' });
  ccd.send({ type: 'set_model', uuid: 'c-effort', effort: 'low' });
  expect(await outcome('c-effort')).toMatchObject({ status: 'done' });
  ccd.send({ type: 'set_mode', uuid: 'c-mode', mode: 'plan' });
  expect(await outcome('c-mode')).toMatchObject({ status: 'done' });
  ccd.send({ type: 'set_model', uuid: 'c-none', model: 'no-such-model' });
  expect(await outcome('c-none')).toMatchObject({ status: 'refused', detail: 'agy has no model no-such-model' });
  const asked = stdin(dir).filter((message) => /set_config_option|set_mode/.test(message.method ?? ''));
  expect(asked.map((message) => [message.method, message.params])).toStrictEqual([
    ['session/set_config_option', { sessionId: 'sess-1', configId: 'model', value: 'gemini-3.8-flash' }],
    ['session/set_config_option', { sessionId: 'sess-1', configId: 'thought_level', value: 'low' }],
    ['session/set_mode', { sessionId: 'sess-1', modeId: 'plan' }],
    ['session/set_config_option', { sessionId: 'sess-1', configId: 'model', value: 'no-such-model' }],
  ]);
  expect(updates(ccd).filter((kind) => kind === 'config_option_update').length).toBe(3);
  expect(updates(ccd)).toContain('current_mode_update');
}, 15_000);

test('a relaunch gives the resumed session the effort and the mode the run ended with, before its prompt', async () => {
  const dir = runDir();
  const work = join(dir, 'work');
  mkdirSync(work);
  startHost(dir);
  const ccd = await open(dir);
  await ccd.attach(0, 1);
  ccd.send({
    ...antigravityStart, prompt: 'go on',
    options: { ...antigravityStart.options, cwd: work, claude_path: writeAgy(dir), resume: 'sess-9', effort: 'medium', permission_mode: 'plan' },
  });
  expect(await ccd.line('the start outcome', (line) => line.type === 'outcome')).toMatchObject({ status: 'done' });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  expect(stdin(dir).slice(1, 5).map((message) => [message.method, message.params])).toStrictEqual([
    ['session/resume', expect.objectContaining({ sessionId: 'sess-9' })],
    ['session/set_config_option', { sessionId: 'sess-9', configId: 'thought_level', value: 'medium' }],
    ['session/set_mode', { sessionId: 'sess-9', modeId: 'plan' }],
    ['session/prompt', expect.objectContaining({ sessionId: 'sess-9' })],
  ]);
}, 15_000);

test('a stop whose prompt agy leaves unanswered ends the turn cancelled, not in an error', async () => {
  const { ccd } = await run({ quiet_on_cancel: true });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  ccd.send({ type: 'stop', uuid: 'c-stop' });
  await ccd.line('the run\'s end', (line) => line.type === 'exited');
  // A prompt's reply, as agy wrote it or as the host closes it when agy has gone: no method.
  const replies = acpLines(ccd).filter((message) => message.method === undefined);
  expect(replies.at(-1)).toMatchObject({ result: { stopReason: 'cancelled' } });
  expect(replies.some((message) => message.error)).toBe(false);
}, 15_000);

test('agy ending on its own, nobody having stopped it, ends the running prompt in an error', async () => {
  const { dir, ccd } = await run();
  await ccd.line('the permission request', (line) => line.type === 'permission');
  const agy = stubLog(dir).find((event) => event.argv);
  expect(agy).toBeDefined();
  const pid = Number(execFileSync('/usr/bin/pgrep', ['-f', `stub-agy.ts ${join(dir, 'agy.json')}`]).toString().trim().split('\n')[0]);
  process.kill(pid, 'SIGTERM');
  await ccd.line('the run\'s end', (line) => line.type === 'exited');
  // A prompt's reply, as agy wrote it or as the host closes it when agy has gone: no method.
  const replies = acpLines(ccd).filter((message) => message.method === undefined);
  expect(replies.at(-1)).toMatchObject({ error: { message: 'agy ended' } });
}, 15_000);

test('what agy leaves running in the run\'s process group is ended before its end is reported', async () => {
  const { dir, ccd } = await run({ leave_child: true });
  await ccd.line('the permission request', (line) => line.type === 'permission');
  const child = stubLog(dir).find((event) => event.child)?.child;
  expect(typeof child).toBe('number');
  const alive = (pid: number) => {
    try {
      process.kill(pid, 0);
      return true;
    } catch {
      return false;
    }
  };
  expect(alive(child)).toBe(true);
  ccd.send({ type: 'stop', uuid: 'c-stop' });
  await ccd.line('the run\'s end', (line) => line.type === 'exited');
  expect(alive(child)).toBe(false);
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
