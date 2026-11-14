// The agent host, one per Claude run, started by ccd as `cc-agent-host <run dir>`. It records itself in
// <run dir>/host.json and speaks the lines of mac/protocol/src/host.rs to ccd over <run dir>/host.sock (link.ts).
//
// host.json is created exclusively: a second host for the same run finds it and exits 3, touching nothing. Having won
// it, the host removes a socket file a dead host left, and listens. It exits 4 if no `start` arrives within 30 s.
//
// The first `start` runs the run's one query() (start.ts); a later one is ignored. claude stays in the host's process
// group (H2). The start's outcome carries the initialize answer, or the reason the start failed. After an answer,
// every message the query yields follows as `sdk`; after a refusal, none does. `stop` ends the run (H5): the query is
// interrupted, its input ended and the query closed, so claude exits; a second `stop` does nothing. claude's end is
// the last message, `exited`. Once the run is over and ccd has acknowledged everything, the host exits 0.
import { query } from '@anthropic-ai/claude-agent-sdk';
import { spawn } from 'node:child_process';
import { closeSync, openSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { join } from 'node:path';
import { createLink } from './link.ts';
import { HOST_PROTOCOL_VERSION, type HostFile } from './protocol.ts';
import { type Facts, firstMessage, initializeAnswer, noteMessage, oneLine, queryOptions, type Start, startFacts } from './start.ts';

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
const link = createLink({ start: begin, stop: () => stop?.(), facts: () => facts, drained: () => process.exit(0) });

// A start after the first is ignored: ccd re-sends a start whose outcome it does not hold yet, and the first one's
// outcome is still to come.
let begun = false;

// The run's stop, once it has a query to stop.
let stop: (() => void) | undefined;

function begin(start: Start): void {
  if (begun) return;
  begun = true;
  clearTimeout(noStart);
  facts = startFacts(start.options);
  let claudeEnded: Promise<{ code: number | null; signal: string | null }> | undefined;
  let endInput = () => {};
  const input = (async function* () {
    yield firstMessage(start);
    // The input stays open for the run's later messages, until the run is stopped.
    await new Promise<void>((resolve) => {
      endInput = resolve;
    });
  })();
  const run = query({
    prompt: input,
    options: {
      ...queryOptions(start.options),
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
        link.send({ type: 'sdk', message });
      }
    } catch (error) {
      console.error(`the query ended: ${oneLine(error)}`);
    }
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
