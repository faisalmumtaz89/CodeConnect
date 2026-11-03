#!/usr/bin/env python3
"""A half-open link between the phone (simulator) and a scratch ccd, for B18/B19.

  halfopen_proxy.py <listen port> <ccd port> <log>

Forwards plain ws:// both ways. On each connection it reads the phone's frames (masked, as WebSocket clients
send them). After it forwards the first frame whose text contains "send_text" (or "compose"), it goes SILENT on that
connection: nothing more is forwarded in either direction, and both TCP connections stay open, as a link whose
network vanished does. That happens once: every later connection, and every later send, forwards normally. Each event is logged with a UTC time."""
import asyncio, datetime, sys

LISTEN, UPSTREAM, LOG = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
TRIGGERS = (b'"type":"send_text"', b'"type":"compose"')
conn_seq = 0
silenced_once = False


def log(msg):
    with open(LOG, "a") as f:
        f.write(f"{datetime.datetime.now(datetime.timezone.utc).strftime('%H:%M:%S.%f')[:-3]} {msg}\n")


def frames(buf):
    """Complete client frames in buf: (payload, consumed bytes). Payloads are unmasked."""
    out, i = [], 0
    while len(buf) - i >= 2:
        b1 = buf[i + 1]
        n, j = b1 & 0x7F, i + 2
        if n == 126:
            if len(buf) - j < 2: break
            n, j = int.from_bytes(buf[j:j + 2], "big"), j + 2
        elif n == 127:
            if len(buf) - j < 8: break
            n, j = int.from_bytes(buf[j:j + 8], "big"), j + 8
        masked = b1 & 0x80
        if masked:
            if len(buf) - j < 4: break
            key, j = buf[j:j + 4], j + 4
        if len(buf) - j < n: break
        payload = buf[j:j + n]
        if masked:
            payload = bytes(c ^ key[k % 4] for k, c in enumerate(payload))
        out.append(payload)
        i = j + n
    return out, i


async def handle(phone_r, phone_w):
    global conn_seq, silenced_once
    conn_seq += 1
    cid, silent = conn_seq, asyncio.Event()
    up_r, up_w = await asyncio.open_connection("127.0.0.1", UPSTREAM)
    log(f"conn {cid}: opened")

    async def phone_to_ccd():
        global silenced_once
        pending, handshake = b"", True
        while not silent.is_set():
            data = await phone_r.read(65536)
            if not data:
                log(f"conn {cid}: the phone closed"); break
            if handshake:  # the HTTP upgrade request goes through untouched
                up_w.write(data); await up_w.drain()
                if b"\r\n\r\n" in data: handshake = False
                continue
            pending += data
            parsed, used = frames(pending)
            pending = pending[used:]
            up_w.write(data); await up_w.drain()
            for p in parsed:
                if not silenced_once and any(t in p for t in TRIGGERS):
                    silenced_once = True
                    log(f"conn {cid}: forwarded the phone's send ({len(p)} bytes); going SILENT both ways")
                    silent.set()
        await silent.wait()

    async def ccd_to_phone():
        while not silent.is_set():
            data = await up_r.read(65536)
            if not data:
                log(f"conn {cid}: ccd closed"); break
            if silent.is_set():
                log(f"conn {cid}: dropped {len(data)} bytes from ccd (silent)"); continue
            phone_w.write(data); await phone_w.drain()
        while True:  # keep draining ccd so it never blocks, dropping everything
            data = await up_r.read(65536)
            if not data:
                log(f"conn {cid}: ccd closed while silent"); return
            log(f"conn {cid}: dropped {len(data)} bytes from ccd (silent)")

    async def phone_while_silent():
        await silent.wait()
        while True:
            data = await phone_r.read(65536)
            if not data:
                log(f"conn {cid}: the phone closed while silent"); return
            log(f"conn {cid}: swallowed {len(data)} bytes from the phone (silent)")

    await asyncio.gather(phone_to_ccd(), ccd_to_phone(), phone_while_silent(), return_exceptions=True)


async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", LISTEN)
    log(f"listening on 127.0.0.1:{LISTEN} -> 127.0.0.1:{UPSTREAM}")
    async with server:
        await server.serve_forever()

asyncio.run(main())
