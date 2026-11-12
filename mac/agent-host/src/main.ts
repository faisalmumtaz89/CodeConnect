// A scratch Agent SDK host for P0 proof 1: drives one Claude session the way a host would, and prints every SDK
// message as one JSON line (type, subtype, uuid, session_id, and a short view of the content).
//
//   cc-agent-host walk   <claude> <work>          deny, allow, question, model, mode, interrupt
//   cc-agent-host resume <claude> <work> <id>     one turn in a new process with resume
//   cc-agent-host once   <claude|-> <work> [<id>] one turn; "-" passes no pathToClaudeCodeExecutable
// The environment (scratch HOME, CLAUDE_CONFIG_DIR, the stub's URL and key) is the caller's.
import { query } from '@anthropic-ai/claude-agent-sdk';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';

const [, , mode, claude, work, extra] = process.argv;

const stamp = () => new Date().toTimeString().slice(0, 8);
const say = (what) => console.log(`${stamp()} ${what}`);

function view(m) {
  const base = { type: m.type, subtype: m.subtype, uuid: m.uuid, session_id: m.session_id };
  if (m.type === 'system' && m.subtype === 'init') {
    return { ...base, model: m.model, permissionMode: m.permissionMode, apiKeySource: m.apiKeySource, cwd: m.cwd,
             claude_code_version: m.claude_code_version, tools: m.tools?.length };
  }
  if (m.type === 'assistant' || m.type === 'user') {
    const c = m.message?.content;
    const parts = Array.isArray(c)
      ? c.map((b) => b.type + (b.name ? `:${b.name}` : '') + (b.text ? `:${b.text.slice(0, 60)}` : '')
          + (b.type === 'tool_result' ? `:${JSON.stringify(b.content).slice(0, 140)}` : ''))
      : [String(c).slice(0, 100)];
    return { ...base, parts, isReplay: m.isReplay };
  }
  if (m.type === 'result') {
    return { ...base, is_error: m.is_error, result: m.result, num_turns: m.num_turns, stop_reason: m.stop_reason };
  }
  if (m.type === 'stream_event') return null;
  return { ...base, status: m.status, permissionMode: m.permissionMode };
}

// Streaming input: a queue the host pushes user messages into; ending it closes Claude's input.
function inputQueue() {
  const waiting = [];
  let wake = null;
  let ended = false;
  return {
    push(content) {
      say(`-> user ${JSON.stringify(content).slice(0, 100)}`);
      waiting.push({ type: 'user', message: { role: 'user', content }, parent_tool_use_id: null, session_id: '' });
      wake?.();
    },
    end() { ended = true; wake?.(); },
    async *[Symbol.asyncIterator]() {
      for (;;) {
        while (waiting.length) yield waiting.shift();
        if (ended) return;
        await new Promise((resolve) => { wake = resolve; });
        wake = null;
      }
    },
  };
}

// What the host answers, set per turn by the walk.
let policy = 'allow';
const canUseToolSeen = [];
async function canUseTool(toolName, input, options) {
  canUseToolSeen.push(toolName);
  say(`<- canUseTool ${toolName} ${JSON.stringify(input).slice(0, 160)} options=${JSON.stringify(
    { toolUseID: options?.toolUseID, suggestions: options?.suggestions?.length, keys: Object.keys(options ?? {}) })}`);
  if (toolName === 'AskUserQuestion') {
    const question = input.questions[0].question;
    const answer = { behavior: 'allow',
      updatedInput: { ...input, answers: { [question]: 'Blue' }, annotations: { [question]: { notes: 'rig note' } } } };
    say(`-> ${JSON.stringify(answer).slice(0, 220)}`);
    return answer;
  }
  const answer = policy === 'deny'
    ? { behavior: 'deny', message: 'Denied by the rig host.' }
    : { behavior: 'allow', updatedInput: input };
  say(`-> ${JSON.stringify(answer).slice(0, 160)}`);
  return answer;
}

function start(prompt, more = {}) {
  const options = {
    cwd: work,
    includePartialMessages: true,
    canUseTool,
    stderr: (line) => say(`stderr: ${line.trim().slice(0, 200)}`),
    ...more,
  };
  if (claude !== '-') options.pathToClaudeCodeExecutable = claude;
  say(`query options: ${JSON.stringify({ ...options, canUseTool: 'fn', stderr: 'fn' })}`);
  return query({ prompt, options });
}

// Every non-partial message's uuid, in order, with its type: the walk prints them so a resumed session's ids can be
// compared with the original's.
const seen = [];

// Reads messages until a result arrives (or the stream ends); returns the session id seen.
async function untilResult(iterator) {
  let sid = null;
  for (;;) {
    const next = await iterator.next();
    if (next.done) { say('<- (stream ended)'); return { sid, done: true }; }
    const m = next.value;
    sid = m.session_id ?? sid;
    if (m.type !== 'stream_event') seen.push(`${m.type}${m.subtype ? '/' + m.subtype : ''}:${m.uuid}`);
    const v = view(m);
    if (v) say(`<- ${JSON.stringify(v)}`);
    if (m.type === 'result') return { sid, done: false };
  }
}

async function walk() {
  const input = inputQueue();
  const q = start(input);
  const it = q[Symbol.asyncIterator]();
  const marker = join(work, 'approved.txt');
  rmSync(marker, { force: true });

  say('== (a) must-fail: the Bash approval DENIED');
  policy = 'deny';
  input.push('TOOLTURN write the marker');
  let { sid } = await untilResult(it);
  say(`   marker exists after deny: ${existsSync(marker)}`);

  say('== approval ALLOWED');
  policy = 'allow';
  input.push('TOOLTURN write the marker');
  await untilResult(it);
  say(`   marker exists after allow: ${existsSync(marker)}`);

  say('== AskUserQuestion answered Blue, with annotations');
  input.push('ASKTURN pick one');
  await untilResult(it);

  say('== setModel, then a turn');
  await q.setModel('claude-sonnet-5-5');
  input.push('MODELTURN after the switch');
  await untilResult(it);

  say('== setPermissionMode plan, then a turn, then back to default');
  await q.setPermissionMode('plan');
  input.push('PLANTURN in plan mode');
  await untilResult(it);
  await q.setPermissionMode('default');

  say('== interrupt a held turn');
  input.push('SLOWTURN keep working');
  await new Promise((r) => setTimeout(r, 4000));
  say('-> interrupt()');
  await q.interrupt();
  await untilResult(it);

  console.log(`SESSION_ID ${sid}`);
  console.log(`UUIDS ${JSON.stringify(seen)}`);
  input.end();
  const tail = await untilResult(it);
  say(`   after input end: ${JSON.stringify(tail)}`);
}

// The user's own messages echoed back (`--replay-user-messages`), and a turn after an interrupted one.
async function walk2() {
  const input = inputQueue();
  const q = start(input, { extraArgs: { 'replay-user-messages': null } });
  const it = q[Symbol.asyncIterator]();
  input.push('hello with replay');
  const { sid } = await untilResult(it);
  input.push('SLOWTURN keep working');
  await new Promise((r) => setTimeout(r, 3000));
  say('-> interrupt()');
  await q.interrupt();
  await untilResult(it);
  say('== a turn after the interrupt');
  input.push('AFTERSTOP still there');
  await untilResult(it);
  console.log(`SESSION_ID ${sid}`);
  console.log(`UUIDS ${JSON.stringify(seen)}`);
  input.end();
  const tail = await untilResult(it);
  say(`   after input end: ${JSON.stringify(tail)}`);
}

// Two plain turns with the user's messages replayed, then input closed after a successful turn.
async function short() {
  const input = inputQueue();
  const q = start(input, { extraArgs: { 'replay-user-messages': null } });
  const it = q[Symbol.asyncIterator]();
  input.push('hello for the id check');
  const { sid } = await untilResult(it);
  input.push('ASKTURN pick one');
  await untilResult(it);
  console.log(`SESSION_ID ${sid}`);
  console.log(`UUIDS ${JSON.stringify(seen)}`);
  input.end();
  await untilResult(it);
}

// Proof 2: one Bash tool turn with the given extra options (JSON); prints the init message's discovery fields and
// which tools reached canUseTool.
async function probe(json) {
  const asked = [];
  const input = inputQueue();
  const q = start(input, JSON.parse(json));
  const it = q[Symbol.asyncIterator]();
  input.push('TOOLTURN write the marker');
  for (;;) {
    const next = await it.next();
    if (next.done) break;
    const m = next.value;
    if (m.type === 'system' && m.subtype === 'init') {
      console.log(`INIT ${JSON.stringify({ mcp_servers: m.mcp_servers, slash_commands: m.slash_commands,
        agents: m.agents, skills: m.skills, permissionMode: m.permissionMode, mcp_tools: m.tools.filter((t) => t.startsWith('mcp__')) })}`);
    }
    if (m.type === 'result') { console.log(`RESULT ${m.subtype}`); break; }
  }
  console.log(`ASKED ${JSON.stringify(canUseToolSeen)}`);
  input.end();
  await it.next().catch(() => {});
}

// Proof 3: start the session, print the initialize answer's account, wait until the file <gate> exists (the rig checks
// the spawned claude's environment first), then send one prompt and print what comes back.
async function gated(gate, json) {
  const input = inputQueue();
  const q = start(input, JSON.parse(json || '{}'));
  const it = q[Symbol.asyncIterator]();
  try {
    const info = await q.initializationResult?.() ?? null;
    // Only how the session is signed in, never who: no email, organisation or token is printed.
    const a = info?.account ?? null;
    console.log(`ACCOUNT ${JSON.stringify(a && { keys: Object.keys(a).sort(), tokenSource: a.tokenSource,
      apiKeySource: a.apiKeySource, apiProvider: a.apiProvider, subscriptionType: a.subscriptionType })}`);
  } catch (err) {
    console.log(`ACCOUNT error ${err?.message ?? err}`);
  }
  console.log('READY');
  while (!existsSync(gate)) await new Promise((r) => setTimeout(r, 200));
  if (readFileSync(gate, 'utf8').trim() !== 'go') { console.log('GATE refused'); input.end(); return; }
  input.push('Reply with exactly the word OK and nothing else.');
  for (;;) {
    const next = await it.next().catch((err) => ({ done: true, error: err }));
    if (next.done) { console.log(`END ${next.error?.message ?? ''}`); break; }
    const m = next.value;
    if (m.type === 'system' && m.subtype === 'init') console.log(`INIT ${JSON.stringify({ apiKeySource: m.apiKeySource, model: m.model })}`);
    else if (m.type === 'assistant') console.log(`ASSISTANT ${JSON.stringify(m.message?.content?.map((b) => b.text ?? b.type))} error=${m.error ?? ''}`);
    else if (m.type === 'result') { console.log(`RESULT ${JSON.stringify({ subtype: m.subtype, is_error: m.is_error, result: m.result, usage_keys: Object.keys(m.usage ?? {}) })}`); break; }
    else if (m.type !== 'stream_event') console.log(`MSG ${m.type}/${m.subtype ?? ''}`);
  }
  input.end();
  await it.next().catch(() => {});
}

async function once(resume) {
  const input = inputQueue();
  const q = start(input, resume ? { resume } : {});
  const it = q[Symbol.asyncIterator]();
  input.push(resume ? 'RESUMETURN do you remember' : 'hello once');
  try {
    await untilResult(it);
  } catch (err) {
    say(`ERROR ${err?.message ?? err}`);
    process.exitCode = 3;
  }
  console.log(`UUIDS ${JSON.stringify(seen)}`);
  input.end();
}

try {
  if (mode === 'walk') await walk();
  else if (mode === 'walk2') await walk2();
  else if (mode === 'short') await short();
  else if (mode === 'probe') await probe(extra);
  else if (mode === 'gated') await gated(extra, process.argv[6]);
  else if (mode === 'resume') await once(extra);
  else if (mode === 'once') await once(extra);
} catch (err) {
  say(`ERROR ${err?.stack ?? err}`);
  process.exitCode = 3;
}
