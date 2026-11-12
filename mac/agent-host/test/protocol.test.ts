// The protocol's TypeScript (src/protocol.ts), and the host code that builds its lines (src/link.ts), against
// fixtures/host/messages.jsonl: the lines mac/protocol/src/host.rs's tests emit, never a hand-written fixture. Each
// golden line becomes a typed constant in a generated module, which the pinned Bun's type checker must accept, so a
// renamed, missing or extra field fails, naming the golden line; and every variant of each direction has a line.
import { expect, test } from 'bun:test';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { HOST_PROTOCOL_VERSION } from '../src/protocol.ts';

const src = join(import.meta.dir, '..', 'src');
const golden = readFileSync(join(import.meta.dir, '..', '..', '..', 'fixtures', 'host', 'messages.jsonl'), 'utf8')
  .split('\n')
  .filter((text) => text !== '')
  .map((text) => JSON.parse(text) as { dir: string; message: { type?: string; protocol_version?: number } });
const typeOf: Record<string, string> = { to_host: 'ToHost', from_host: 'FromHost', host_json: 'HostFile' };

test('every golden line is a value of its type, every variant has a line, and link.ts type-checks', () => {
  const module = [`import type { FromHost, HostFile, ToHost } from ${JSON.stringify(join(src, 'protocol.ts'))};`];
  golden.forEach(({ dir, message }, index) => {
    expect(typeOf[dir]).toBeDefined();
    module.push(`export const line${index + 1}: ${typeOf[dir]} = ${JSON.stringify(message)};`);
  });
  for (const [dir, type] of [['to_host', 'ToHost'], ['from_host', 'FromHost']]) {
    const tags = [...new Set(golden.filter((entry) => entry.dir === dir).map((entry) => JSON.stringify(entry.message.type)))];
    const missing = `Exclude<${type}['type'], ${tags.join(' | ')}>`;
    module.push(`export const every_${dir}_variant_has_a_line: [${missing}] extends [never] ? true : ${missing} = true;`);
  }
  const dir = mkdtempSync(join(tmpdir(), 'cc-agent-host-'));
  try {
    writeFileSync(join(dir, 'golden.ts'), `${module.join('\n')}\n`);
    const check = Bun.spawnSync([process.execPath, 'check', '--no-pretty', '--exactOptionalPropertyTypes',
      join(dir, 'golden.ts'), join(src, 'link.ts')], { env: { ...process.env, BUN_BE_BUN: '1' } });
    // golden.ts's row n + 1 holds golden line n; the rows after them are the variant checks.
    const errors = `${check.stdout}${check.stderr}`.replace(/\S*golden\.ts\((\d+),\d+\)/g, (_, row: string) =>
      Number(row) - 1 <= golden.length ? `golden line ${Number(row) - 1}` : 'every variant has a line');
    if (check.exitCode !== 0) throw new Error(errors);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("host.json's version is the one host.rs writes", () => {
  const hostFiles = golden.filter((entry) => entry.dir === 'host_json');
  expect(hostFiles.length).toBeGreaterThan(0);
  for (const { message } of hostFiles) expect(message.protocol_version).toBe(HOST_PROTOCOL_VERSION);
});
