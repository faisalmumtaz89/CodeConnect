// The host's side of its link to ccd (mac/protocol/src/host.rs): sequence numbers, the messages kept until ccd
// acknowledges them, the one attached connection, and whole lines in. It names no Node API, so the pinned Bun's type
// checker checks it against the protocol's types (test/protocol.test.ts); main.ts wires the run's socket to it.
import type { FromHost, ToHost } from './protocol.ts';
import type { Facts, Start } from './start.ts';

/** A connection from ccd: lines out, and closing it. */
export interface Connection {
  write(text: string): unknown;
  destroy(): unknown;
}

/** What the link calls on: the run's start and stop, its facts for the state message, and its end. */
export interface Run {
  start(command: Start): void;
  /** ccd ends the run (H5). */
  stop(): void;
  facts(): Facts;
  /** The run is over and ccd has acknowledged every message: nothing is left for the host to do. */
  drained(): void;
}

type Unsequenced<M = FromHost> = M extends FromHost ? Omit<M, 'seq'> : never;

const line = (message: FromHost): string => `${JSON.stringify(message)}\n`;

/** The link for one host's life. */
export function createLink(run: Run) {
  let seq = 0;
  const kept: FromHost[] = [];
  let attacher: Connection | null = null;
  let finished = false;

  // Every message has the next sequence number and is kept until ccd acknowledges it.
  function send(body: Unsequenced): void {
    seq += 1;
    const message = { ...body, seq } as FromHost;
    kept.push(message);
    attacher?.write(line(message));
  }

  // Everything up to `upTo` is committed in ccd's store.
  function drop(upTo: number): void {
    const later = kept.findIndex((message) => message.seq > upTo);
    kept.splice(0, later < 0 ? kept.length : later);
  }

  // Asked after an acknowledgement and when the run ends; an attach asks for itself, once it has dropped what ccd's
  // cursor commits and every superseded state.
  function settle(): void {
    if (finished && kept.length === 0) run.drained();
  }

  // A newer attach replaces the attached connection: the older one is closed, so nothing more it sent is applied.
  // ccd's cursor is committed, so what precedes it is dropped; every later message follows in order, then one state.
  // A state is a snapshot the next one supersedes, so the kept ones go: the replay holds none, and the first state
  // ccd reads on a connection ends its replay.
  function attach(connection: Connection, after: number): void {
    if (attacher !== connection) attacher?.destroy();
    attacher = connection;
    drop(after);
    for (let index = kept.length - 1; index >= 0; index -= 1) {
      if (kept[index]!.type === 'state') kept.splice(index, 1);
    }
    // A run that is over, committed up to ccd's cursor: a ccd that died before its ack re-attaches with that cursor,
    // so this attach is the end's acknowledgement.
    if (finished && kept.length === 0) {
      run.drained();
      return;
    }
    for (const message of kept) connection.write(line(message));
    send({ type: 'state', ...run.facts(), pending: [] });
  }

  function apply(connection: Connection, text: string): void {
    let message: ToHost;
    try {
      message = JSON.parse(text);
    } catch {
      return;
    }
    if (message.type === 'attach') attach(connection, message.after);
    else if (message.type === 'ack') {
      drop(message.seq);
      settle();
    } else if (message.type === 'start') run.start(message);
    else if (message.type === 'stop') run.stop();
  }

  return {
    send,
    /** The run is over: once ccd has acknowledged every message, the run is drained. */
    finish(): void {
      finished = true;
      settle();
    },
    /** Applies each whole line of `text` in order, and returns what follows the last one: a partial line. */
    read(connection: Connection, text: string): string {
      let rest = text;
      for (let end = rest.indexOf('\n'); end >= 0; end = rest.indexOf('\n')) {
        apply(connection, rest.slice(0, end));
        rest = rest.slice(end + 1);
      }
      return rest;
    },
    closed(connection: Connection): void {
      if (attacher === connection) attacher = null;
    },
  };
}
