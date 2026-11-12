// The agent host, one per Claude run, started by ccd as `cc-agent-host <run dir>`. It records itself in
// <run dir>/host.json and speaks the lines of mac/protocol/src/host.rs to ccd over <run dir>/host.sock (link.ts).
//
// host.json is created exclusively: a second host for the same run finds it and exits 3, touching nothing. Having won
// it, the host removes a socket file a dead host left, and listens. It exits 4 if no `start` arrives within 30 s.
import { closeSync, openSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { join } from 'node:path';
import { createLink } from './link.ts';
import { HOST_PROTOCOL_VERSION, type HostFile } from './protocol.ts';

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

const link = createLink(() => clearTimeout(noStart));
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
