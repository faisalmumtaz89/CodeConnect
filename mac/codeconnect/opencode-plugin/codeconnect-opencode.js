// CodeConnect's OpenCode TUI plugin (link wire version 1).
//
// It observes the TUI it runs in and forwards what it sees to the CodeConnect daemon over the daemon's unix socket:
// bus events (filtered and size-capped), the session the keyboard is showing, and on every connect a paged snapshot
// read from the OpenCode server, with the liveness verdict of every pending request. It executes nothing.
//
// The keyboard is never delayed by it: `tui()` returns at once, every listener body is synchronous, bounded and
// wrapped in try/catch, and all socket work (connect, hello, snapshot reads, writes) runs in the background. Writes
// are never awaited by a listener; a queue over its cap closes the link and the next connect resynchronises.
//
// Dependency-free on purpose: CodeConnect writes this one file into a private session directory and points
// OPENCODE_TUI_CONFIG at it. `solid-js` is the host's own copy, loaded on demand for the head effect.

import { createConnection } from "node:net"
import { constants as fsc, promises as fsp } from "node:fs"
import { createHash } from "node:crypto"
import { fileURLToPath } from "node:url"

/** @typedef {import("@opencode-ai/plugin/tui").TuiPluginApi} Api */
/** @typedef {import("@opencode-ai/plugin/tui").TuiPluginModule} TuiPluginModule */
/** @typedef {import("node:net").Socket} Socket */
/** @typedef {{ sec: number, usec: number }} Start */
/** @typedef {{ from: string | null, inclusive: boolean }} Bound */
/** @typedef {Record<string, Bound>} Acked */
/** @typedef {{ live: boolean, dead: string | null, listed: boolean | "unknown", in_store: boolean }} Verdict */
/** @typedef {"abort" | "idle" | "retry" | "deleted" | "disposed"} EpochEventType */
/** @typedef {{ seq: number, type: EpochEventType, sid?: string }} EpochEvent */
/** @typedef {{ line: string, bytes: number, stub: boolean }} Encoded */
/** @typedef {{ scope: "full" | "requests", reason: string }} Trigger */
/** @typedef {{ route: string, session_id?: string, directory?: string | null }} Head */

export const WIRE = 1
export const STRING_CAP = 65536
export const FRAME_CAP = 1048576
export const QUEUE_CAP = 4194304
export const PAGE_CAP = 262144
export const HELLO_TIMEOUT_MS = 3000
export const BACKOFF_MIN_MS = 500
export const BACKOFF_MAX_MS = 8000
/** A link counts as stable, and the backoff starts over, after its first `settled` or after this long. */
export const STABLE_MS = 8000
/** Full snapshots start at least this far apart. */
export const FULL_SYNC_GAP_MS = 1000
/** Most epoch events kept; past it the oldest goes, and requests asked before it are judged dead ("epoch-evicted"). */
export const EPOCH_EVENTS_CAP = 4096
const ENDED_CAP = 1024
const AGENT_FILE_CAP = 4096
const API_PAGE_LIMIT = 10
const READ_CAP = 1048576

/** The v1 bus events the daemon maps. Never `message.part.delta`, `session.next.*` or a `*.v2.*` event. */
export const SUBSCRIBED = Object.freeze([
  "session.created",
  "session.status",
  "session.idle",
  "session.error",
  "session.deleted",
  "server.instance.disposed",
  "message.updated",
  "message.removed",
  "message.part.updated",
  "message.part.removed",
  "permission.asked",
  "permission.replied",
  "question.asked",
  "question.replied",
  "question.rejected",
])
const SUBSCRIBED_SET = new Set(SUBSCRIBED)

/** Events after which the pending-request list is read again (no history pages). */
const REQUEST_TRIGGERS = new Set(["session.idle", "session.error", "session.deleted", "server.instance.disposed"])

/** Card fields: exempt from the string cap up to the frame cap, so the daemon classifies the whole text. */
const CARD_FIELDS = new Set(["metadata", "patterns", "always", "anchor_input", "questions"])

const MESSAGE_KEYS = ["id", "sessionID", "role", "parentID", "agent", "model", "modelID", "providerID", "error", "finish", "time"]

// ---------------------------------------------------------------------------------------------------------- caps

const utf8 = (/** @type {string} */ s) => Buffer.byteLength(s, "utf8")
/** @param {unknown} v */
export const encodedLength = (v) => utf8(JSON.stringify(v))
const sha256 = (/** @type {string} */ s) => createHash("sha256").update(s, "utf8").digest("hex")
/** A JSON object that is not an array. */
const isRecord = (/** @type {unknown} */ v) => !!v && typeof v === "object" && !Array.isArray(v)

/**
 * A string whose encoded JSON form is over `cap` bytes keeps its longest prefix (never splitting a surrogate pair)
 * such that prefix plus marker encode to at most `cap`. The marker is the daemon's own:
 * `… (N bytes elided; sha256 of the whole text H)`, N counted in UTF-8 bytes of the raw text.
 * @param {string} s
 * @param {number} [cap]
 */
export function cutString(s, cap = STRING_CAP) {
  if (s.length * 6 + 2 <= cap) return s
  if (s.length + 2 <= cap && encodedLength(s) <= cap) return s
  const total = utf8(s)
  const hash = sha256(s)
  const mark = (/** @type {number} */ kept) => `… (${total - kept} bytes elided; sha256 of the whole text ${hash})`
  const room = cap - encodedLength(mark(0))
  let lo = 0
  let hi = Math.min(s.length, cap)
  while (lo < hi) {
    const k = (lo + hi + 1) >> 1
    if (encodedLength(s.slice(0, k)) <= room) lo = k
    else hi = k - 1
  }
  let k = lo
  const c = s.charCodeAt(k - 1)
  if (k > 0 && c >= 0xd800 && c <= 0xdbff) k--
  const kept = s.slice(0, k)
  return kept + mark(utf8(kept))
}

const EVERYTHING = Symbol("exempt")

/**
 * Copy-on-write walk: removes patch bodies (`summary.diffs`, `filediff`, `revert.diff`, any string `patch`, and a
 * string `diff` under a `metadata` outside the card fields) and cuts strings, except inside the top-level keys
 * named by `exempt`. OpenCode's objects are never mutated.
 * @param {any} v
 * @param {Set<string>} [exempt]
 * @returns {any}
 */
export function prepare(v, exempt) {
  return walk(v, exempt, 0, "")
}

/**
 * @param {any} v
 * @param {Set<string> | typeof EVERYTHING | undefined} exempt
 * @param {number} depth
 * @param {string} key
 * @returns {any}
 */
function walk(v, exempt, depth, key) {
  if (depth > 64) return v
  if (typeof v === "string") return exempt === EVERYTHING ? v : cutString(v)
  if (Array.isArray(v)) {
    /** @type {any[] | null} */
    let o = null
    const sub = exempt === EVERYTHING ? EVERYTHING : undefined
    for (let i = 0; i < v.length; i++) {
      const x = walk(v[i], sub, depth + 1, key)
      if (x !== v[i]) (o ??= v.slice())[i] = x
    }
    return o ?? v
  }
  if (v && typeof v === "object") {
    /** @type {any} */
    let o = null
    for (const k of Object.keys(v)) {
      const drop =
        k === "filediff" ||
        (k === "diffs" && key === "summary") ||
        (k === "diff" && key === "revert") ||
        (k === "patch" && typeof v[k] === "string") ||
        (k === "diff" && key === "metadata" && exempt !== EVERYTHING && typeof v[k] === "string")
      if (drop) {
        o ??= { ...v }
        delete o[k]
        continue
      }
      const sub = exempt === EVERYTHING ? EVERYTHING : depth === 0 && exempt instanceof Set && exempt.has(k) ? EVERYTHING : undefined
      const x = walk(v[k], sub, depth + 1, k)
      if (x !== v[k]) (o ??= { ...v })[k] = x
    }
    return o ?? v
  }
  return v
}

/**
 * The identifiers a stub keeps.
 * @param {any} p
 */
export function ids(p) {
  /** @type {Record<string, string>} */
  const o = {}
  const put = (/** @type {string} */ k, /** @type {unknown} */ v) => {
    if (typeof v === "string") o[k] = v
  }
  put("sessionID", p?.sessionID ?? p?.info?.sessionID ?? p?.part?.sessionID)
  put("id", p?.id)
  put("messageID", p?.info?.id ?? p?.part?.messageID ?? p?.messageID)
  put("partID", p?.part?.id ?? p?.partID)
  put("callID", p?.part?.callID ?? p?.tool?.callID)
  put("parentID", p?.parentID)
  return o
}

/**
 * What a stub keeps of the part it replaces, so the daemon can still close a turn: its type and its tool status.
 * @param {any} part
 */
export function partShape(part) {
  /** @type {{ part_type?: string, status?: string }} */
  const o = {}
  if (typeof part?.type === "string") o.part_type = part.type
  if (typeof part?.state?.status === "string") o.status = part.state.status
  return o
}

/**
 * A live bus frame. Over the frame cap it becomes a stub; its size and sha256 are those of the full frame's encoded
 * JSON, without the newline.
 * @param {number} seq
 * @param {string} type
 * @param {any} properties
 * @returns {Encoded}
 */
export function eventLine(seq, type, properties) {
  const json = JSON.stringify({ t: "ev", seq, type, properties: prepare(properties) })
  const bytes = utf8(json) + 1
  if (bytes <= FRAME_CAP) return { line: json + "\n", bytes, stub: false }
  const s = JSON.stringify({ t: "stub", seq, type, ids: ids(properties), ...partShape(properties?.part), size: bytes - 1, sha256: sha256(json) }) + "\n"
  return { line: s, bytes: utf8(s), stub: true }
}

/**
 * A permission or question card. Card fields are not cut; over the frame cap the card becomes a `card_stub` that
 * carries no request text.
 * @param {number} seq
 * @param {string} type
 * @param {any} card
 * @returns {Encoded}
 */
export function cardLine(seq, type, card) {
  const json = JSON.stringify({ t: "ev", seq, type, properties: prepare(card, CARD_FIELDS) })
  const bytes = utf8(json) + 1
  if (bytes <= FRAME_CAP) return { line: json + "\n", bytes, stub: false }
  const s = JSON.stringify({ t: "card_stub", seq, type, properties: cardIds(card), size: bytes - 1, sha256: sha256(json) }) + "\n"
  return { line: s, bytes: utf8(s), stub: true }
}

/** @param {any} card */
const cardIds = (card) => ({ id: card?.id, sessionID: card?.sessionID, permission: card?.permission, tool: card?.tool })

// -------------------------------------------------------------------------------------------------------- filter

/** @param {any} pt */
const isUserText = (pt) => pt.type === "text" && !("time" in pt)

/**
 * What is forwarded of each bus event: only state transitions. Its maps lose each entry at that entry's terminal
 * (user messages excepted: OpenCode re-emits their infos unchanged, so one entry per prompt is kept), and at the
 * removal of its part, its message or its session.
 */
export class Filter {
  constructor() {
    /** @typedef {{ sid: string, mid: string }} Owner */
    /** @type {Map<string, Owner>} part id -> owner, for a tool part whose `running` was sent */
    this.toolSent = new Map()
    /** @type {Map<string, Owner>} part id -> owner, for a text/reasoning part whose start marker was sent */
    this.started = new Map()
    /** @type {Map<string, { sig: string, sid: string }>} */
    this.messageSent = new Map()
  }

  /**
   * @param {string} type
   * @param {any} p
   * @returns {any} the properties to forward, or null
   */
  apply(type, p) {
    if (!SUBSCRIBED_SET.has(type)) return null
    if (type === "message.updated") {
      const i = p.info
      /** @type {Record<string, unknown>} */
      const keep = {}
      for (const k of MESSAGE_KEYS) if (k in i) keep[k] = i[k]
      if (typeof i.summary === "boolean") keep.summary = i.summary
      const completed = Boolean(i.time?.completed)
      const sig = JSON.stringify([i.role, i.parentID ?? null, keep.summary ?? null, completed, i.error?.name ?? null])
      if (this.messageSent.get(i.id)?.sig === sig) return null
      if (completed || i.role === "user") {
        this.messageSent.delete(i.id)
        if (i.role === "user") this.messageSent.set(i.id, { sig, sid: i.sessionID })
      } else this.messageSent.set(i.id, { sig, sid: i.sessionID })
      return { sessionID: p.sessionID ?? null, info: keep }
    }
    if (type === "message.part.updated") {
      let pt = p.part
      const ty = pt.type
      const owner = { sid: pt.sessionID, mid: pt.messageID }
      if (ty === "step-start") return null
      if (ty === "text" && (pt.synthetic || pt.ignored)) return null
      if ((ty === "text" || ty === "reasoning") && !isUserText(pt) && (pt.time ?? {}).end == null) {
        if (this.started.has(pt.id)) return null
        this.started.set(pt.id, owner)
        return { sessionID: p.sessionID ?? null, part: { ...pt, text: "" } }
      }
      if (ty === "text" || ty === "reasoning") this.started.delete(pt.id)
      if (ty === "tool") {
        const status = pt.state.status
        if (status === "pending") return null
        if (status === "running") {
          if (this.toolSent.has(pt.id)) return null
          this.toolSent.set(pt.id, owner)
          const { metadata, ...state } = pt.state
          pt = { ...pt, state }
        } else this.toolSent.delete(pt.id)
      }
      return { sessionID: p.sessionID ?? null, part: pt }
    }
    if (type === "message.part.removed") {
      this.toolSent.delete(p.partID)
      this.started.delete(p.partID)
    } else if (type === "message.removed") this.forget((o) => o.mid === p.messageID, p.messageID)
    else if (type === "session.deleted") {
      const sid = p.info?.id ?? p.sessionID
      this.forget((o) => o.sid === sid)
    }
    return p
  }

  /**
   * @param {(o: { sid: string, mid?: string }) => boolean} gone
   * @param {string} [messageID]
   */
  forget(gone, messageID) {
    for (const m of [this.toolSent, this.started]) for (const [k, o] of m) if (gone(o)) m.delete(k)
    if (messageID !== undefined) this.messageSent.delete(messageID)
    else for (const [k, o] of this.messageSent) if (gone(o)) this.messageSent.delete(k)
  }
}

// ------------------------------------------------------------------------------------------------ epoch / liveness

/**
 * Per-activation record of requests asked and of the events that end a request's epoch (abort, idle, retry,
 * delete of its session or an ancestor; instance disposal). Created inside `tui()`, so a re-activation starts
 * empty and judges every older request dead. A request whose epoch ended by what this log knows leaves `asked`
 * with its reason; events older than the oldest open request are pruned, a repeat of an event no open request
 * sits between is not kept, and the list never exceeds EPOCH_EVENTS_CAP.
 */
export class EpochLog {
  constructor() {
    /** @type {Map<string, { seq: number, sid: string }>} */
    this.asked = new Map()
    /** @type {Map<string, { why: string, sid: string }>} request id -> why its epoch ended, newest ENDED_CAP kept */
    this.endedIds = new Map()
    /** @type {EpochEvent[]} */
    this.events = []
    /** @type {Map<string, string | undefined>} */
    this.parent = new Map()
  }

  /**
   * @param {number} seq
   * @param {string} type
   * @param {any} p
   */
  note(seq, type, p) {
    switch (type) {
      case "permission.asked":
      case "question.asked":
        if (typeof p.id === "string") {
          this.endedIds.delete(p.id)
          this.asked.set(p.id, { seq, sid: p.sessionID })
        }
        return
      case "permission.replied":
      case "question.replied":
      case "question.rejected":
        this.asked.delete(p.requestID)
        return this.prune()
      case "session.created":
        if (p.info?.id) this.parent.set(p.info.id, p.info.parentID)
        return
      case "session.status":
        if (p.status?.type === "idle" || p.status?.type === "retry") this.push({ seq, type: p.status.type, sid: p.sessionID })
        return
      case "session.idle":
        return this.push({ seq, type: "idle", sid: p.sessionID })
      case "session.error":
        if (p.error?.name === "MessageAbortedError") this.push({ seq, type: "abort", sid: p.sessionID })
        return
      case "session.deleted": {
        const sid = p.info?.id ?? p.sessionID
        this.push({ seq, type: "deleted", sid })
        this.parent.delete(sid)
        return
      }
      case "server.instance.disposed":
        return this.push({ seq, type: "disposed" })
    }
  }

  /** @param {EpochEvent} e */
  push(e) {
    for (const [id, a] of this.asked) {
      const why = match(e, a.sid, this.ancestors(a.sid))
      if (why) this.end(id, a.sid, why)
    }
    if (!this.asked.size) return void (this.events.length = 0)
    let newest = 0
    for (const a of this.asked.values()) newest = Math.max(newest, a.seq)
    if (this.events.some((x) => x.type === e.type && x.sid === e.sid && x.seq >= newest)) return this.prune()
    this.events.push(e)
    while (this.events.length > EPOCH_EVENTS_CAP) {
      const dropped = /** @type {EpochEvent} */ (this.events.shift())
      for (const [id, a] of this.asked) if (a.seq <= dropped.seq) this.end(id, a.sid, this.ended(id, a.sid, []) ?? "epoch-evicted")
    }
    this.prune()
  }

  /**
   * @param {string} id
   * @param {string} sid
   * @param {string} why
   */
  end(id, sid, why) {
    this.asked.delete(id)
    this.endedIds.set(id, { why, sid })
    if (this.endedIds.size > ENDED_CAP) this.endedIds.delete(/** @type {string} */ (this.endedIds.keys().next().value))
  }

  prune() {
    let oldest = Infinity
    for (const a of this.asked.values()) oldest = Math.min(oldest, a.seq)
    if (oldest === Infinity) this.events.length = 0
    else if (this.events.length && this.events[0].seq < oldest) this.events = this.events.filter((e) => e.seq >= oldest)
  }

  /** @param {string} sid */
  ancestors(sid) {
    const chain = []
    let p = this.parent.get(sid)
    while (p && chain.length < 8) {
      chain.push(p)
      p = this.parent.get(p)
    }
    return chain
  }

  /**
   * Why the request's epoch has ended, or null while it has not. "asked-before-activation" when this activation
   * never saw it asked; "epoch-evicted" when the events after its ask no longer fit EPOCH_EVENTS_CAP.
   * @param {string} id
   * @param {string} sid
   * @param {string[]} ancestors
   * @returns {string | null}
   */
  ended(id, sid, ancestors) {
    const a = this.asked.get(id)
    if (!a) return this.endedIds.get(id)?.why ?? "asked-before-activation"
    for (const e of this.events) {
      if (e.seq < a.seq) continue
      const why = match(e, sid, ancestors)
      if (why) return why
    }
    return null
  }
}

/**
 * @param {EpochEvent} e
 * @param {string} sid
 * @param {string[]} ancestors
 */
function match(e, sid, ancestors) {
  if (e.type === "disposed") return "epoch:disposed"
  if (e.sid === sid) return "epoch:" + e.type
  if (e.sid && ancestors.includes(e.sid)) return "epoch:" + e.type + ":ancestor"
  return null
}

/**
 * The liveness rule over gathered facts: PRESENT (listed, or in the TUI store when the list call failed), SERVER (the
 * session exists and is busy or retrying), ANCHOR (its own tool part is running in an unfinished, error-free
 * message; a doom-loop request: a matching running part in the newest assistant message; any other request
 * without a tool is never live), EPOCH (see EpochLog). The first failing condition names the dead reason.
 * @param {{ listed: boolean | "unknown", inStore: boolean, sessionExists: boolean, busy: boolean, anchor: boolean, epoch: string | null }} f
 * @returns {Verdict}
 */
export function verdict(f) {
  const present = f.listed === true || (f.listed === "unknown" && f.inStore)
  const dead = !present ? "absent" : !f.sessionExists ? "session-missing" : !f.busy ? "not-busy" : !f.anchor ? "anchor" : f.epoch
  return { live: dead === null, dead, listed: f.listed, in_store: f.inStore }
}

// ------------------------------------------------------------------------------------------------------ backoff

/**
 * Reconnect delay: 0.5 s doubling to 8 s, ±20% jitter. The count starts over once a link has settled a snapshot
 * or stayed up STABLE_MS, so a daemon that welcomes and drops at once is not redialed at the minimum rate.
 * @param {number} attempt 0 for the first retry
 * @param {number} [rand] in [0, 1)
 */
export function backoffDelay(attempt, rand = Math.random()) {
  const base = Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** Math.min(attempt, 16))
  return Math.round(base * (0.8 + 0.4 * rand))
}

// --------------------------------------------------------------------------------------------------- API check

/**
 * Every member this plugin uses, checked at load. A missing member is named in the hello and leaves the link
 * observe-only.
 * @param {any} api
 * @returns {{ ok: boolean, missing: string[], version: string | null }}
 */
export function checkApi(api) {
  /** @type {string[]} */
  const missing = []
  /** @param {string} path @param {"function" | "object" | "string" | "getter"} kind */
  const need = (path, kind) => {
    try {
      let v = api
      for (const k of path.split(".").slice(1)) v = v?.[k]
      const ok = kind === "getter" ? v !== undefined : kind === "object" ? v !== null && typeof v === "object" : typeof v === kind
      if (!ok) missing.push(path)
    } catch {
      missing.push(path)
    }
  }
  need("api.event.on", "function")
  need("api.slots.register", "function")
  need("api.route.current", "getter")
  need("api.lifecycle.onDispose", "function")
  need("api.state.path.directory", "string")
  need("api.state.session.get", "function")
  need("api.state.session.permission", "function")
  need("api.state.session.question", "function")
  need("api.client.session.status", "function")
  need("api.client.session.list", "function")
  need("api.client.session.messages", "function")
  need("api.client.session.get", "function")
  need("api.client.session.message", "function")
  need("api.client.permission.list", "function")
  need("api.client.question.list", "function")
  need("api.app.version", "string")
  /** @type {string | null} */
  let version = null
  try {
    version = typeof api?.app?.version === "string" ? api.app.version : null
  } catch {}
  return { ok: missing.length === 0, missing, version }
}

// ------------------------------------------------------------------------------------------------------ plugin

let activations = 0

/** @param {unknown} v @returns {v is Start} */
const isStart = (v) =>
  !!v && typeof v === "object" && Number.isInteger(/** @type {any} */ (v).sec) && Number.isInteger(/** @type {any} */ (v).usec)

/**
 * `agent.json` next to this file: the agent's pid and birth time, written by CodeConnect after it started OpenCode.
 * Read off the main thread, opened non-blocking (a FIFO planted there cannot hold the read), and only when it is a
 * regular file of at most 4 KiB.
 * @param {string} [path]
 * @returns {Promise<Start | null>}
 */
export async function readStart(path = fileURLToPath(new URL("./agent.json", import.meta.url))) {
  /** @type {import("node:fs/promises").FileHandle | null} */
  let fh = null
  try {
    fh = await fsp.open(path, fsc.O_RDONLY | fsc.O_NONBLOCK)
    const st = await fh.stat()
    if (!st.isFile() || st.size > AGENT_FILE_CAP) return null
    const buf = Buffer.alloc(AGENT_FILE_CAP + 1)
    const { bytesRead } = await fh.read(buf, 0, buf.length, 0)
    if (bytesRead > AGENT_FILE_CAP) return null
    const v = JSON.parse(buf.subarray(0, bytesRead).toString("utf8"))
    return isStart(v?.start) ? { sec: v.start.sec, usec: v.start.usec } : null
  } catch {
    return null
  } finally {
    await fh?.close().catch(() => {})
  }
}

/**
 * The daemon's `acked` with every malformed entry dropped: a `ses_` key, `from` a string or null, `inclusive` a
 * boolean.
 * @param {unknown} v
 * @returns {Acked}
 */
export function validAcked(v) {
  /** @type {Acked} */
  const o = {}
  if (!v || typeof v !== "object" || Array.isArray(v)) return o
  for (const [k, b] of Object.entries(v)) {
    if (!k.startsWith("ses_") || !b || typeof b !== "object") continue
    const from = /** @type {any} */ (b).from
    const inclusive = /** @type {any} */ (b).inclusive
    if ((from === null || typeof from === "string") && typeof inclusive === "boolean") o[k] = { from, inclusive }
  }
  return o
}

/** @param {any} r */
const ok = (r) => !!r && !r.error && (r.response?.status ?? 200) < 400

/** @param {() => Promise<any>} f */
const call = async (f) => {
  try {
    const r = await f()
    return { ok: ok(r), data: r?.data, response: r?.response }
  } catch {
    return { ok: false, data: undefined, response: undefined }
  }
}

/**
 * The liveness verdict of one pending request, from the server's facts (session, anchoring tool part, ancestor
 * chain), the TUI store and the activation's epoch log.
 * @param {{ client: any, state: any, epoch: EpochLog, directory: string | undefined }} env
 * @param {"permission" | "question"} kind
 * @param {any} r
 * @param {boolean | "unknown"} listed
 * @param {Record<string, any>} status
 * @param {Map<string, string | null>} parents session -> parent, shared by the requests of one snapshot
 * @returns {Promise<Verdict>}
 */
export async function judge(env, kind, r, listed, status, parents) {
  const { client, directory: dir, epoch } = env
  const sid = r.sessionID
  let inStore = false
  try {
    inStore = (env.state.session[kind](sid) ?? []).some((/** @type {any} */ x) => x?.id === r.id)
  } catch {}
  const s = status[sid]
  const busy = !!s && s.type !== "idle"
  const got = await call(() => client.session.get({ sessionID: sid, directory: dir }))
  let anchor = false
  const tool = r.tool
  if (tool?.messageID) {
    const m = await call(() => client.session.message({ sessionID: sid, messageID: tool.messageID, directory: dir }))
    const parts = Array.isArray(m.data?.parts) ? m.data.parts : []
    const part = parts.find((/** @type {any} */ p) => p?.type === "tool" && p.callID === tool.callID)
    anchor = m.ok && part?.state?.status === "running" && !m.data?.info?.error && !m.data?.info?.time?.completed
  } else if (kind === "permission" && r.permission === "doom_loop") {
    const meta = r.metadata ?? {}
    const ms = await call(() => client.session.messages({ sessionID: sid, directory: dir, limit: 4 }))
    const last = (Array.isArray(ms.data) ? ms.data : []).filter((/** @type {any} */ x) => x?.info?.role === "assistant").at(-1)
    const want = JSON.stringify(meta.input)
    const running = (Array.isArray(last?.parts) ? last.parts : []).some(
      (/** @type {any} */ p) => p?.type === "tool" && p.tool === meta.tool && p.state?.status === "running" && JSON.stringify(p.state?.input) === want,
    )
    anchor = ms.ok && running && !last?.info?.error && !last?.info?.time?.completed
  }
  const chain = new Set(epoch.ancestors(sid))
  let cur = sid
  for (let i = 0; i < 8; i++) {
    let p = parents.get(cur)
    if (p === undefined) {
      const g = cur === sid ? got : await call(() => client.session.get({ sessionID: cur, directory: dir }))
      if (!g.ok) break
      const parent = typeof g.data?.parentID === "string" ? g.data.parentID : null
      parents.set(cur, parent)
      p = parent
    }
    if (!p) break
    chain.add(p)
    cur = p
  }
  return verdict({ listed, inStore, sessionExists: got.ok, busy, anchor, epoch: epoch.ended(r.id, sid, [...chain]) })
}

const yieldToLoop = () => new Promise((r) => setImmediate(r))

/**
 * @param {Api} api
 * @param {any} options
 */
async function tui(api, options) {
  const socketPath = options?.socket
  const nonce = options?.nonce
  if (typeof socketPath !== "string" || typeof nonce !== "string") return
  activate(api, socketPath, nonce)
}

/**
 * @param {Api} api
 * @param {string} socketPath
 * @param {string} nonce
 */
function activate(api, socketPath, nonce) {
  const activation = ++activations
  const T0 = Date.now()
  const check = checkApi(api)
  /** @type {any} */
  const client = api.client
  const directory = () => {
    try {
      return api.state.path.directory
    } catch {
      return undefined
    }
  }

  const filter = new Filter()
  const epoch = new EpochLog()
  /** @type {Map<string, { input: any, pid: string, mid: string, sid: string }>} callID -> the running tool part's input */
  const inputs = new Map()
  /** @type {Map<string, string>} root session -> first user message seen in this activation */
  const firstPrompt = new Map()
  let seq = 0
  let syncs = 0

  // link state
  /** @type {Socket | null} */
  let sock = null
  /** @type {"down" | "hello" | "live"} */
  let state = "down"
  let syncing = false
  /** @type {{ line: string, bytes: number, seq: number }[]} */
  let held = []
  let heldBytes = 0
  /** @type {Trigger | null} */
  let pending = null
  /** @type {Acked} */
  let acked = {}
  let attempt = 0
  let stopped = false
  let disposed = false
  let dialing = false
  let lastFull = -Infinity
  /** @type {ReturnType<typeof setTimeout> | null} */
  let retryTimer = null
  /** @type {ReturnType<typeof setTimeout> | null} */
  let stableTimer = null
  /** @type {Head | null} */
  let head = null
  let headSent = ""
  /** @type {(() => void) | null} */
  let disposeRoot = null
  /** @type {(() => void)[]} */
  const unsubscribe = []

  const rootOf = (/** @type {string} */ s) => {
    let x = s
    for (let k = 0; k < 50 && epoch.parent.get(x); k++) x = /** @type {string} */ (epoch.parent.get(x))
    return x
  }

  // ---- listeners: synchronous and bounded; nothing here waits on I/O
  /**
   * @param {string} type
   * @param {any} p
   */
  function track(type, p) {
    epoch.note(seq, type, p)
    if (type === "message.part.updated" && p.part?.type === "tool" && p.part.callID) {
      const pt = p.part
      const s = pt.state?.status
      if (s === "pending" || s === "running") inputs.set(pt.callID, { input: pt.state?.input, pid: pt.id, mid: pt.messageID, sid: pt.sessionID })
      else inputs.delete(pt.callID)
    }
    if (type === "message.part.removed") for (const [k, v] of inputs) v.pid === p.partID && inputs.delete(k)
    if (type === "message.removed") for (const [k, v] of inputs) v.mid === p.messageID && inputs.delete(k)
    if (type === "session.deleted") {
      const sid = p.info?.id ?? p.sessionID
      for (const [k, v] of inputs) v.sid === sid && inputs.delete(k)
      firstPrompt.delete(sid)
    }
    if (type === "message.updated") {
      const i = p.info
      if (i?.role === "user" && i.sessionID && !epoch.parent.get(i.sessionID) && !firstPrompt.has(i.sessionID) && (i.time?.created ?? 0) >= T0)
        firstPrompt.set(i.sessionID, i.id)
    }
  }

  /**
   * @param {string} type
   * @param {any} event
   */
  function forward(type, event) {
    const p = event?.properties ?? {}
    seq++
    track(type, p)
    if (REQUEST_TRIGGERS.has(type) || (type === "session.status" && p.status?.type === "retry")) trigger({ scope: "requests", reason: "trigger:" + type })
    const out = filter.apply(type, p)
    if (out === null || state !== "live") return
    if (type === "permission.asked" || type === "question.asked") enqueue(cardLine(seq, type, withAnchor(out)), seq, true)
    else enqueue(eventLine(seq, type, out), seq)
  }

  /** @param {any} request */
  const withAnchor = (request) => {
    const callID = request?.tool?.callID
    const known = callID ? inputs.get(callID) : undefined
    return known ? { ...request, anchor_input: known.input } : request
  }

  // ---- writes
  /**
   * A frame is written at once, or held while a snapshot is being sent. A card is never held: the daemon keys it by
   * request id, so it may arrive ahead of the snapshot.
   * @param {Encoded} e
   * @param {number} s
   * @param {boolean} [card]
   */
  function enqueue(e, s, card = false) {
    const c = sock
    if (!c || state !== "live") return
    if (syncing && !card) {
      if (c.writableLength + heldBytes + e.bytes > QUEUE_CAP) return overflow(c)
      held.push({ line: e.line, bytes: e.bytes, seq: s })
      heldBytes += e.bytes
      return
    }
    if (c.writableLength + heldBytes + e.bytes > QUEUE_CAP) return overflow(c)
    c.write(e.line)
  }

  /** @param {Socket} c */
  function overflow(c) {
    if (sock !== c) return
    c.destroy()
  }

  /**
   * @param {Socket} c
   * @param {unknown} frame
   */
  const send = (c, frame) => {
    const line = JSON.stringify(frame) + "\n"
    c.write(line)
    return utf8(line)
  }

  /** @param {Socket} c */
  const drained = (c) =>
    new Promise((resolve) => {
      if (c.writableLength === 0 || c.destroyed) return resolve(undefined)
      const done = () => {
        c.off("drain", done)
        c.off("close", done)
        resolve(undefined)
      }
      c.on("drain", done)
      c.on("close", done)
    })

  // ---- head
  /** @param {Head} h */
  function setHead(h) {
    head = h
    sendHead()
  }

  function sendHead() {
    if (!head || state !== "live") return
    const key = JSON.stringify(head)
    if (key === headSent) return
    headSent = key
    seq++
    const line = JSON.stringify({ t: "head", seq, ...head }) + "\n"
    enqueue({ line, bytes: utf8(line), stub: false }, seq)
  }

  /** @returns {Head} */
  function readHead() {
    /** @type {any} */
    const r = api.route.current
    if (r?.name === "session" && typeof r.params?.sessionID === "string") {
      const id = r.params.sessionID
      const dir = api.state.session.get(id)?.directory
      return { route: "session", session_id: id, directory: typeof dir === "string" ? dir : null }
    }
    return { route: r?.name === "home" ? "home" : "other" }
  }

  /** @param {any} solid */
  function registerHead(solid) {
    let started = false
    api.slots.register({
      order: 0,
      slots: {
        app() {
          try {
            if (started || disposed) return null
            started = true
            solid.createRoot((/** @type {() => void} */ dispose) => {
              disposeRoot = dispose
              solid.createEffect(() => {
                try {
                  setHead(readHead())
                } catch {}
              })
            })
          } catch {}
          return null
        },
      },
    })
  }

  // ---- sync
  /** @param {Trigger} t */
  function trigger(t) {
    if (state !== "live") return
    if (syncing) {
      if (!pending || (pending.scope === "requests" && t.scope === "full")) pending = t
      return
    }
    if (pending) {
      if (pending.scope === "requests" && t.scope === "full") pending = t
      return
    }
    pending = t
    schedule()
  }

  /** Runs the pending snapshot on a later tick; a full one no sooner than FULL_SYNC_GAP_MS after the last full one. */
  function schedule() {
    const wait = pending?.scope === "full" ? Math.max(0, lastFull + FULL_SYNC_GAP_MS - Date.now()) : 0
    setTimeout(runPending, wait)
  }

  function runPending() {
    const c = sock
    const t = pending
    if (!t || !c || state !== "live" || syncing) return
    if (t.scope === "full" && Date.now() < lastFull + FULL_SYNC_GAP_MS) return schedule()
    pending = null
    if (t.scope === "full") lastFull = Date.now()
    void runSync(c, t).catch(() => {
      if (sock === c) c.destroy()
    })
  }

  /**
   * @param {Socket} c
   * @param {Trigger} t
   */
  async function runSync(c, t) {
    const sync = ++syncs
    const asOf = seq
    syncing = true
    held = []
    heldBytes = 0
    const alive = () => sock === c && state === "live" && !disposed
    const doneAt = t.scope === "full" ? await fullSync(c, sync, t.reason, asOf, alive) : await requestsSync(c, sync, t.reason, asOf, alive)
    if (doneAt === null || !alive()) return
    let settled = false
    for (const h of held) {
      if (!settled && h.seq > doneAt) {
        send(c, { t: "settled", sync })
        settled = true
      }
      c.write(h.line)
    }
    if (!settled) send(c, { t: "settled", sync })
    attempt = 0
    held = []
    heldBytes = 0
    syncing = false
    if (pending) schedule()
  }

  /**
   * A requests-only snapshot: the status, then every pending request with its verdict.
   * @param {Socket} c
   * @param {number} sync
   * @param {string} reason
   * @param {number} asOf
   * @param {() => boolean} alive
   * @returns {Promise<number | null>} done_at, or null when the link went away
   */
  async function requestsSync(c, sync, reason, asOf, alive) {
    const reads = await readRequests()
    if (!alive()) return null
    send(c, { t: "sync_begin", sync, reason, scope: "requests", as_of: asOf, status: reads.status.ok ? reads.status.data : {}, status_ok: reads.status.ok })
    const n = await sendRequests(c, sync, reads, alive)
    if (n === null) return null
    const doneAt = seq
    send(c, {
      t: "sync_end",
      sync,
      done_at: doneAt,
      permissions_ok: reads.permissions.ok,
      questions_ok: reads.questions.ok,
      requests_ok: reads.permissions.ok && reads.questions.ok,
      lower: {},
      pages: 0,
      items: 0,
      bytes: n,
      activation,
    })
    return doneAt
  }

  const readRequests = async () => {
    const dir = directory()
    const [status, permissions, questions] = await Promise.all([
      call(() => client.session.status({ directory: dir })),
      call(() => client.permission.list({ directory: dir })),
      call(() => client.question.list({ directory: dir })),
    ])
    for (const r of [permissions, questions]) if (r.ok && !Array.isArray(r.data)) r.ok = false
    if (status.ok && !isRecord(status.data)) status.ok = false
    return { status, permissions, questions }
  }

  /**
   * @param {Socket} c
   * @param {number} sync
   * @param {Awaited<ReturnType<typeof readRequests>>} reads
   * @param {() => boolean} alive
   * @returns {Promise<number | null>} bytes sent
   */
  async function sendRequests(c, sync, reads, alive) {
    /** @type {Record<string, any>} */
    const status = reads.status.ok ? reads.status.data : {}
    /** @type {{ kind: "permission" | "question", request: any, listed: boolean | "unknown" }[]} */
    const requests = []
    const valid = (/** @type {any} */ r) => !!r && typeof r === "object" && typeof r.id === "string" && typeof r.sessionID === "string"
    const fromStore = (/** @type {"permission" | "question"} */ kind) => {
      const seen = new Set()
      const sids = new Set([
        ...epoch.parent.keys(),
        ...[...epoch.asked.values(), ...epoch.endedIds.values()].map((a) => a.sid),
        ...Object.keys(status),
        ...(head?.session_id ? [head.session_id] : []),
      ])
      for (const s of sids) {
        try {
          for (const r of api.state.session[kind](s) ?? []) if (valid(r) && !seen.has(r.id)) (seen.add(r.id), requests.push({ kind, request: r, listed: "unknown" }))
        } catch {}
      }
    }
    if (reads.permissions.ok) for (const r of reads.permissions.data) valid(r) && requests.push({ kind: "permission", request: r, listed: true })
    else fromStore("permission")
    if (reads.questions.ok) for (const r of reads.questions.data) valid(r) && requests.push({ kind: "question", request: r, listed: true })
    else fromStore("question")
    let bytes = 0
    const parents = new Map()
    const env = { client, state: api.state, epoch, directory: directory() }
    for (const x of requests) {
      const v = await judge(env, x.kind, x.request, x.listed, status, parents)
      if (!alive()) return null
      const request = withAnchor(x.request)
      const json = JSON.stringify({ t: "sync_request", sync, kind: x.kind, request: prepare(request, CARD_FIELDS), verdict: v })
      const nb = utf8(json) + 1
      const line =
        nb > FRAME_CAP
          ? JSON.stringify({ t: "sync_request", sync, kind: x.kind, stub: { properties: cardIds(request), size: nb - 1, sha256: sha256(json) }, verdict: v }) + "\n"
          : json + "\n"
      bytes += utf8(line)
      c.write(line)
      if (c.writableLength > 0) await drained(c)
      if (!alive()) return null
    }
    return bytes
  }

  /**
   * A full snapshot: the status, the pending requests, then the history pages of every root in the window (each
   * root's messages newest first, as the server pages them), then its children created inside the window.
   * @param {Socket} c
   * @param {number} sync
   * @param {string} reason
   * @param {number} asOf
   * @param {() => boolean} alive
   * @returns {Promise<number | null>}
   */
  async function fullSync(c, sync, reason, asOf, alive) {
    const dir = directory()
    const status = await call(() => client.session.status({ directory: dir }))
    if (status.ok && !isRecord(status.data)) status.ok = false
    if (!alive()) return null
    /** @type {Record<string, any>} */
    const statusData = status.ok ? status.data : {}
    send(c, { t: "sync_begin", sync, reason, scope: "full", as_of: asOf, status: statusData, status_ok: status.ok })
    const list = await call(() => client.session.list({ directory: dir }))
    if (!alive()) return null
    const listOk = list.ok && Array.isArray(list.data)
    /** @type {any[]} */
    const infos = listOk ? list.data.filter((/** @type {any} */ x) => !!x && typeof x === "object" && typeof x.id === "string") : []
    for (const x of infos) if (typeof x.parentID === "string" && !epoch.parent.has(x.id)) epoch.parent.set(x.id, x.parentID)

    const reads = await readRequests()
    if (!alive()) return null
    const n = await sendRequests(c, sync, reads, alive)
    if (n === null) return null

    const busy = new Set(Object.entries(statusData).filter(([, v]) => v?.type !== "idle").map(([k]) => k))
    const roots = infos.filter((x) => !x.parentID && (x.id in acked || busy.has(x.id) || firstPrompt.has(x.id)))

    /** @type {Record<string, Bound>} */
    const lower = {}
    /** @type {Record<string, "bound" | "start" | "last-user">} */
    const mode = {}
    for (const r of roots) {
      if (r.id in acked) {
        lower[r.id] = { from: acked[r.id].from, inclusive: acked[r.id].inclusive }
        mode[r.id] = acked[r.id].from === null ? "start" : "bound"
      } else if ((r.time?.created ?? 0) >= T0) {
        lower[r.id] = { from: null, inclusive: false }
        mode[r.id] = "start"
      } else if (firstPrompt.has(r.id)) {
        lower[r.id] = { from: /** @type {string} */ (firstPrompt.get(r.id)), inclusive: true }
        mode[r.id] = "bound"
      } else {
        lower[r.id] = { from: null, inclusive: false }
        mode[r.id] = "last-user"
      }
    }

    const ENVELOPE = encodedLength({ t: "sync_page", sync, n: 999999, items: [] }) + 1
    /** @type {any[]} */
    let page = []
    let pageBytes = 0
    let pages = 0
    let items = 0
    let bytes = 0
    const flush = async () => {
      if (!page.length) return
      const line = JSON.stringify({ t: "sync_page", sync, n: pages, items: page }) + "\n"
      page = []
      pageBytes = 0
      pages++
      bytes += utf8(line)
      c.write(line)
      if (c.writableLength > 0) await drained(c)
    }
    /**
     * One page item; `size` is its encoded length when the caller already knows it. An item that cannot fit a page
     * becomes a stub whose size and sha256 cover the item's encoded JSON.
     * @param {any} item
     * @param {number} [size]
     */
    const add = async (item, size = encodedLength(item)) => {
      let nb = size + 1
      if (ENVELOPE + nb > PAGE_CAP) {
        const s = JSON.stringify(item)
        const kind = item.part ? "part" : item.session ? "session" : "message"
        const keyed = item.part ? { sessionID: item.sessionID, part: item.part } : item.session ? { sessionID: item.session.id, parentID: item.session.parentID } : item
        item = { stub: true, kind, ids: ids(keyed), ...(item.part ? partShape(item.part) : {}), size: utf8(s), sha256: sha256(s) }
        nb = encodedLength(item) + 1
      }
      if (ENVELOPE + pageBytes + nb > PAGE_CAP) await flush()
      if (!alive()) return
      page.push(item)
      pageBytes += nb
      items++
    }
    /**
     * @param {string} root
     * @param {string} id
     * @param {string} role
     * @returns {"keep" | "last" | "stop"}
     */
    const keep = (root, id, role) => {
      const m = mode[root]
      if (m === "start") return "keep"
      if (m === "last-user") return role === "user" ? "last" : "keep"
      const b = lower[root]
      const from = /** @type {string} */ (b.from)
      return (b.inclusive ? id >= from : id > from) ? "keep" : "stop"
    }
    /**
     * @param {string} sid
     * @param {string} root
     * @param {boolean} all
     */
    const window = async (sid, root, all) => {
      /** @type {string | undefined} */
      let before
      let oldest = Infinity
      for (let n = 0; n < 100000 && alive(); n++) {
        const r = await call(() => client.session.messages({ sessionID: sid, directory: dir, limit: API_PAGE_LIMIT, ...(before ? { before } : {}) }))
        if (!alive()) return oldest
        if (!r.ok || !Array.isArray(r.data)) {
          await add({ error: true, sessionID: sid })
          return oldest
        }
        /** @type {any[]} */
        const msgs = r.data
        let reached = false
        for (let i = msgs.length - 1; i >= 0 && !reached; i--) {
          const m = msgs[i]
          const id = m?.info?.id
          if (typeof id !== "string") continue
          const k = all ? "keep" : keep(root, id, m.info.role)
          if (k === "stop") {
            reached = true
            continue
          }
          if (k === "last") {
            reached = true
            lower[root] = { from: id, inclusive: true }
          }
          const created = m.info.time?.created
          if (typeof created === "number") oldest = Math.min(oldest, created)
          await message(sid, m)
          if (!alive()) return oldest
          await yieldToLoop()
          if (!alive()) return oldest
        }
        const cursor = r.response?.headers?.get?.("x-next-cursor")
        if (reached || typeof cursor !== "string" || !cursor || !msgs.length) break
        before = cursor
      }
      return oldest
    }
    /**
     * A message as one item, or split across items in part order when it does not fit a page. Every part is
     * encoded once; the item sizes are sums of those lengths.
     * @param {string} sid
     * @param {any} m
     */
    const message = async (sid, m) => {
      const info = prepare(m.info)
      const parts = (Array.isArray(m.parts) ? m.parts : []).filter((/** @type {any} */ p) => !!p && typeof p === "object").map((/** @type {any} */ p) => prepare(p))
      const sizes = parts.map((/** @type {any} */ p) => encodedLength(p))
      // `[a,b]` encodes to the elements' lengths plus a comma between each pair.
      const span = (/** @type {number} */ from, /** @type {number} */ to) => {
        let n = Math.max(0, to - from - 1)
        for (let i = from; i < to; i++) n += sizes[i]
        return n
      }
      const wholeBase = encodedLength({ sessionID: sid, info, parts: [] })
      if (ENVELOPE + wholeBase + span(0, parts.length) + 1 <= PAGE_CAP) return add({ sessionID: sid, info, parts }, wholeBase + span(0, parts.length))
      const splitBase = encodedLength({ sessionID: sid, info, parts: [], split: true })
      const oneBase = encodedLength({ sessionID: sid, info, part: 0, split: true }) - 1
      const fits = (/** @type {number} */ size) => ENVELOPE + size + 1 <= PAGE_CAP
      let from = 0
      for (let i = 0; i < parts.length; i++) {
        if (i > from && !fits(splitBase + span(from, i + 1))) {
          await add({ sessionID: sid, info, parts: parts.slice(from, i), split: true }, splitBase + span(from, i))
          from = i
        }
        if (i === from && !fits(splitBase + sizes[i])) {
          await add({ sessionID: sid, info, part: parts[i], split: true }, oneBase + sizes[i])
          from = i + 1
        }
      }
      if (from < parts.length) await add({ sessionID: sid, info, parts: parts.slice(from), split: true }, splitBase + span(from, parts.length))
    }

    const want = new Set(roots.map((r) => r.id))
    for (const r of roots) await add({ session: prepare(r) })
    /** @type {Record<string, number>} */
    const start = {}
    for (const r of roots) {
      start[r.id] = await window(r.id, r.id, false)
      if (!alive()) return null
    }
    for (const k of infos) {
      if (typeof k.parentID !== "string" || !want.has(rootOf(k.id))) continue
      if ((k.time?.created ?? 0) < (start[rootOf(k.id)] ?? Infinity)) continue
      await add({ session: prepare(k) })
      await window(k.id, rootOf(k.id), true)
      if (!alive()) return null
    }
    await flush()
    if (!alive()) return null
    const doneAt = seq
    send(c, {
      t: "sync_end",
      sync,
      done_at: doneAt,
      list_ok: listOk,
      permissions_ok: reads.permissions.ok,
      questions_ok: reads.questions.ok,
      requests_ok: reads.permissions.ok && reads.questions.ok,
      lower,
      pages,
      items,
      bytes: bytes + n,
      activation,
    })
    return doneAt
  }

  // ---- connection
  async function connect() {
    retryTimer = null
    if (disposed || stopped || dialing) return
    dialing = true
    const start = await readStart()
    dialing = false
    if (disposed || stopped || sock) return
    if (!start) return retry()
    /** @type {Socket} */
    let c
    try {
      c = createConnection({ path: socketPath })
    } catch {
      return retry()
    }
    sock = c
    state = "hello"
    let buffer = ""
    const timer = setTimeout(() => {
      if (sock === c && state === "hello") c.destroy()
    }, HELLO_TIMEOUT_MS)
    c.on("connect", () => {
      if (sock !== c) return
      send(c, {
        type: "opencode_hello",
        wire: WIRE,
        nonce,
        pid: process.pid,
        start,
        activation,
        directory: directory() ?? null,
        api: { ok: check.ok, missing: check.missing, version: check.version },
      })
    })
    c.on("data", (d) => {
      if (sock !== c) return
      buffer += d.toString("utf8")
      if (buffer.length > READ_CAP) return void c.destroy()
      let i
      while ((i = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, i)
        buffer = buffer.slice(i + 1)
        try {
          receive(c, JSON.parse(line))
        } catch {}
        if (sock !== c) return
      }
    })
    c.on("error", () => {})
    c.on("end", () => c.destroy())
    c.on("close", () => {
      clearTimeout(timer)
      if (stableTimer) clearTimeout(stableTimer)
      stableTimer = null
      if (sock !== c) return
      sock = null
      state = "down"
      syncing = false
      held = []
      heldBytes = 0
      pending = null
      headSent = ""
      retry()
    })
  }

  function retry() {
    if (disposed || stopped || retryTimer) return
    retryTimer = setTimeout(() => void connect(), backoffDelay(attempt++))
  }

  /**
   * @param {Socket} c
   * @param {any} m
   */
  function receive(c, m) {
    if (state === "hello") {
      if (m?.type === "opencode_welcome") {
        acked = validAcked(m.acked)
        state = "live"
        stableTimer = setTimeout(() => {
          if (sock === c && state === "live") attempt = 0
        }, STABLE_MS)
        sendHead()
        trigger({ scope: "full", reason: "connect" })
      } else if (m?.type === "opencode_refused") {
        if (m.final === true) stopped = true
        c.destroy()
      }
      return
    }
    if (m?.type === "opencode_resync") {
      if (m.acked !== undefined) acked = validAcked(m.acked)
      trigger({ scope: "full", reason: "requested" })
    }
  }

  // ---- registration
  for (const type of SUBSCRIBED) {
    try {
      const off = api.event.on(/** @type {any} */ (type), (/** @type {any} */ event) => {
        try {
          forward(type, event)
        } catch {}
      })
      if (typeof off === "function") unsubscribe.push(off)
    } catch {}
  }
  try {
    api.lifecycle.onDispose(() => {
      disposed = true
      if (retryTimer) clearTimeout(retryTimer)
      if (stableTimer) clearTimeout(stableTimer)
      retryTimer = stableTimer = null
      for (const off of unsubscribe.splice(0)) {
        try {
          off()
        } catch {}
      }
      try {
        disposeRoot?.()
      } catch {}
      sock?.destroy()
    })
  } catch {}

  setTimeout(() => {
    if (disposed) return
    import("solid-js")
      .then((solid) => {
        try {
          if (!disposed) registerHead(solid)
        } catch {}
      })
      .catch(() => {
        check.ok = false
        check.missing.push("solid-js")
      })
      .finally(() => {
        if (!disposed && !sock && !retryTimer) void connect()
      })
  }, 0)
}

/** @type {TuiPluginModule & { id: string }} */
export default { id: "codeconnect", tui }
