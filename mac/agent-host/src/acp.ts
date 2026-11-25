// An Antigravity run: `agy acp`, the Agent Client Protocol over its stdio (JSON-RPC 2.0, one message per line), in
// the host's process group as claude is. The start initializes it (protocol version 1; no file system or terminal
// offered, so agy uses its own) and opens one session in the run's folder with CodeConnect's mail server, or, for a
// relaunch (`resume`), resumes the run's session with that server again, only when agy advertises session/resume; a
// model the start names is set on the session (session/set_model). Its outcome is `done` once the session is open,
// `refused` with the reason otherwise. Every notification agy sends, and the reply to each prompt, goes to ccd whole as
// `acp`, with the turn it belongs to: the uuid of the command whose prompt is running (the start's for its own). A
// `say`, or ccd's `wake` for the run's mail, is one more prompt, queued and `done` at once, and sent once the one
// before has its reply, so prompts never overlap. An `interrupt` of the running turn answers its open requests
// `cancelled`, withdrawing their cards, cancels the session's prompt and drops the prompts queued after it; once the
// prompt's reply comes, its outcome is `done` with the reply's stopReason and the uuids dropped. A permission request
// is held by its JSON-RPC id (its card named by the start's uuid and that id) and sent as `permission`; ccd's answer
// picks the request's own allow_once or reject_once option, and `done` follows once it is written to agy. `stop`
// answers every request still open `cancelled`, withdrawing its card, cancels the session's prompt and ends agy's
// input; a prompt agy then leaves unanswered ends `cancelled`, one cut off by any other end of agy's in the error "agy
// ended". agy's end is `exited`. Settings changes are refused: a later slice.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import type { Unsequenced } from './link.ts';
import type { Json, PermissionRequest, ToHost } from './protocol.ts';
import type { Say, Start, Wake } from './start.ts';

/** The ACP version this host speaks. */
const PROTOCOL_VERSION = 1;

type Answer = Extract<ToHost, { type: 'answer' }>;
type Interrupt = Extract<ToHost, { type: 'interrupt' }>;
type Message = { id?: number | string; method?: string; params?: any; result?: any; error?: { message?: string } };
type Held = { id: number | string; request: PermissionRequest; options: { optionId: string; kind: string }[] };

export function runAcp(start: Start, send: (body: Unsequenced) => void, finish: () => void) {
  const agy = spawn(start.options.claude_path, ['acp'], { cwd: start.options.cwd, stdio: ['pipe', 'pipe', 'inherit'] });
  // Writes after agy has gone fail here, not in the host: agy's end is reported as `exited`.
  agy.stdin.on('error', () => {});
  const write = (message: object) => agy.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
  let nextId = 1;
  const calls = new Map<number, (message: Message) => void>();
  // The running prompt's call, and whether the run was stopped: a prompt agy leaves unanswered when it ends after a
  // stop ends `cancelled`, the turn the person stopped; after any other end of agy's, in an error.
  let promptCall: number | undefined;
  let stopped = false;
  const call = (method: string, params: object) =>
    new Promise<Message>((resolve) => {
      const id = nextId++;
      calls.set(id, resolve);
      if (method === 'session/prompt') promptCall = id;
      write({ id, method, params });
    });
  const held = new Map<string, Held>();
  let sessionId: string | undefined;
  let ended = false;
  // The prompts waiting, in the order ccd sent them; the turn running, by its command's uuid; and an app's Stop of it,
  // until its reply comes.
  const queued: { uuid: string; text: string }[] = [];
  let running: string | undefined;
  let stopping: { uuid: string; cancelled: string[] } | undefined;
  // What agy says before the start's outcome waits for it: ccd files a run's messages only once it is live.
  let early: Json[] | undefined = [];
  const forward = (message: Json) =>
    early ? early.push(message) : send({ type: 'acp', ...(running === undefined ? {} : { turn: running }), message });
  const refuse = (uuid: string, detail: string) => send({ type: 'outcome', uuid, status: 'refused', detail });

  createInterface({ input: agy.stdout }).on('line', (text) => {
    let message: Message;
    try {
      message = JSON.parse(text);
    } catch {
      return;
    }
    if (message.method === 'session/request_permission' && message.id !== undefined) {
      const update = message.params?.toolCall ?? {};
      // agy numbers its requests from 1 in each process, so a card is named by this host's start too: a relaunched
      // run's requests never take its earlier cards' names.
      const request: PermissionRequest = {
        tool_use_id: `${start.uuid}/${message.id}`,
        tool_name: String(update.title ?? update.kind ?? 'tool'),
        input: (update.rawInput ?? {}) as Json,
        options: (message.params?.options ?? []) as Json,
      };
      held.set(request.tool_use_id, { id: message.id, request, options: message.params?.options ?? [] });
      send({ type: 'permission', request });
    } else if (message.method !== undefined && message.id !== undefined) {
      // A request the host does not serve: the file system and terminal it never offered.
      write({ id: message.id, error: { code: -32601, message: `${message.method} is not offered` } });
    } else if (message.method !== undefined) {
      forward(message as Json);
    } else if (typeof message.id === 'number' && calls.has(message.id)) {
      const resolve = calls.get(message.id)!;
      calls.delete(message.id);
      resolve(message);
    }
  });

  // The prompts, one at a time: the next is sent once the one before has its reply, which goes to ccd as the turn's
  // end, and a Stop's outcome after it.
  let current: Promise<void> = Promise.resolve();
  const next = () => {
    if (running !== undefined || ended || sessionId === undefined || queued.length === 0) return;
    const { uuid, text } = queued.shift()!;
    running = uuid;
    current = call('session/prompt', { sessionId, prompt: [{ type: 'text', text }] }).then((reply) => {
      send({ type: 'acp', turn: uuid, message: reply as Json });
      running = undefined;
      if (stopping) {
        const reason = reply.result?.stopReason;
        send({
          type: 'outcome',
          uuid: stopping.uuid,
          status: 'done',
          ...(typeof reason === 'string' ? { terminal_reason: reason } : {}),
          cancelled: stopping.cancelled,
        });
        stopping = undefined;
      }
      next();
    });
  };
  // Each open request answered `cancelled`, as the spec has a cancelled turn's, and its card withdrawn.
  const cancelHeld = () => {
    for (const [toolUseId, open] of held) {
      write({ id: open.id, result: { outcome: { outcome: 'cancelled' } } });
      send({ type: 'withdrawn', tool_use_id: toolUseId });
    }
    held.clear();
  };

  const opened = (async () => {
    const initialized = await call('initialize', {
      protocolVersion: PROTOCOL_VERSION,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
    });
    if (initialized.error) throw new Error(initialized.error.message ?? 'agy refused initialize');
    if (initialized.result?.protocolVersion !== PROTOCOL_VERSION) {
      throw new Error(`agy speaks ACP version ${initialized.result?.protocolVersion}, not ${PROTOCOL_VERSION}`);
    }
    const mail = start.options.mail;
    const opening = {
      cwd: start.options.cwd,
      mcpServers: [{ name: 'codeconnect', command: mail.command, args: mail.args, env: [] }],
    };
    const resume = start.options.resume;
    if (resume !== undefined) {
      if (initialized.result?.agentCapabilities?.sessionCapabilities?.resume === undefined) {
        throw new Error('this agy does not offer session/resume, so the session cannot go on');
      }
      const resumed = await call('session/resume', { sessionId: resume, ...opening });
      if (resumed.error) throw new Error(resumed.error.message ?? 'agy refused to resume the session');
      sessionId = resume;
    } else {
      const session = await call('session/new', opening);
      if (session.error) throw new Error(session.error.message ?? 'agy refused the session');
      sessionId = String(session.result?.sessionId);
    }
    // The model the app chose, set on the session before its first prompt.
    const model = start.options.model;
    if (model !== undefined) {
      const set = await call('session/set_model', { sessionId, modelId: model });
      if (set.error) throw new Error(set.error.message ?? `agy refused the model ${model}`);
    }
  })();
  opened.then(
    () => {
      send({ type: 'outcome', uuid: start.uuid, status: 'done' });
      for (const message of early ?? []) send({ type: 'acp', message });
      early = undefined;
      if (start.prompt !== '') queued.push({ uuid: start.uuid, text: start.prompt });
      next();
    },
    (error) => {
      refuse(start.uuid, error instanceof Error ? error.message : String(error));
      agy.stdin.end();
    },
  );

  agy.once('exit', (code, signal) => {
    ended = true;
    for (const [id, resolve] of calls) {
      const cancelled = stopped && id === promptCall;
      resolve(cancelled ? { result: { stopReason: 'cancelled' } } : { error: { message: 'agy ended' } });
    }
    calls.clear();
    held.clear();
    opened.catch(() => {}).finally(() =>
      current.finally(() => {
        send({ type: 'exited', ...(code === null ? {} : { code }), ...(signal === null ? {} : { signal }) });
        finish();
      }),
    );
  });
  // agy not found or not runnable: no exit follows an error at spawn.
  agy.once('error', (error) => {
    if (agy.pid !== undefined) return;
    ended = true;
    refuse(start.uuid, error.message);
    finish();
  });

  return {
    say(command: Say | Wake): void {
      if (ended) {
        refuse(command.uuid, 'the run is not taking messages');
        return;
      }
      queued.push({ uuid: command.uuid, text: command.text });
      send({ type: 'outcome', uuid: command.uuid, status: 'done' });
      next();
    },
    interrupt(command: Interrupt): void {
      if (running === undefined || running !== command.turn_id || stopping) {
        refuse(command.uuid, 'that turn is not running');
        return;
      }
      stopping = { uuid: command.uuid, cancelled: queued.splice(0).map((prompt) => prompt.uuid) };
      cancelHeld();
      write({ method: 'session/cancel', params: { sessionId } });
    },
    answer(command: Answer): void {
      const request = held.get(command.tool_use_id);
      const kind = command.decision.behavior === 'allow' ? 'allow_once' : 'reject_once';
      const option = request?.options.find((offered) => offered.kind === kind);
      if (request === undefined || option === undefined) {
        refuse(command.uuid, request === undefined ? 'the request is no longer open' : `agy offered no ${kind}`);
        return;
      }
      held.delete(command.tool_use_id);
      write({ id: request.id, result: { outcome: { outcome: 'selected', optionId: option.optionId } } });
      send({ type: 'outcome', uuid: command.uuid, status: 'done' });
    },
    pending: () => [...held.values()].map((open) => open.request),
    stop(): void {
      stopped = true;
      cancelHeld();
      if (sessionId !== undefined) write({ method: 'session/cancel', params: { sessionId } });
      agy.stdin.end();
    },
  };
}
