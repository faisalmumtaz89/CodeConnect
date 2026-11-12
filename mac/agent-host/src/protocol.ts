// The lines between ccd and the agent host: the TypeScript side of mac/protocol/src/host.rs, which is the contract.
// One JSON object per line over the run's unix socket, tagged by `type`, snake_case. test/protocol.test.ts checks
// these types against fixtures/host/messages.jsonl, the lines host.rs's own tests emit.

/** The version the host writes into `host.json`. */
export const HOST_PROTOCOL_VERSION = 1;

/** A JSON value the host passes on untouched (serde_json's `Value`). */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** `host.json` in the run's folder, created by the host exclusively. */
export interface HostFile {
  protocol_version: number;
  host_pid: number;
  /** Written before claude is handed to the SDK. */
  claude_pid?: number;
}

/** CodeConnect's mail server, run by Claude as a stdio MCP server. */
export interface McpCommand {
  command: string;
  args: string[];
}

/** What the run's `query()` is started with. */
export interface StartOptions {
  cwd: string;
  claude_path: string;
  permission_mode: string;
  trust: 'trusted' | 'not_trusted';
  /** The Claude session to continue. */
  resume?: string;
  model?: string;
  mail: McpCommand;
}

/** An answer to a permission request, as the SDK's `PermissionResult` takes it. */
export type Decision = { behavior: 'allow'; updated_input?: Json } | { behavior: 'deny'; message: string };

/** A line ccd sends the host. `attach` is the first line of every connection; every command carries ccd's uuid. */
export type ToHost =
  | { type: 'attach'; after: number }
  | { type: 'ack'; seq: number }
  | { type: 'start'; uuid: string; options: StartOptions; prompt: string }
  | { type: 'say'; uuid: string; text: string }
  | { type: 'answer'; uuid: string; tool_use_id: string; decision: Decision }
  | { type: 'set_model'; uuid: string; model: string; effort?: string }
  | { type: 'set_mode'; uuid: string; mode: string }
  | { type: 'interrupt'; uuid: string }
  | { type: 'stop'; uuid: string };

/** A `canUseTool` call: `options` is the SDK's, without its signal, tool-use id and control request id. */
export interface PermissionRequest {
  tool_use_id: string;
  tool_name: string;
  input: Json;
  options: Json;
}

/** Of the account, only which provider Claude uses. */
export interface Account {
  api_provider?: string;
}

/** The SDK's initialize answer, as far as ccd keeps it. */
export interface InitializeAnswer {
  commands: Json[];
  models: Json[];
  account: Account;
}

/** A line the host sends ccd. Each carries its sequence number and is kept until ccd acknowledges it. */
export type FromHost =
  | { type: 'sdk'; seq: number; message: Json }
  | { type: 'permission'; seq: number; request: PermissionRequest }
  | { type: 'withdrawn'; seq: number; tool_use_id: string }
  | {
      type: 'outcome';
      seq: number;
      uuid: string;
      status: 'done' | 'refused';
      detail?: string;
      initialize?: InitializeAnswer;
      cancelled?: string[];
    }
  | {
      type: 'state';
      seq: number;
      session_id?: string;
      model?: string;
      effort?: string;
      permission_mode?: string;
      pending: PermissionRequest[];
    }
  | { type: 'exited'; seq: number; code?: number; signal?: string };
