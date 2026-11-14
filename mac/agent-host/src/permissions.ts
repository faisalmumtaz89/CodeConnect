// The run's permission requests: each `canUseTool` call claude makes is held open and sent to ccd as `permission`,
// answered once by ccd's `answer`, and told back `done` only once the SDK has written the answer into claude's input,
// so `done` means the answer reached claude. A request the SDK drops unanswered (an interrupt, the run's stop or its
// end) is `withdrawn`; an answer to a request no longer open, or one the run ended before it could be written, is
// `refused`. A question (AskUserQuestion) is one of them: ccd's answer to it carries the answers as its input.
import type { CanUseTool, PermissionResult } from '@anthropic-ai/claude-agent-sdk';
import type { Unsequenced } from './link.ts';
import type { Json, PermissionRequest, ToHost } from './protocol.ts';

type Answer = Extract<ToHost, { type: 'answer' }>;

export function createPermissions(send: (body: Unsequenced) => void) {
  // Open requests by tool-use id, each with its control request id and the SDK's resolver.
  const open = new Map<string, { request: PermissionRequest; controlId: string; resolve: (result: PermissionResult | null) => void }>();
  // Answers resolved and not yet seen written to claude's input: ccd's answer uuid by control request id.
  const writing = new Map<string, string>();

  const canUseTool: CanUseTool = (toolName, input, options) => {
    const { signal, toolUseID, requestId, ...rest } = options;
    const request: PermissionRequest = {
      tool_use_id: toolUseID,
      tool_name: toolName,
      input: input as Json,
      options: JSON.parse(JSON.stringify(rest)) as Json,
    };
    return new Promise((resolve) => {
      open.set(toolUseID, { request, controlId: requestId, resolve });
      signal.addEventListener(
        'abort',
        () => {
          if (!open.delete(toolUseID)) return;
          send({ type: 'withdrawn', tool_use_id: toolUseID });
          // No answer is written for a request the SDK dropped.
          resolve(null);
        },
        { once: true },
      );
      send({ type: 'permission', request });
    });
  };

  function answer(command: Answer): void {
    const held = open.get(command.tool_use_id);
    if (held === undefined) {
      send({ type: 'outcome', uuid: command.uuid, status: 'refused', detail: 'the request is no longer open' });
      return;
    }
    open.delete(command.tool_use_id);
    writing.set(held.controlId, command.uuid);
    const decision = command.decision;
    held.resolve(
      decision.behavior === 'allow'
        ? { behavior: 'allow', updatedInput: (decision.updated_input ?? held.request.input) as Record<string, unknown> }
        : { behavior: 'deny', message: decision.message, interrupt: decision.interrupt === true },
    );
  }

  /** A chunk the SDK wrote to claude's input: an answer's control response in it has reached claude. */
  function written(chunk: unknown): void {
    if (writing.size === 0) return;
    for (const text of String(chunk).split('\n')) {
      if (!text.includes('"control_response"')) continue;
      let id: unknown;
      try {
        id = JSON.parse(text)?.response?.request_id;
      } catch {
        continue;
      }
      const uuid = typeof id === 'string' ? writing.get(id) : undefined;
      if (uuid === undefined) continue;
      writing.delete(id as string);
      send({ type: 'outcome', uuid, status: 'done' });
    }
  }

  /** The run is over: an answer never written to claude's input never reached it. */
  function ended(): void {
    for (const uuid of writing.values()) {
      send({ type: 'outcome', uuid, status: 'refused', detail: 'the run ended before the answer reached Claude' });
    }
    writing.clear();
  }

  return { canUseTool, answer, written, ended, pending: () => [...open.values()].map((held) => held.request) };
}
