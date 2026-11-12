// The host's side of its link to ccd (mac/protocol/src/host.rs): sequence numbers, the messages kept until ccd
// acknowledges them, the one attached connection, and whole lines in. It names no Node API, so the pinned Bun's type
// checker checks it against the protocol's types (test/protocol.test.ts); main.ts wires the run's socket to it.
import type { FromHost, ToHost } from './protocol.ts';

/** A connection from ccd: lines out, and closing it. */
export interface Connection {
  write(text: string): unknown;
  destroy(): unknown;
}

type Unsequenced<M = FromHost> = M extends FromHost ? Omit<M, 'seq'> : never;

const line = (message: FromHost): string => `${JSON.stringify(message)}\n`;

/** The link for one host's life. `started` runs when ccd sends `start`. */
export function createLink(started: () => void) {
  let seq = 0;
  const kept: FromHost[] = [];
  let attacher: Connection | null = null;

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

  // A newer attach replaces the attached connection: the older one is closed, so nothing more it sent is applied.
  // ccd's cursor is committed, so what precedes it is dropped; every later message follows in order, then one state.
  function attach(connection: Connection, after: number): void {
    if (attacher !== connection) attacher?.destroy();
    attacher = connection;
    drop(after);
    for (const message of kept) connection.write(line(message));
    send({ type: 'state', pending: [] });
  }

  function apply(connection: Connection, text: string): void {
    let message: ToHost;
    try {
      message = JSON.parse(text);
    } catch {
      return;
    }
    if (message.type === 'attach') attach(connection, message.after);
    else if (message.type === 'ack') drop(message.seq);
    else if (message.type === 'start') started();
  }

  return {
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
