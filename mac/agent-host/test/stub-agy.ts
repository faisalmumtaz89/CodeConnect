// The stand-in agy a test gives an Antigravity run as its agy (rig.ts writeAgy), run on the built host as Bun. It
// speaks `agy acp`'s Agent Client Protocol over stdio as the CLI's own changelog and the ACP schema describe it: it
// answers `initialize` with its script's protocol version, advertising session/resume unless its script says `resume:
// false`, `session/new` with a session after a notification of its commands (which the host must hold until the start's
// outcome) and `session/resume`, each with agy 1.5.15's settings: its models (effort baked into each id), modes and
// configOptions (model family, the family's thought levels, mode). `session/set_config_option` sets one of those and
// `session/set_mode` the mode, each followed by a `config_option_update` (and `current_mode_update`); a value agy does
// not offer is refused, the model `no-such-model` as "agy has no model no-such-model". `session/close` is answered with
// nothing. At each `session/prompt` it writes its reply in two chunks, a tool call, then asks permission for it with
// allow_once and reject_once options; once answered it ends the tool as the answer says, writes one more chunk and
// answers the prompt `end_turn`. A request answered `cancelled` ends the prompt `cancelled`, unless its script says
// `quiet_on_cancel`. With `tool` its request names that tool as agy names an MCP server's (`name`, and `title` "Run
// <tool>"). With `leave_child` it starts a child that outlives it. It records its argv, its folder and every
// line it read; its input's end ends it.
//
//   stub-agy.ts <script.json> acp
import { appendFileSync, readFileSync } from 'node:fs';
import { createInterface } from 'node:readline';

const script = JSON.parse(readFileSync(process.argv[2]!, 'utf8'));
const record = (event: object) => appendFileSync(script.log, `${JSON.stringify(event)}\n`);
const write = (message: object) => process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
const update = (sessionId: string, update: object) => write({ method: 'session/update', params: { sessionId, update } });
const chunk = (sessionId: string, text: string) =>
  update(sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text } });

record({ argv: process.argv.slice(3), cwd: process.cwd() });
// `leave_child`: as a command agy ran in the background would, a child that outlives agy, in its process group.
if (script.leave_child) {
  const { spawn } = await import('node:child_process');
  const child = spawn('/bin/sleep', ['60'], { stdio: 'ignore' });
  child.unref();
  record({ child: child.pid });
}
// The session's settings, and what agy 1.5.15 offers (cut down): families whose levels are their ids' last part, a
// family that is one id (gpt-oss-120b-medium) and one whose id begins with another's (gemini-pro-orchestrator-neon).
const families: Record<string, string[]> = {
  'gemini-4-neon': ['high', 'xhigh', 'medium', 'low'],
  'gemini-3.8-flash': ['high', 'medium', 'low'],
  'gpt-oss-120b-medium': [],
  'gemini-pro-orchestrator': [],
  'gemini-pro-orchestrator-neon': [],
};
// Each family's name, as agy 1.5.15 names it.
const familyNames: Record<string, string> = {
  'gemini-4-neon': 'Gemini 4 Neon',
  'gemini-3.8-flash': 'Gemini 3.8 Flash',
  'gpt-oss-120b-medium': 'GPT-OSS 120B (Medium)',
  'gemini-pro-orchestrator': 'Orchestrator (Carbon)',
  'gemini-pro-orchestrator-neon': 'Orchestrator (Neon)',
};
const modes = ['default', 'accept-edits', 'auto', 'plan'];
const names: Record<string, string> = { default: 'Default', 'accept-edits': 'Accept edits', auto: 'Auto', plan: 'Plan' };
const settings = { model: 'gemini-4-neon', effort: 'high', mode: 'default' };
const configOptions = () => [
  { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: settings.model,
    options: Object.keys(families).map((value) => ({ value, name: familyNames[value] })) },
  { id: 'thought_level', name: 'Thinking Level', category: 'thought_level', type: 'select', currentValue: settings.effort,
    options: families[settings.model]!.map((value) => ({ value, name: value })) },
  { id: 'mode', name: 'Mode', category: 'mode', type: 'select', currentValue: settings.mode,
    options: modes.map((value) => ({ value, name: names[value] })) },
];
const opening = (sessionId: string) => ({
  sessionId,
  models: {
    availableModels: Object.entries(families).flatMap(([family, levels]) =>
      levels.length === 0 ? [{ modelId: family }] : levels.map((level) => ({ modelId: `${family}-${level}` }))),
    currentModelId: `${settings.model}-${settings.effort}`,
  },
  modes: { currentModeId: settings.mode, availableModes: modes.map((id) => ({ id, name: names[id] })) },
  configOptions: configOptions(),
});
// The prompt waiting on a permission request, by the request's id.
const waiting = new Map<number, { sessionId: string; promptId: number }>();
let asked = 100;
for await (const text of createInterface({ input: process.stdin })) {
  const message = JSON.parse(text);
  record({ stdin: message });
  if (message.method === 'initialize') {
    const sessionCapabilities = script.resume === false ? {} : { resume: {} };
    write({
      id: message.id,
      result: { protocolVersion: script.version ?? 1, agentCapabilities: { loadSession: true, sessionCapabilities } },
    });
  } else if (message.method === 'session/set_config_option') {
    const { sessionId, configId, value } = message.params;
    const key = ({ model: 'model', thought_level: 'effort', mode: 'mode' } as const)[configId as string];
    const offered = key === 'model' ? Object.keys(families) : key === 'effort' ? families[settings.model] : modes;
    if (key === undefined || !offered!.includes(value)) {
      write({ id: message.id, error: { code: -32602, message: `agy has no ${configId} ${value}` } });
    } else {
      settings[key] = value;
      if (key === 'model' && !families[value]!.includes(settings.effort)) settings.effort = families[value]![0] ?? '';
      update(sessionId, { sessionUpdate: 'config_option_update', configOptions: configOptions() });
      write({ id: message.id, result: { configOptions: configOptions() } });
    }
  } else if (message.method === 'session/set_mode') {
    const { sessionId, modeId } = message.params;
    if (!modes.includes(modeId)) {
      write({ id: message.id, error: { code: -32602, message: `agy has no mode ${modeId}` } });
    } else {
      settings.mode = modeId;
      update(sessionId, { sessionUpdate: 'current_mode_update', currentModeId: modeId });
      update(sessionId, { sessionUpdate: 'config_option_update', configOptions: configOptions() });
      write({ id: message.id, result: {} });
    }
  } else if (message.method === 'session/close') {
    write({ id: message.id, result: {} });
  } else if (message.method === 'session/resume') {
    write({ id: message.id, result: opening(message.params.sessionId) });
  } else if (message.method === 'session/new') {
    update('sess-1', { sessionUpdate: 'available_commands_update', availableCommands: [] });
    write({ id: message.id, result: opening('sess-1') });
  } else if (message.method === 'session/prompt') {
    const sessionId = message.params.sessionId;
    chunk(sessionId, 'Hel');
    chunk(sessionId, 'lo.');
    update(sessionId, {
      sessionUpdate: 'tool_call', toolCallId: 'call_1', title: 'cat NOTE.txt', kind: 'read', status: 'pending',
      rawInput: { command: 'cat NOTE.txt' },
    });
    asked += 1;
    waiting.set(asked, { sessionId, promptId: message.id });
    write({
      id: asked,
      method: 'session/request_permission',
      params: {
        sessionId,
        toolCall: script.tool
          ? { toolCallId: 'call_1', name: script.tool, title: `Run ${script.tool}`, kind: 'other', rawInput: { toolSummary: 'a summary' } }
          : { toolCallId: 'call_1', title: 'cat NOTE.txt', kind: 'read', rawInput: { command: 'cat NOTE.txt' } },
        options: [
          { optionId: 'allow-once', name: 'Allow', kind: 'allow_once' },
          { optionId: 'allow-always', name: 'Always allow', kind: 'allow_always' },
          { optionId: 'reject-once', name: 'Reject', kind: 'reject_once' },
        ],
      },
    });
  } else if (message.method === undefined && waiting.has(message.id)) {
    const { sessionId, promptId } = waiting.get(message.id)!;
    waiting.delete(message.id);
    const outcome = message.result?.outcome;
    if (outcome?.outcome === 'cancelled') {
      // `quiet_on_cancel`: as agy may, it leaves the cancelled prompt unanswered and ends when its input does.
      if (!script.quiet_on_cancel) write({ id: promptId, result: { stopReason: 'cancelled' } });
      continue;
    }
    const allowed = outcome?.optionId === 'allow-once';
    update(sessionId, {
      sessionUpdate: 'tool_call_update', toolCallId: 'call_1', status: allowed ? 'completed' : 'failed',
      content: [{ type: 'content', content: { type: 'text', text: allowed ? 'the word' : 'denied' } }],
    });
    chunk(sessionId, ' Done.');
    write({ id: promptId, result: { stopReason: 'end_turn' } });
  }
}
