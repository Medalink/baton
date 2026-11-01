#!/usr/bin/env python3
"""A raw baton.v1 client for e2e/sync.sh: it sends what a daemon never would.

    wire.py <case> <port> <token-file> <machine> [args...]

Each case talks to a real `baton serve` on 127.0.0.1 and prints one line per check, `ok <name>` or
`bad <name> :: <why>`, which sync.sh turns into its own pass and fail counts. It uses only the standard
library: a WebSocket over a plain socket, and git object builders. It never listens and never writes a file.
"""
import base64, hashlib, json, os, re, socket, struct, subprocess, sys, threading, time

sys.stdout.reconfigure(encoding="utf-8", errors="replace")


def say(kind, name, why=""):
    print(f"{kind} {name}" + (f" :: {why}" if why else ""), flush=True)


def check(name, cond, why=""):
    say("ok" if cond else "bad", name, "" if cond else why)


# ---------------------------------------------------------------- git objects

def oid(kind, body):
    return hashlib.sha1(f"{kind} {len(body)}\0".encode() + body).hexdigest()


def blob(data):
    return ("blob", data, oid("blob", data))


def tree(entries):
    body = b"".join(m.encode() + b" " + n + b"\0" + bytes.fromhex(i) for m, n, i in entries)
    return ("tree", body, oid("tree", body))


def git_sort(entries):
    return sorted(entries, key=lambda e: e[1] + (b"/" if e[0] == "40000" else b""))


def commit(tree_id, parent, thread, seq, machine):
    lines = [f"tree {tree_id}"] + ([f"parent {parent}"] if parent else [])
    lines += ["author baton <baton@localhost> 1793508000 +0000", "committer baton <baton@localhost> 1793508000 +0000"]
    msg = (f"wire\n\nBaton-Thread: {thread}\nBaton-Turn: {seq}\nBaton-Agent: claude-code\n"
           f"Baton-Machine: {machine}\nBaton-Base: 0123456789abcdef0123456789abcdef01234567\n")
    body = ("\n".join(lines) + "\n\n" + msg).encode()
    return ("commit", body, oid("commit", body))


KIND = {"commit": 1, "tree": 2, "blob": 3}
CHUNK = 1 << 20
MAX_FRAME = 1049600


def object_frames(xfer, obj):
    kind, body, hexid = obj
    chunks = [body[i:i + CHUNK] for i in range(0, len(body), CHUNK)] or [b""]
    out, off = [], 0
    for c in chunks:
        out.append(b"\x01" + struct.pack("<I", xfer) + bytes([KIND[kind]]) + bytes.fromhex(hexid)
                   + struct.pack("<QQ", len(body), off) + c)
        off += len(c)
    return out


def raw_object_frame(xfer, kind_byte, hexid, total, offset, payload):
    return (b"\x01" + struct.pack("<I", xfer) + bytes([kind_byte]) + bytes.fromhex(hexid)
            + struct.pack("<QQ", total, offset) + payload)


def control(msg):
    return b"\x00" + json.dumps(msg, separators=(",", ":")).encode()


# ---------------------------------------------------------------- websocket

class Closed(Exception):
    pass


class WS:
    def __init__(self, port, token, proto="baton.v1", rcvbuf=None, timeout=10):
        self.s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        if rcvbuf:
            self.s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
        self.s.settimeout(timeout)
        self.s.connect(("127.0.0.1", port))
        req = (f"GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {base64.b64encode(os.urandom(16)).decode()}\r\nSec-WebSocket-Version: 13\r\n")
        if proto:
            req += f"Sec-WebSocket-Protocol: {proto}\r\n"
        if token is not None:
            req += f"Authorization: Bearer {token}\r\n"
        self.s.sendall((req + "\r\n").encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            d = self.s.recv(4096)
            if not d:
                break
            buf += d
        head, _, self.rest = buf.partition(b"\r\n\r\n")
        self.status_line = head.split(b"\r\n")[0].decode(errors="replace")
        self.status = int(self.status_line.split()[1]) if self.status_line else 0

    def send(self, payload):
        n = len(payload)
        if n < 126:
            hdr = struct.pack("!BB", 0x82, 0x80 | n)
        elif n < 65536:
            hdr = struct.pack("!BBH", 0x82, 0x80 | 126, n)
        else:
            hdr = struct.pack("!BBQ", 0x82, 0x80 | 127, n)
        mask = os.urandom(4)
        body = (int.from_bytes(payload, "big") ^ int.from_bytes((mask * (n // 4 + 1))[:n], "big")).to_bytes(n, "big") if n else b""
        self.s.sendall(hdr + mask + body)

    def _read(self, n):
        while len(self.rest) < n:
            d = self.s.recv(1 << 20)
            if not d:
                raise Closed("eof")
            self.rest += d
        out, self.rest = self.rest[:n], self.rest[n:]
        return out

    def recv(self, timeout=None):
        """The next message: ('control', dict), ('object', n), or ('close', None)."""
        if timeout is not None:
            self.s.settimeout(timeout)
        while True:
            b0, b1 = self._read(2)
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack("!H", self._read(2))[0]
            elif n == 127:
                n = struct.unpack("!Q", self._read(8))[0]
            data = self._read(n)
            op = b0 & 0x0F
            if op == 0x8:
                return ("close", None)
            if op == 0x2 and data[:1] == b"\x00":
                return ("control", json.loads(data[1:]))
            if op == 0x2 and data[:1] == b"\x01":
                return ("object", len(data) - 42)

    def control(self, timeout=10):
        """The next control message, skipping object frames, or None when the server closed."""
        try:
            while True:
                kind, v = self.recv(timeout)
                if kind == "control":
                    return v
                if kind == "close":
                    return None
        except (Closed, OSError):
            return None

    def hello(self, projects, machine):
        self.send(control({"t": "hello", "proto": 1, "machine": machine, "projects": projects, "client": "wire"}))
        self.welcome = self.control()
        return self.welcome

    def push(self, xfer, project, thread, expected, new, objs):
        for o in objs:
            for f in object_frames(xfer, o):
                self.send(f)
        self.send(control({"t": "advance", "xfer": xfer, "project": project, "thread": thread,
                           "expected": expected, "new": new}))
        return self.control()

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


class Ctx:
    def __init__(self, port, token, machine, extra):
        self.port, self.token, self.machine, self.extra = port, token, machine, extra
        self.n = 0

    def connect(self, **kw):
        return WS(self.port, self.token, **kw)

    def session(self, projects=(), **kw):
        ws = self.connect(**kw)
        welcome = ws.hello(list(projects), self.machine)
        if not welcome or welcome.get("t") != "welcome":
            raise RuntimeError(f"no welcome: {welcome}")
        return ws

    def thread(self):
        self.n += 1
        return f"w{self.n}"

    def one_turn(self, project, objs_for, thread=None):
        """Pushes a first turn built by objs_for(thread) -> (objects, tip). Returns the server's reply."""
        thread = thread or self.thread()
        ws = self.session([project])
        objs, tip = objs_for(thread)
        reply = ws.push(self.n * 2 + 1, project, thread, None, tip, objs) or {"t": "closed"}
        ws.close()
        return reply

    def alive(self):
        try:
            ws = self.session()
            ws.close()
            return True
        except Exception:
            return False


def one_file(machine, name, content=b"x\n", mode="100644"):
    def build(thread):
        b = blob(content)
        t = tree([(mode, name, b[2])])
        c = commit(t[2], None, thread, 1, machine)
        return [b, t, c], c[2]
    return build


def verdict(reply):
    return f"{reply.get('t')}/{reply.get('reason', '')}" if reply.get("t") == "refused" else reply.get("t")


# ---------------------------------------------------------------- cases

def case_names(c):
    """Finding 1: every name and mode git would not keep is refused; an ordinary tree is not."""
    p, m = "wire/names", c.machine
    check("an ordinary file is accepted", verdict(c.one_turn(p, one_file(m, b"a.txt"))) == "advanced")
    hostile = [
        ("name .git with a trailing space", b".git "), ("name .git with a trailing dot", b".git."),
        ("name .GIT", b".GIT"), ("name git~1", b"git~1"), ("name GIT~1", b"GIT~1"),
        ("name .git::$INDEX_ALLOCATION", b".git::$INDEX_ALLOCATION"),
        ("name .git with a zero-width non-joiner", ".g{}it".format(chr(0x200c)).encode()), ("name .git", b".git"),
        ("name ..", b".."), ("name .git\\x", b".git\\x"),
    ]
    for label, name in hostile:
        check(f"{label} is refused", verdict(c.one_turn(p, one_file(m, name))) == "refused/invalid")
    for label, mode in [("mode 100664", "100664"), ("mode 100600", "100600")]:
        check(f"{label} is refused", verdict(c.one_turn(p, one_file(m, b"a.txt", b"x\n", mode))) == "refused/invalid")
    check(".gitmodules as a symbolic link is refused",
          verdict(c.one_turn(p, one_file(m, b".gitmodules", b"x", "120000"))) == "refused/invalid")
    for label, name in [(".gitignore", b".gitignore"), (".gitattributes", b".gitattributes"), (".mailmap", b".mailmap"),
                        (".GitIgnore", b".GitIgnore"), ("GI250A~1", b"GI250A~1"), (".mailmap with a trailing space", b".mailmap ")]:
        check(f"{label} as a symbolic link is refused",
              verdict(c.one_turn(p, one_file(m, name, b"x", "120000"))) == "refused/invalid")
    check("a regular .gitignore is accepted", verdict(c.one_turn(p, one_file(m, b".gitignore", b"*.log\n"))) == "advanced")

    def null_gitlink(thread):
        t = tree([("160000", b"sub", "0" * 40)])
        cm = commit(t[2], None, thread, 1, m)
        return [t, cm], cm[2]
    check("a tree entry that points to the null id is refused", verdict(c.one_turn(p, null_gitlink)) == "refused/invalid")
    modules = {
        "a url that starts with a dash": '[submodule "a"]\n\tpath = a\n\turl = -oevil\n',
        "a path that starts with a dash": '[submodule "a"]\n\tpath = -x\n\turl = https://example.com/a\n',
        "a submodule name with ..": '[submodule "../x"]\n\tpath = x\n\turl = https://example.com/a\n',
        "an update command": '[submodule "a"]\n\tpath = a\n\turl = https://example.com/a\n\tupdate = !touch pwned\n',
    }
    for label, text in modules.items():
        check(f".gitmodules with {label} is refused",
              verdict(c.one_turn(p, one_file(m, b".gitmodules", text.encode()))) == "refused/invalid")
    check("an ordinary .gitmodules is accepted", verdict(c.one_turn(p, one_file(
        m, b".gitmodules", b'[submodule "a"]\n\tpath = a\n\turl = https://example.com/a\n'))) == "advanced")
    check("the server still answers after the hostile pushes", c.alive())


def case_kinds(c):
    """Finding 2: an object id that is stored as one kind is not accepted as another."""
    p, m = "wire/kinds", c.machine
    first = c.thread()
    b1 = blob(b"hello\n")
    t1 = tree([("100644", b"a.txt", b1[2])])
    c1 = commit(t1[2], None, first, 1, m)
    ws = c.session([p])
    check("the first turn is accepted", verdict(ws.push(1, p, first, None, c1[2], [b1, t1, c1])) == "advanced")
    ws.close()

    def second(name, build):
        thread = c.thread()
        objs, tip = build(thread)
        ws = c.session([p])
        reply = ws.push(1, p, thread, None, tip, objs) or {"t": "closed"}
        ws.close()
        check(f"{name} is refused", verdict(reply) == "refused/invalid", verdict(reply))

    def over(entries):
        def build(thread):
            t = tree(git_sort(entries))
            cm = commit(t[2], None, thread, 1, m)
            return [t, cm], cm[2]
        return build

    def commit_over_blob(thread):
        cm = commit(b1[2], None, thread, 1, m)
        return [cm], cm[2]

    second("a tree entry that names a stored blob as a directory", over([("100644", b"a.txt", b1[2]), ("40000", b"x", b1[2])]))
    second("a file entry that names a stored tree", over([("100644", b"a.txt", b1[2]), ("100644", b"y", t1[2])]))
    second("a commit whose tree is a stored blob", commit_over_blob)
    second("a stored blob reused as a directory", over([("40000", b"a.txt", b1[2])]))
    check("the server is still up", c.alive())


def case_depth(c):
    """Finding 4: trees nest at most 512 levels, and a deep chain never takes the server down."""
    p, m = "wire/depth", c.machine

    def chain(levels):
        def build(thread):
            b = blob(b"leaf\n")
            objs, cur = [b], tree([("100644", b"f", b[2])])
            objs.append(cur)
            for _ in range(levels):
                cur = tree([("40000", b"d", cur[2])])
                objs.append(cur)
            cm = commit(cur[2], None, thread, 1, m)
            return objs + [cm], cm[2]
        return build

    check("512 levels are accepted", verdict(c.one_turn(p, chain(512))) == "advanced")
    check("513 levels are refused", verdict(c.one_turn(p, chain(513))) == "refused/invalid")
    check("20,000 levels are refused", verdict(c.one_turn(p, chain(20000))) == "refused/invalid")
    check("the server is still up after the deep tree", c.alive())


def case_partial(c):
    """Finding 5: announcing big objects and never sending them is cut off by the session budget."""
    ws = c.session()
    end, at = None, None
    for i in range(12):
        ws.send(raw_object_frame(7, 3, hashlib.sha1(f"partial-{i}".encode()).hexdigest(), 64 << 20, 0, os.urandom(8)))
        end = ws.control(timeout=0.3)
        if end is not None:
            at = i + 1
            break
    ws.close()
    check("announcing 64 MiB objects without sending them ends the session with too_large by the ninth",
          end and end.get("code") == "too_large" and at is not None and at <= 9, f"at {at}: {end}")
    check("the server is still up", c.alive())


def refused_after(c, count, step):
    """Runs step(ws, i) up to count times on one session; each call returns that step's one answer.
    Gives (answers before the session ended, the error it ended with)."""
    ws = c.session()
    answers, end = 0, None
    for i in range(count):
        try:
            reply = step(ws, i)
        except (OSError, Closed):
            reply = None
        if reply is None or reply.get("t") == "error":
            end = reply or {"code": "closed"}
            break
        answers += 1
    ws.close()
    return answers, end


def case_flood(c):
    """Findings 6 and 7: naming many projects is bounded per session and costs the others nothing."""
    def subscribe(ws, i):
        p = f"wire/sub-{i}"
        ws.send(control({"t": "subscribe", "project": p}))
        ws.send(control({"t": "want", "xfer": i + 1, "project": p, "ids": []}))
        return ws.control(timeout=10)

    def advance(ws, i):
        ws.send(control({"t": "advance", "xfer": 2 * i + 1, "project": f"wire/adv-{i}", "thread": "t",
                         "expected": None, "new": "1" * 40}))
        return ws.control(timeout=10)

    for name, step in (("subscribe", subscribe), ("advance", advance)):
        answers, end = refused_after(c, 600, step)
        check(f"{name} to 600 distinct projects is answered 256 times, then ends with too_large",
              answers == 256 and end and end.get("code") == "too_large", f"{answers} answers, then {end}")

    # The watcher is its own process so that the flood cannot slow it down through the interpreter lock.
    watcher = subprocess.Popen([sys.executable, "-I", os.path.abspath(__file__), "watch", str(c.port), sys.argv[3], c.machine],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    time.sleep(1)
    flooder = None
    for i in range(20000):
        try:
            if flooder is None:
                flooder = c.session()
            flooder.send(control({"t": "subscribe", "project": f"wire/flood-{i}"}))
        except OSError:
            if flooder is not None:
                flooder.close()
            flooder = None
    if flooder is not None:
        flooder.close()
    watcher.stdin.close()
    worst = float(watcher.stdout.read().strip() or "inf")
    watcher.wait()
    check("another session's round trip stays under 1 s through 20,000 subscribes to distinct projects", worst < 1.0, f"{worst:.2f}s")
    check("the server is still up", c.alive())


def case_watch(c):
    """Round trips of a `want` on one session until stdin closes. Prints the worst, in seconds."""
    worst, stop = 0.0, threading.Event()

    def loop():
        nonlocal worst
        w = c.session(["wire/watch"])
        while not stop.is_set():
            t = time.time()
            w.send(control({"t": "want", "xfer": 1, "project": "wire/watch", "ids": []}))
            if w.control(timeout=120) is None:
                worst = float("inf")
                break
            worst = max(worst, time.time() - t)
            time.sleep(0.02)
        w.close()

    th = threading.Thread(target=loop)
    th.start()
    sys.stdin.read()
    stop.set()
    th.join()
    print(worst, flush=True)


def case_quiet(c):
    """Finding 7: a project's writer ends when it is idle, and the next advance starts a new one."""
    p, m = "wire/idle", c.machine
    thread = c.thread()
    b = blob(b"one\n")
    t = tree([("100644", b"a.txt", b[2])])
    c1 = commit(t[2], None, thread, 1, m)
    ws = c.session([p])
    check("the first advance is accepted", verdict(ws.push(1, p, thread, None, c1[2], [b, t, c1])) == "advanced")
    time.sleep(45)
    b2 = blob(b"two\n")
    t2 = tree([("100644", b"a.txt", b2[2])])
    c2 = commit(t2[2], c1[2], thread, 2, m)
    check("an advance after the writer stopped is accepted", verdict(ws.push(3, p, thread, c1[2], c2[2], [b2, t2, c2])) == "advanced")
    ws.close()


def case_frames(c):
    """Finding 11: max_frame, as the welcome announces it, is exact."""
    def padded(size):
        msg = {"t": "want", "xfer": 1, "project": "wire/frames", "ids": [], "pad": ""}
        msg["pad"] = "x" * (size - len(control(msg)))
        f = control(msg)
        assert len(f) == size
        return f

    ws = c.session()
    limit = ws.welcome["limits"]["max_frame"]
    ws.send(padded(limit))
    reply = ws.control(timeout=10)
    check(f"a frame of exactly max_frame ({limit}) bytes is served", reply and reply.get("t") == "objects_sent", str(reply))
    ws.close()
    for extra in (1, 64, 65):
        ws = c.session()
        ws.send(padded(limit + extra))
        reply = ws.control(timeout=10)
        check(f"a frame of max_frame + {extra} bytes ends the session with protocol",
              reply is not None and reply.get("code") == "protocol", str(reply))
        ws.close()


def case_want(c):
    """Finding 9: a want that repeats one id 2,048 times reads it once."""
    p, m = "wire/want", c.machine
    big = blob(os.urandom(32 << 20))
    big_tree = tree([("100644", b"big.bin", big[2])])

    def build(thread):
        cm = commit(big_tree[2], None, thread, 1, m)
        return [big, big_tree, cm], cm[2]

    check("a 32 MiB blob is stored", verdict(c.one_turn(p, build)) == "advanced")
    ws = c.session([p], timeout=120)
    t0 = time.time()
    ws.send(control({"t": "want", "xfer": 1, "project": p, "ids": [big[2]] * 2048}))
    objects, end = 0, None
    while True:
        kind, v = ws.recv(timeout=120)
        if kind == "object":
            objects += 1
            continue
        end = v
        break
    took = time.time() - t0
    check("the repeated want is answered", end and end.get("t") == "objects_sent", str(end))
    check("the blob is sent once, in 32 frames", objects == 32, f"{objects} frames")
    check("and within 20 s", took < 20, f"{took:.1f}s")
    ws.close()


def case_handshake(c):
    """Finding 10: connections that never finish the upgrade are closed, and the number is capped."""
    def closed_within(socks, seconds):
        end, gone = time.time() + seconds, set()
        while time.time() < end and len(gone) < len(socks):
            for i, s in enumerate(socks):
                if i in gone:
                    continue
                s.settimeout(0.01)
                try:
                    if s.recv(1) == b"":
                        gone.add(i)
                except (socket.timeout, TimeoutError):
                    pass
                except OSError:
                    gone.add(i)
        return len(gone)

    socks = [socket.create_connection(("127.0.0.1", c.port)) for _ in range(40)]
    socks[0].sendall(b"GET / HTTP/1.1\r\nHost: x\r\n")
    gone = closed_within(socks, 20)
    check("40 connections that send nothing or half a request are closed within 20 s", gone == 40, f"{gone} of 40")
    for s in socks:
        s.close()

    socks = []
    for _ in range(200):
        try:
            socks.append(socket.create_connection(("127.0.0.1", c.port)))
        except OSError:
            break
    early = closed_within(socks, 2)
    check("past 128 pending handshakes the server drops new connections at accept", early >= 40, f"{early} dropped of {len(socks)}")
    gone = closed_within(socks, 20)
    check("and the rest are closed within 20 s", gone == len(socks), f"{gone} of {len(socks)}")
    for s in socks:
        s.close()
    check("a normal client connects afterwards", c.alive())


def case_stall(c):
    """Finding 8: a client that stops reading is dropped, not held. extra = [the server's log file].
    The error alone is not enough, since it queues behind the unread data: the socket has to be closed."""
    p, m = "wire/stall", c.machine
    big = blob(os.urandom(32 << 20))
    big_tree = tree([("100644", b"big.bin", big[2])])

    def build(thread):
        cm = commit(big_tree[2], None, thread, 1, m)
        return [big, big_tree, cm], cm[2]

    check("a 32 MiB blob is stored", verdict(c.one_turn(p, build)) == "advanced")
    s = c.session([p], rcvbuf=4096, timeout=120)
    for _ in range(64):
        for f in object_frames(3, blob(os.urandom(1 << 20))):
            s.send(f)
    s.send(control({"t": "want", "xfer": 5, "project": p, "ids": [big[2]]}))
    time.sleep(50)
    text = open(c.extra[0], encoding="utf-8", errors="replace").read().splitlines()
    gave_up = [i for i, line in enumerate(text) if "the client did not read for 30s" in line]
    closed = False
    for i in gave_up:
        m = re.search(r"session (\d+) \(", text[i])
        closed = closed or bool(m and any(re.search(rf"session {m.group(1)} \(.*\): closed", line) for line in text[i:]))
    check("the server gave up on the reader after 30 s and closed its socket without waiting for it", closed,
          f"{len(gave_up)} sessions gave up, none closed")
    drained, how = 0, "still open"
    try:
        while True:
            kind, v = s.recv(timeout=30)
            if kind == "object":
                drained += v
            elif kind == "control" and v.get("t") == "objects_sent":
                how = "the want was answered"
                break
            elif kind == "close":
                how = "closed"
                break
    except Closed:
        how = "closed"
    except socket.timeout:
        how = "no data for 30 s"
    except OSError:
        how = "reset"
    # What the server could still send after it gave up sits in socket buffers, a few MiB at most.
    check("after 50 s without reading the server has ended the session without finishing the want",
          how in ("closed", "reset") and drained < (16 << 20), f"{how} after {drained} payload bytes")
    s.close()


def case_revoke(c):
    """Finding 13: a revoked device gets nothing more.
    extra = [token file of another device, its machine name, the command that revokes this device]."""
    other_token, other_machine, *revoke = c.extra
    p = "wire/revoke"
    victim = c.session([p])
    other = WS(c.port, open(other_token).read().strip())
    other.hello([p], other_machine)
    t0 = time.time()
    done = subprocess.run(revoke, capture_output=True, text=True)
    check("the revoke command succeeds", done.returncode == 0, done.stdout + done.stderr)
    got = victim.control(timeout=8)
    took = time.time() - t0
    check("the open session of the revoked device is told unauthorized", got is not None and got.get("code") == "unauthorized", str(got))
    check("and that happens within 4 s of the revoke", took < 4.0, f"{took:.1f}s")
    thread = "rv"
    b = blob(b"after the revoke\n")
    t = tree([("100644", b"plan.txt", b[2])])
    cm = commit(t[2], None, thread, 1, other_machine)
    check("another device still advances the project", verdict(other.push(1, p, thread, None, cm[2], [b, t, cm])) == "advanced")
    other.close()
    leaked = False
    try:
        while True:
            kind, v = victim.recv(timeout=2)
            if kind == "object" or (kind == "control" and v.get("t") == "moved"):
                leaked = True
            if kind == "close":
                break
    except (Closed, OSError):
        pass
    check("nothing from the other device reaches the revoked session", not leaked)
    victim.close()


def case_connect(c):
    """Prints `<status> <milliseconds>` for one connect with the given token, for the auth deadline checks."""
    t0 = time.time()
    try:
        ws = WS(c.port, c.token, timeout=60)
        status = ws.status
        ws.close()
    except Exception:
        status = 0
    print(f"{status} {int((time.time() - t0) * 1000)}", flush=True)


CASES = {name[5:]: fn for name, fn in globals().items() if name.startswith("case_")}

if __name__ == "__main__":
    case, port, token_file, machine = sys.argv[1:5]
    token = open(token_file).read().strip()
    ctx = Ctx(int(port), token, machine, sys.argv[5:])
    try:
        CASES[case](ctx)
    except Exception as e:
        say("bad", f"{case} ran to the end", repr(e))
