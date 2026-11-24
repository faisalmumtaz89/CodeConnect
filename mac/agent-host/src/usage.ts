// The host's short `--usage <claude>` launch, for ccd's account facts (306 Slice F): one query() that sends claude no
// message, asks it for its plan's limits (the SDK's get_usage, with the transcript scan skipped), prints what ccd keeps
// of the answer as one JSON line, and ends. claude runs in the environment ccd gave the host (the account's folder),
// from the folder the host was started in, with no settings of anyone's: it is asked one question and never runs a turn.
import { query } from '@anthropic-ai/claude-agent-sdk';

/** One plan-wide window the answer names: its name, the share used in percent (0-100) and its reset in Unix seconds. */
export type KeptWindow = { name: 'five_hour' | 'seven_day'; utilization: number; resets_at: number };

/**
 * What ccd keeps of a get_usage answer: whether plan limits apply to this sign-in, its plan, and the plan-wide windows
 * it names. A window that is null, or without a share or a readable reset, is left out. Everything else in the answer
 * (the session's cost, spend, extra usage, per-model and scoped windows, the organisation's keys) never leaves here.
 */
export function keptUsage(answer: unknown): { available: boolean; plan: string | null; windows: KeptWindow[] } {
  const said = (answer ?? {}) as { rate_limits_available?: unknown; subscription_type?: unknown; rate_limits?: unknown };
  const limits = (said.rate_limits ?? {}) as Record<string, { utilization?: unknown; resets_at?: unknown } | null>;
  const windows: KeptWindow[] = [];
  for (const name of ['five_hour', 'seven_day'] as const) {
    const window = limits[name];
    const resets = typeof window?.resets_at === 'string' ? Date.parse(window.resets_at) : NaN;
    if (typeof window?.utilization === 'number' && Number.isFinite(resets)) {
      windows.push({ name, utilization: window.utilization, resets_at: Math.floor(resets / 1000) });
    }
  }
  return {
    available: said.rate_limits_available === true,
    plan: typeof said.subscription_type === 'string' ? said.subscription_type : null,
    windows,
  };
}

/** Asks the claude at `claude` for its plan's limits and prints [`keptUsage`] of the answer. */
export async function printUsage(claude: string): Promise<void> {
  // An input that never yields: claude is asked no message, so no turn runs.
  const input = (async function* (): AsyncGenerator<never> {
    await new Promise<never>(() => {});
  })();
  // Restricted (Claude Code's --restricted; the SDK has no typed option for it), so this short claude opens no
  // cross-session messaging socket and never appears as a peer in the person's own Claude sessions' ListAgents.
  const run = query({
    prompt: input,
    options: { pathToClaudeCodeExecutable: claude, cwd: process.cwd(), settingSources: [], extraArgs: { restricted: null } },
  });
  try {
    const answer = await run.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({ skipBehaviors: true });
    process.stdout.write(`${JSON.stringify(keptUsage(answer))}\n`);
  } finally {
    run.close();
  }
}
