// The stand-in agy a test gives an Antigravity run as its agy (rig.ts writeAgy), run on the built host as Bun. It
// speaks `agy acp`'s Agent Client Protocol over stdio as the CLI's own changelog and the ACP schema describe it: it
// answers `initialize` with its script's protocol version, advertising session/resume unless its script says
// `resume: false`, `session/new` with a session after a notification of its commands (which the host must hold until
// the start's outcome), and `session/resume` with an empty result. At each `session/prompt` it writes its reply in
// two chunks, a tool call, then asks permission for it with allow_once and reject_once options; once answered it ends
// the tool as the answer says, writes one more chunk and answers the prompt `end_turn`. A request answered
// `cancelled` ends the prompt `cancelled`. It records its argv, its folder and every line it read; its input's end
// ends it.
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
  } else if (message.method === 'session/resume') {
    write({ id: message.id, result: {} });
  } else if (message.method === 'session/new') {
    update('sess-1', { sessionUpdate: 'available_commands_update', availableCommands: [] });
    write({ id: message.id, result: { sessionId: 'sess-1' } });
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
        toolCall: { toolCallId: 'call_1', title: 'cat NOTE.txt', kind: 'read', rawInput: { command: 'cat NOTE.txt' } },
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
      write({ id: promptId, result: { stopReason: 'cancelled' } });
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
