// The run's start (mac/protocol/src/host.rs `start`): the query() options built from StartOptions (design §1, §6),
// the first message, the start outcome's initialize answer, and the run's facts for the state message. It names no
// Node or SDK API, so the pinned Bun's type checker checks it against the protocol's types (test/protocol.test.ts).
import type { InitializeAnswer, Json, StartOptions, ToHost } from './protocol.ts';

export type Start = Extract<ToHost, { type: 'start' }>;
export type Say = Extract<ToHost, { type: 'say' }>;
export type Wake = Extract<ToHost, { type: 'wake' }>;

/** What the state message reports of the run, besides the pending requests. */
export interface Facts {
  session_id?: string;
  model?: string;
  permission_mode?: string;
}

/**
 * The options of the run's one query(), but the spawn hook, which main.ts adds. claude is the one ccd's probe
 * checked, exactly as given. No `env`: claude inherits the host's environment as the SDK passes it, and nothing here
 * points claude at a configuration of its own, so it uses the person's (decision 159). A folder not trusted reads only
 * the person's own settings and only CodeConnect's MCP server (§6).
 */
export function queryOptions(options: StartOptions) {
  return {
    pathToClaudeCodeExecutable: options.claude_path,
    cwd: options.cwd,
    // With no mode named, claude picks its own: the person's `defaultMode`, else its default (decision 182).
    ...(options.permission_mode === undefined ? {} : { permissionMode: options.permission_mode }),
    // The SDK's guard for bypassPermissions, on every start, so the run can take it later when the person confirms it
    // in the app (decision 215). It sets no mode by itself; ccd's confirmation is what decides.
    allowDangerouslySkipPermissions: true,
    ...(options.resume === undefined ? {} : { resume: options.resume }),
    ...(options.model === undefined ? {} : { model: options.model }),
    // Without it the initialize request carries an empty system prompt.
    systemPrompt: { type: 'preset', preset: 'claude_code' } as const,
    mcpServers: { codeconnect: { type: 'stdio', command: options.mail.command, args: options.mail.args } as const },
    // Without replay-user-messages a typed message gets no echo. A worktree is one claude makes and starts in; the
    // run's real cwd is then the one its init names.
    extraArgs: { 'replay-user-messages': null, ...(options.worktree === undefined ? {} : { worktree: options.worktree }) },
    ...(options.trust === 'not_trusted' ? { settingSources: ['user'], strictMcpConfig: true } : {}),
  };
}

/**
 * A message into Claude's input, stamped with ccd's uuid and marked by who it is from (the SDK treats a user message
 * with no `origin` as unattributed, its `SDKMessageOrigin`): a `say` as typed by a person, a `wake` as a background
 * notification, which the mail nudge is.
 */
export function userMessage(uuid: string, text: string, from: 'say' | 'wake' = 'say') {
  return {
    type: 'user',
    message: { role: 'user', content: text },
    parent_tool_use_id: null,
    session_id: '',
    uuid,
    origin: from === 'wake' ? ({ kind: 'task-notification' } as const) : ({ kind: 'human' } as const),
  } as const;
}

/** The start's prompt as the run's first message, stamped with the start's uuid. */
export function firstMessage(start: Start) {
  return userMessage(start.uuid, start.prompt);
}

/**
 * How the initialize answer names a claude.ai connector's commands: `claude.ai <connector>:<prompt> (MCP)`, after the
 * connector's server, which claude names `claude.ai <connector>`.
 */
const CONNECTOR_COMMAND = 'claude.ai ';

function isConnectorCommand(command: Json): boolean {
  return typeof command === 'object' && command !== null && !Array.isArray(command) && String(command.name).startsWith(CONNECTOR_COMMAND);
}

/**
 * The SDK's initialize answer as ccd keeps it. Of the account only which provider Claude uses, in a fresh object:
 * the email, the organization and every other account field never reach a line (H4, P1 22). The claude.ai
 * connectors' commands are left out for the same reason: they name the services the account has connected. A local
 * MCP server named that way is left out with them.
 */
export function initializeAnswer(answer: {
  commands: Json[];
  models: Json[];
  account: { apiProvider?: string };
  claude_code_version?: unknown;
}): InitializeAnswer {
  const provider = answer.account.apiProvider;
  const version = answer.claude_code_version;
  return {
    commands: answer.commands.filter((command) => !isConnectorCommand(command)),
    models: answer.models,
    account: provider === undefined ? {} : { api_provider: provider },
    ...(typeof version === 'string' ? { claude_code_version: version } : {}),
  };
}

/**
 * An SDK message as it goes to ccd: as the query yielded it, but a `system/commands_changed` without the claude.ai
 * connectors' commands, as the initialize answer is.
 */
export function forwarded<M extends { type?: unknown; subtype?: unknown; commands?: unknown }>(message: M): M {
  if (message.type !== 'system' || message.subtype !== 'commands_changed' || !Array.isArray(message.commands)) return message;
  return { ...message, commands: (message.commands as Json[]).filter((command) => !isConnectorCommand(command)) } as M;
}

/** The run's facts as the start names them, until an init says otherwise (H4). */
export function startFacts(options: StartOptions): Facts {
  return {
    ...(options.permission_mode === undefined ? {} : { permission_mode: options.permission_mode }),
    ...(options.model === undefined ? {} : { model: options.model }),
  };
}

/** The facts after one SDK message: each `system/init` names the session, the model and the mode. */
export function noteMessage(
  facts: Facts,
  message: { type?: unknown; subtype?: unknown; session_id?: unknown; model?: unknown; permissionMode?: unknown },
): Facts {
  if (message.type !== 'system' || message.subtype !== 'init') return facts;
  const next = { ...facts };
  if (typeof message.session_id === 'string') next.session_id = message.session_id;
  if (typeof message.model === 'string') next.model = message.model;
  if (typeof message.permissionMode === 'string') next.permission_mode = message.permissionMode;
  return next;
}

/** An error as one line, for a refused start's detail. */
export function oneLine(error: unknown): string {
  const text = error instanceof Error ? error.message : String(error);
  return text.split('\n')[0] ?? '';
}
