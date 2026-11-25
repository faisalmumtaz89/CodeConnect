// The agent host, one per Claude or Antigravity run (acp.ts), started by ccd as `cc-agent-host <run dir>`. It records
// itself in <run dir>/host.json and speaks the lines of mac/protocol/src/host.rs to ccd over <run dir>/host.sock
// (link.ts).
//
// host.json is created exclusively: a second host for the same run finds it and exits 3, touching nothing. Having won
// it, the host removes a socket file a dead host left, and listens. It exits 4 if no `start` arrives within 30 s.
//
// The first `start` runs the run's one query() (start.ts); a later one is ignored. claude stays in the host's process
// group (H2). The start's outcome carries the initialize answer, or the reason the start failed. After an answer,
// every message the query yields follows as `sdk`, a `commands_changed` without the claude.ai connectors' commands as
// the answer is (start.ts); after a refusal, none does. A `say` is one more message from the
// person, given to the query in the order ccd sent it, with its outcome `done` once the SDK has taken it; one the run
// cannot take (before the start, or once its input has ended) is `refused`. A `wake` is ccd's line waking the agent
// for its mail, queued and answered as a `say` is, marked as a background notification rather than the person's
// words (start.ts `userMessage`). claude's permission requests are held for
// ccd's `answer` (permissions.ts), each answer told back once the SDK has written it into claude's input; the state
// message lists those still open. `set_model` and `set_mode` change the run's model, effort or permission mode
// through the SDK's own calls, one line at a time in the order ccd sent them: a model with `setModel`, an effort alone
// with `applyFlagSettings` (never `setModel`, which with no model resets it), a mode with `setPermissionMode`; each
// is `done` once claude has taken it, or `refused` with claude's reason. `stop` ends the run (H5): the query is
// interrupted, its input ended and the query
// closed, so claude exits; a second `stop` does nothing. `interrupt` is an app's Stop of one turn, taken only while that
// turn (its `system/init` uuid, until its `result`) is the one claude is running, and refused otherwise: the query is
// interrupted with its queued messages cancelled, and the outcome `done` follows once both the turn's result and the
// interrupt's receipt are in, with the result's `terminal_reason` and the receipt's `cancelled`. claude's end is the last
// message, `exited`. Once the run is over and ccd has acknowledged everything, the host exits 0.
import { query } from '@anthropic-ai/claude-agent-sdk';
import { runAcp } from './acp.ts';
import { spawn } from 'node:child_process';
import { closeSync, openSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { join } from 'node:path';
import { createLink } from './link.ts';
import { createPermissions } from './permissions.ts';
import { printUsage } from './usage.ts';
import { HOST_PROTOCOL_VERSION, type HostFile, type PermissionRequest, type ToHost } from './protocol.ts';
import {
  type Facts,
  firstMessage,
  forwarded,
  initializeAnswer,
  noteMessage,
  oneLine,
  queryOptions,
  type Say,
  type Start,
  startFacts,
  userMessage,
  type Wake,
} from './start.ts';

// `cc-agent-host --usage <claude>`: ccd's short launch for an account's plan limits (usage.ts), then out.
if (process.argv[2] === '--usage') {
  await printUsage(process.argv[3]!);
  process.exit(0);
}

const runDir = process.argv[2];
const recordPath = join(runDir, 'host.json');

let record: number;
try {
  record = openSync(recordPath, 'wx', 0o600);
} catch (error) {
  if ((error as { code?: unknown }).code !== 'EEXIST') throw error;
  console.error(`${recordPath} exists: another host has this run`);
  process.exit(3);
}
const hostFile: HostFile = { protocol_version: HOST_PROTOCOL_VERSION, host_pid: process.pid };
writeFileSync(record, JSON.stringify(hostFile));
closeSync(record);

const noStart = setTimeout(() => {
  console.error('no start within 30 s');
  process.exit(4);
}, 30_000);

let facts: Facts = {};
const link = createLink({
  start: begin,
  say: (command) => (say ? say(command) : refuse(command.uuid)),
  wake: (command) => (say ? say(command) : refuse(command.uuid)),
  answer: (command) =>
    permissions
      ? permissions.answer(command)
      : link.send({ type: 'outcome', uuid: command.uuid, status: 'refused', detail: 'the request is no longer open' }),
  interrupt: (command) =>
    interrupt
      ? interrupt(command)
      : link.send({ type: 'outcome', uuid: command.uuid, status: 'refused', detail: 'that turn is not running' }),
  setting: (command) =>
    setting
      ? setting(command)
      : link.send({ type: 'outcome', uuid: command.uuid, status: 'refused', detail: 'the run has not started' }),
  stop: () => stop?.(),
  facts: () => facts,
  pending: () => permissions?.pending() ?? [],
  drained: () => process.exit(0),
});

// The run's permission requests, once it has a query (or, for Antigravity, an agy).
let permissions:
  | { answer(command: Extract<ToHost, { type: 'answer' }>): void; pending(): PermissionRequest[] }
  | undefined;

// A message the run cannot take: before its start, or once its input has ended.
function refuse(uuid: string): void {
  link.send({ type: 'outcome', uuid, status: 'refused', detail: 'the run is not taking messages' });
}

// The run's later messages, the person's and ccd's mail wakes, while its input is open.
let say: ((command: Say | Wake) => void) | undefined;

// A start after the first is ignored: ccd re-sends a start whose outcome it does not hold yet, and the first one's
// outcome is still to come.
let begun = false;

// The run's stop, once it has a query to stop.
let stop: (() => void) | undefined;

// An app's Stop of one turn, once the run has a query.
let interrupt: ((command: Extract<ToHost, { type: 'interrupt' }>) => void) | undefined;

// A change of the run's settings, once the run has a query.
let setting: ((command: Extract<ToHost, { type: 'set_model' | 'set_mode' }>) => void) | undefined;

function begin(start: Start): void {
  if (begun) return;
  begun = true;
  clearTimeout(noStart);
  facts = startFacts(start.options);
  // An Antigravity run is agy's, over the Agent Client Protocol (acp.ts); it takes no wake yet.
  if (start.options.agent === 'antigravity') {
    const run = runAcp(start, (body) => link.send(body), () => link.finish());
    permissions = run;
    say = (command) => (command.type === 'say' ? run.say(command) : refuse(command.uuid));
    stop = () => {
      stop = undefined;
      run.stop();
    };
    return;
  }
  let claudeEnded: Promise<{ code: number | null; signal: string | null }> | undefined;
  // The run's later messages, in the order ccd sent them. Each one's outcome is `done` once the SDK has taken it from
  // the input; claude then folds it into the running turn or runs it next.
  const queued: (Say | Wake)[] = [];
  let wake = () => {};
  let inputEnded = false;
  say = (command) => {
    queued.push(command);
    wake();
  };
  // The input ends with the run's stop or claude's own end: a message still queued, or sent later, is refused.
  const endInput = () => {
    inputEnded = true;
    say = undefined;
    for (const command of queued.splice(0)) refuse(command.uuid);
    wake();
  };
  const input = (async function* () {
    // A start with an empty prompt has no first message: the run waits for the person's first say.
    if (start.prompt !== '') yield firstMessage(start);
    for (;;) {
      while (queued.length === 0 && !inputEnded) {
        await new Promise<void>((resolve) => {
          wake = resolve;
        });
      }
      if (inputEnded) return;
      const command = queued.shift()!;
      yield userMessage(command.uuid, command.text, command.type);
      link.send({ type: 'outcome', uuid: command.uuid, status: 'done' });
    }
  })();
  const held = createPermissions((body) => link.send(body));
  permissions = held;
  const run = query({
    prompt: input,
    options: {
      ...queryOptions(start.options),
      canUseTool: held.canUseTool,
      // No `detached`: claude stays in the host's process group, whose id is the host's pid. Its stderr is the host's
      // own (host.log), so no unread pipe can stall it.
      spawnClaudeCodeProcess: (spawned) => {
        const child = spawn(spawned.command, spawned.args, {
          cwd: spawned.cwd,
          env: spawned.env,
          signal: spawned.signal,
          stdio: ['pipe', 'pipe', 'inherit'],
        });
        if (child.pid !== undefined) {
          claudeEnded = new Promise((resolve) => child.once('exit', (code, signal) => resolve({ code, signal })));
        }
        // What the SDK writes into claude's input: an answer is told back once it is written there.
        const stdin = child.stdin!;
        const write = stdin.write.bind(stdin) as (...args: unknown[]) => boolean;
        stdin.write = ((chunk: unknown, ...rest: unknown[]) => {
          const flowing = write(chunk, ...rest);
          held.written(chunk);
          return flowing;
        }) as typeof stdin.write;
        return child;
      },
    },
  });
  stop = () => {
    stop = undefined;
    run.interrupt().catch(() => {});
    endInput();
    run.close();
  };
  // The run's settings, each line's calls once the line before has been answered, so two changes reach claude in the
  // order ccd sent them.
  let settings: Promise<void> = Promise.resolve();
  setting = (command) => {
    settings = settings.then(async () => {
      try {
        if (command.type === 'set_mode') {
          await run.setPermissionMode(command.mode as Parameters<typeof run.setPermissionMode>[0]);
        } else if (command.model === undefined && command.effort === undefined) {
          throw new Error('nothing to change');
        } else {
          if (command.model !== undefined) await run.setModel(command.model);
          if (command.effort !== undefined) {
            await run.applyFlagSettings({ effortLevel: command.effort } as Parameters<typeof run.applyFlagSettings>[0]);
          }
        }
        link.send({ type: 'outcome', uuid: command.uuid, status: 'done' });
      } catch (error) {
        link.send({ type: 'outcome', uuid: command.uuid, status: 'refused', detail: oneLine(error) });
      }
    });
  };
  // The turn claude is running: its `system/init` uuid, until that turn's `result`.
  let running: string | undefined;
  // An app's Stop of the running turn, until both its turn's result and the interrupt's receipt are in, in either
  // order: claude writes the receipt first, but a turn that ends as the Stop arrives writes its result first.
  let stopping: { uuid: string; ended: boolean; reason?: string; answered: boolean; cancelled: string[] } | undefined;
  const stopped = () => {
    if (!stopping?.ended || !stopping.answered) return;
    const { uuid, reason, cancelled } = stopping;
    stopping = undefined;
    link.send({ type: 'outcome', uuid, status: 'done', ...(reason === undefined ? {} : { terminal_reason: reason }), cancelled });
  };
  // The SDK's interrupt takes `{ cancelQueued }` and returns the receipt (sdk.mjs), though its type shows no argument.
  const interruptAll = run.interrupt.bind(run) as unknown as (options: {
    cancelQueued: true;
  }) => Promise<{ cancelled?: string[] } | undefined>;
  interrupt = (command) => {
    if (running === undefined || running !== command.turn_id) {
      link.send({ type: 'outcome', uuid: command.uuid, status: 'refused', detail: 'that turn is not running' });
      return;
    }
    const ask = { uuid: command.uuid, ended: false, answered: false, cancelled: [] as string[] };
    stopping = ask;
    // A rejection means claude went: the run's end follows, and no outcome.
    interruptAll({ cancelQueued: true }).then(
      (receipt) => {
        ask.cancelled = receipt?.cancelled ?? [];
        ask.answered = true;
        stopped();
      },
      () => {},
    );
  };
  // The running turn, from each message after it is sent: a stopped turn's result is sent before its outcome.
  const noteTurn = (message: { type?: unknown; subtype?: unknown; uuid?: unknown; terminal_reason?: unknown }) => {
    if (message.type === 'system' && message.subtype === 'init' && typeof message.uuid === 'string') {
      running = message.uuid;
    } else if (message.type === 'result') {
      running = undefined;
      if (stopping) {
        stopping.ended = true;
        if (typeof message.terminal_reason === 'string') stopping.reason = message.terminal_reason;
        stopped();
      }
    }
  };
  // Whether claude answered initialize.
  const outcome = run.initializationResult().then(
    (answer) => {
      link.send({ type: 'outcome', uuid: start.uuid, status: 'done', initialize: initializeAnswer(answer) });
      return true;
    },
    (error) => {
      link.send({ type: 'outcome', uuid: start.uuid, status: 'refused', detail: oneLine(error) });
      run.close();
      return false;
    },
  );
  void (async () => {
    // The outcome is the run's first line to ccd. claude can write the first message's own lines before it answers
    // initialize; they wait for it, and a refused start sends none of them.
    const answered = await outcome;
    try {
      for await (const message of run) {
        if (!answered) continue;
        facts = noteMessage(facts, message);
        link.send({ type: 'sdk', message: forwarded(message) });
        noteTurn(message);
      }
    } catch (error) {
      console.error(`the query ended: ${oneLine(error)}`);
    }
    endInput();
    held.ended();
    // claude's end comes after every message the query yielded. A claude that never started (no pid) has no end to
    // report.
    const ended = await claudeEnded;
    if (ended) {
      link.send({
        type: 'exited',
        ...(ended.code === null ? {} : { code: ended.code }),
        ...(ended.signal === null ? {} : { signal: ended.signal }),
      });
    }
    link.finish();
  })();
}

const socketPath = join(runDir, 'host.sock');
rmSync(socketPath, { force: true });
createServer((socket) => {
  socket.setEncoding('utf8');
  // A ccd that went away: its next attach replays what it missed.
  socket.on('error', () => {});
  let partial = '';
  socket.on('data', (chunk: string) => {
    partial = link.read(socket, partial + chunk);
  });
  socket.on('close', () => link.closed(socket));
}).listen(socketPath);
