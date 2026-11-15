// The stand-in claude a test gives a run as its claude (rig.ts writeStub), run on the built host as Bun. It speaks the
// SDK's stream-json over stdio as claude does: it answers `initialize` with its script's answer (a recorded claude's
// own, from fixtures/claude/), after any lines its script has it write first, and after the first user message writes
// its script's lines. Every other control request is refused and recorded, so a test sees any the SDK sends unasked.
// It records its argv, its environment, its pid, and every line it read.
//
//   stub-claude.ts <script.json> <claude's own arguments...>
import { appendFileSync, readFileSync } from 'node:fs';
import { createInterface } from 'node:readline';

const script = JSON.parse(readFileSync(process.argv[2]!, 'utf8'));
const record = (event: object) => appendFileSync(script.log, `${JSON.stringify(event)}\n`);
const write = (message: object) => process.stdout.write(`${JSON.stringify(message)}\n`);

record({ argv: process.argv.slice(3), pid: process.pid, env: process.env });
for await (const text of createInterface({ input: process.stdin })) {
  const message = JSON.parse(text);
  record({ stdin: message });
  if (message.type === 'control_request' && message.request?.subtype === 'initialize') {
    if (script.die_on_init) process.exit(1);
    // Lines claude writes before its answer, as a slow answer lets the first message's own lines come first.
    process.stdout.write((script.before_init ?? []).map((line: string) => `${line}\n`).join(''));
    if (script.init_error) {
      write({ type: 'control_response', response: { subtype: 'error', request_id: message.request_id, error: 'refused by the stand-in' } });
    } else {
      const answer = JSON.parse(script.init);
      answer.response.request_id = message.request_id;
      write(answer);
    }
  } else if (message.type === 'control_request' && message.request?.subtype === 'interrupt' && script.interrupt) {
    // An interrupt answered as claude answers it: its receipt (with `cancelled`), and the stopped turn's own lines (a
    // cancel of an open permission, its result), after the receipt, or before it for a turn that ends as it arrives.
    const receipt = {
      type: 'control_response',
      response: { subtype: 'success', request_id: message.request_id, response: { still_queued: [], cancelled: script.interrupt.cancelled ?? [] } },
    };
    const lines = (script.interrupt.lines ?? []).map((line: string) => `${line}\n`).join('');
    if (script.interrupt.result_first) {
      process.stdout.write(lines);
      write(receipt);
    } else {
      write(receipt);
      process.stdout.write(lines);
    }
  } else if (message.type === 'control_request') {
    record({ unexpected: message.request?.subtype });
    write({ type: 'control_response', response: { subtype: 'error', request_id: message.request_id, error: 'not in the stand-in' } });
  } else if (message.type === 'user') {
    const lines = (script.after_user ?? []).map((line: string) => `${line}\n`).join('');
    // Exit only once the lines are written, so none is lost in the pipe.
    process.stdout.write(lines, () => {
      if (script.exit_on_user !== undefined) process.exit(script.exit_on_user);
    });
  }
}
record({ eof: true });
