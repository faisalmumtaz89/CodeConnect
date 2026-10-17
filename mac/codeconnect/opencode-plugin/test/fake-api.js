// A fake TuiPluginApi: an event bus, a route signal, a TUI store, slots, lifecycle, a dialog flag, the Permission
// context in the app slot's owner chain, and a client backed by an in-memory server (sessions, messages paged
// newest-first with x-next-cursor, status, permission and question lists, the permission reply, tool ids, MCP
// status). The bus mirrors the TUI's own Sync, which runs before any plugin listener: a permission asked under auto
// mode is replied "once" and never stored, otherwise it enters the store; a reply removes it. Hooks let a test act
// in the middle of a client call.
import { createRoot, createSignal, getOwner } from "solid-js/dist/solid.js"

/**
 * @param {{ version?: string, directory?: string, mode?: "normal" | "auto" | null }} [opts] `mode: null` renders no
 * Permission context, so the plugin's mode stays unknown
 */
export function fakeApi(opts = {}) {
  const handlers = new Map()
  const [route, setRoute] = createSignal(/** @type {any} */ ({ name: "home" }))
  const [storeTick, setStoreTick] = createSignal(0)
  const server = {
    /** @type {any[]} */ sessions: [],
    /** @type {Map<string, any[]>} */ messages: new Map(),
    /** @type {Record<string, any>} */ status: {},
    /** @type {any[]} */ permissions: [],
    /** @type {any[]} */ questions: [],
    /** @type {string[]} */ toolIds: ["bash", "edit", "read", "question"],
    /** @type {Record<string, any>} */ mcp: {},
    /** methods answering 500 */ failing: /** @type {Set<string>} */ (new Set()),
    /** methods whose call throws */ throwing: /** @type {Set<string>} */ (new Set()),
    /** @type {{ method: string, args: any }[]} */ calls: [],
    /** runs after the delay and before the body; awaited when it returns a promise */
    before: /** @type {Record<string, (args: any) => void | Promise<void>>} */ ({}),
    delay: 0,
  }
  const store = { /** @type {Map<string, any>} */ sessions: new Map(), /** @type {any[]} */ permissions: [], /** @type {any[]} */ questions: [] }
  const slots = /** @type {any[]} */ ([])
  const disposers = /** @type {(() => void)[]} */ ([])
  let dialogOpen = false
  /** @type {"normal" | "auto"} */
  let mode = opts.mode ?? "normal"
  const permission =
    opts.mode === null
      ? null
      : {
          get mode() {
            return mode
          },
          set(/** @type {"normal" | "auto"} */ m) {
            mode = m
          },
          toggle() {
            mode = mode === "auto" ? "normal" : "auto"
          },
        }

  const respond = async (/** @type {string} */ method, /** @type {any} */ args, /** @type {() => any} */ body, headers = {}) => {
    server.calls.push({ method, args })
    await (server.delay ? new Promise((r) => setTimeout(r, server.delay)) : Promise.resolve())
    await server.before[method]?.(args)
    if (server.throwing.has(method)) throw new Error(method + " threw")
    if (server.failing.has(method)) return { error: { name: "Fail" }, response: new Response(null, { status: 500 }) }
    const data = body()
    if (data === undefined) return { error: { name: "NotFound" }, response: new Response(null, { status: 404 }) }
    return { data, response: new Response(null, { status: 200, headers }) }
  }

  /** @param {string} type @param {any} properties */
  const publish = (type, properties) => {
    for (const fn of handlers.get(type) ?? []) fn({ type, properties })
  }

  /** A bus event: Sync first, then every plugin listener. @param {string} type @param {any} properties */
  const emit = (type, properties) => {
    sync(type, properties)
    publish(type, properties)
  }

  const client = {
    session: {
      status: (/** @type {any} */ a) => respond("session.status", a, () => structuredClone(server.status)),
      list: (/** @type {any} */ a) => respond("session.list", a, () => structuredClone(server.sessions)),
      get: (/** @type {any} */ a) => respond("session.get", a, () => structuredClone(server.sessions.find((s) => s.id === a.sessionID))),
      message: (/** @type {any} */ a) =>
        respond("session.message", a, () => structuredClone((server.messages.get(a.sessionID) ?? []).find((m) => m.info.id === a.messageID))),
      messages: (/** @type {any} */ a) => {
        const all = server.messages.get(a.sessionID)
        if (!all) return respond("session.messages", a, () => undefined)
        const older = a.before ? all.filter((m) => m.info.id < a.before) : all
        if (!a.limit) return respond("session.messages", a, () => structuredClone(older))
        const pageItems = older.slice(Math.max(0, older.length - a.limit))
        const more = older.length > pageItems.length
        return respond("session.messages", a, () => structuredClone(pageItems), more ? { "x-next-cursor": pageItems[0].info.id } : {})
      },
    },
    permission: {
      list: (/** @type {any} */ a) => respond("permission.list", a, () => structuredClone(server.permissions)),
      // The server removes the request and publishes `replied` before answering; a second reply finds nothing (404).
      reply: (/** @type {any} */ a) =>
        respond("permission.reply", a, () => {
          const i = server.permissions.findIndex((p) => p.id === a.requestID)
          if (i < 0) return undefined
          const [p] = server.permissions.splice(i, 1)
          emit("permission.replied", { sessionID: p.sessionID, requestID: p.id, reply: a.reply })
          return true
        }),
    },
    question: { list: (/** @type {any} */ a) => respond("question.list", a, () => structuredClone(server.questions)) },
    tool: { ids: (/** @type {any} */ a) => respond("tool.ids", a, () => structuredClone(server.toolIds)) },
    mcp: { status: (/** @type {any} */ a) => respond("mcp.status", a, () => structuredClone(server.mcp)) },
  }

  const api = {
    app: { version: opts.version ?? "1.18.34" },
    event: {
      on(/** @type {string} */ type, /** @type {(e: any) => void} */ fn) {
        if (!handlers.has(type)) handlers.set(type, new Set())
        handlers.get(type).add(fn)
        return () => handlers.get(type).delete(fn)
      },
    },
    route: {
      get current() {
        return route()
      },
    },
    slots: {
      register(/** @type {any} */ plugin) {
        slots.push(plugin)
        render(plugin)
        return "slot-" + slots.length
      },
    },
    lifecycle: {
      onDispose(/** @type {() => void} */ fn) {
        disposers.push(fn)
        return () => {}
      },
    },
    ui: {
      dialog: {
        get open() {
          return dialogOpen
        },
      },
    },
    state: {
      path: { directory: opts.directory ?? "/Users/ada/project" },
      session: {
        get: (/** @type {string} */ id) => (storeTick(), store.sessions.get(id)),
        permission: (/** @type {string} */ id) => store.permissions.filter((p) => p.sessionID === id),
        question: (/** @type {string} */ id) => store.questions.filter((q) => q.sessionID === id),
      },
    },
    client,
  }

  /**
   * Renders the plugin's app slot under an owner whose context holds the Permission context, as the TUI's does.
   * The callback takes its dispose argument so Solid makes a fresh root rather than reusing its shared unowned one.
   */
  function render(/** @type {any} */ plugin) {
    createRoot((_dispose) => {
      const owner = /** @type {any} */ (getOwner())
      if (owner && permission) owner.context = { [Symbol("Permission")]: permission }
      plugin.slots?.app?.({}, {})
    })
  }

  /** What Sync does with a bus event before any plugin listener sees it. @param {string} type @param {any} p */
  function sync(type, p) {
    if (type === "permission.asked") {
      if (mode === "auto") {
        void client.permission.reply({ requestID: p.id, reply: "once", directory: api.state.path.directory })
        return false
      }
      if (!store.permissions.some((x) => x.id === p.id)) store.permissions.push(p)
    }
    if (type === "permission.replied") store.permissions = store.permissions.filter((x) => x.id !== p.requestID)
    return true
  }

  return {
    api,
    server,
    store,
    emit,
    /** @param {string} type @param {any} event */
    emitRaw(type, event) {
      for (const fn of handlers.get(type) ?? []) fn(event)
    },
    handlerCount: () => [...handlers.values()].reduce((n, s) => n + s.size, 0),
    types: () => [...handlers.keys()].filter((k) => handlers.get(k).size),
    navigate: (/** @type {any} */ r) => setRoute(r),
    /** @param {any} s */
    storeSession(s) {
      store.sessions.set(s.id, s)
      setStoreTick((n) => n + 1)
    },
    /** @param {"normal" | "auto"} m */
    setMode(m) {
      mode = m
    },
    /** @param {boolean} open */
    setDialog(open) {
      dialogOpen = open
    },
    renderSlots() {
      for (const p of slots) render(p)
    },
    slotCount: () => slots.length,
    dispose() {
      for (const fn of disposers.splice(0)) fn()
    },
  }
}
