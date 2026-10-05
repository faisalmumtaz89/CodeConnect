// OpenCode hands its plugins the client build of solid-js; Bun would resolve the server build, whose effects never run.
import { mock } from "bun:test"
import * as solid from "solid-js/dist/solid.js"

mock.module("solid-js", () => solid)
