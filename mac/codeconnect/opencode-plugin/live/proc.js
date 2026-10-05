// Process facts for the live tests, read with `ps` so the same code runs on macOS and Linux.
import { spawnSync } from "node:child_process"

/** @param {number} pid @param {string} field */
function ps(pid, field) {
  const r = spawnSync("ps", ["-o", `${field}=`, "-p", String(pid)], { encoding: "utf8", env: { ...process.env, LC_ALL: "C" } })
  return r.status === 0 ? r.stdout.trim() : ""
}

/**
 * A process's start time as `ps` reports it (whole seconds), in the hello's `{sec, usec}` shape. The live tests
 * write agent.json and admit the hello with this same function, so both sides agree; the daemon reads the kernel's
 * microseconds instead.
 * @param {number} pid
 */
export function birth(pid) {
  const ms = Date.parse(ps(pid, "lstart"))
  if (!Number.isFinite(ms)) throw new Error(`no start time for pid ${pid}`)
  return { sec: Math.floor(ms / 1000), usec: 0 }
}

/** True while the process exists and is not a zombie. @param {number} pid */
export function alive(pid) {
  const stat = ps(pid, "stat")
  return stat !== "" && !stat.startsWith("Z")
}
