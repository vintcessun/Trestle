#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Trestle 远端常驻 agent。

一条 SSH channel 上跑 JSON-Lines，多路复用 + 并发处理。负责把服务器的东西传过来、
把主机的指令发过去——就这一件事，七个基本操作的远端一侧。

**只用标准库。** 没有第三方依赖，所以整组机器上任何 3.9+ 的解释器都能直接跑；
uv 的作用是把解释器版本固定下来，不是拉包。

协议
----
请求（每行一个 JSON）::

    {"id": 7, "op": "read", "args": {...}}

响应（同 id，顺序不保证——并发处理，谁先完谁先回）::

    {"id": 7, "ok": true,  "result": {...}}
    {"id": 7, "ok": false, "error": {"kind": "not_found", "detail": "..."}}

为什么是常驻进程而不是每次 ``ssh host cmd``：每次新建 exec channel + 起一个解释器，
跨 VPN 是几百 ms 起步；常驻之后每次调用只是在已有通道上发一行收一行，实测 33–52ms。

两种运行模式
------------

``--serve <sock>``
    绑一个 AF_UNIX socket 常驻，每个连进来的客户端各跑一遍协议循环。
    **agent 的生命周期因此与任何一条 SSH 连接无关**——主机重启、daemon 重启、
    网络抖断，agent 都还在，下次连上来直接接管（不重新部署、不重起、任务不丢）。
    SSH channel 上跑的是 ``relay.py``，它只把字节在 socket 与 stdio 之间搬来搬去。

无参数
    直接在 stdin/stdout 上跑协议。测试用，也是不想常驻时的退路。
"""

from __future__ import annotations

import base64
import errno
import fnmatch
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
import traceback

PROTOCOL_VERSION = 1
AGENT_VERSION = "0.1.0"

# 单帧内容上限。base64 之后约 1.4 倍，留足余量避免行缓冲爆掉。
MAX_CHUNK = 1 << 19  # 512 KiB
# read 默认最多返回这么多字节，防止一次把 8G 日志拉过来。
DEFAULT_MAX_BYTES = 1 << 20


class OpError(Exception):
    """能翻译成结构化错误响应的失败。

    ``kind`` 给主机侧做分类（比如 not_found 不该触发重连），``detail`` 给 agent 读。
    """

    def __init__(self, kind: str, detail: str) -> None:
        super().__init__(detail)
        self.kind = kind
        self.detail = detail


# ────────────────────────────── 工具函数 ──────────────────────────────


def expand(path: str) -> str:
    """展开 ``~`` 与环境变量。主机侧传过来的路径经常带 ``~``。"""
    return os.path.expanduser(os.path.expandvars(path))


def ensure_parent(path: str) -> None:
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def translate_oserror(exc: OSError, path: str) -> OpError:
    if exc.errno == errno.ENOENT:
        return OpError("not_found", "no such file or directory: %s" % path)
    if exc.errno == errno.EACCES:
        return OpError("permission_denied", "permission denied: %s" % path)
    if exc.errno == errno.EISDIR:
        return OpError("is_a_directory", "%s is a directory" % path)
    if exc.errno == errno.ENOSPC:
        return OpError("no_space", "no space left on device while writing %s" % path)
    return OpError("io_error", "%s: %s" % (path, exc))


def excluded(rel: str, patterns: "list[str]") -> bool:
    """任一路径分量匹配任一 glob 就算被排除。

    这样 ``__pycache__`` 能挡掉 ``a/b/__pycache__/c.pyc``，而不只是顶层那个。
    """
    if not patterns:
        return False
    parts = rel.replace("\\", "/").split("/")
    for pat in patterns:
        if fnmatch.fnmatch(rel, pat):
            return True
        if any(fnmatch.fnmatch(p, pat) for p in parts):
            return True
    return False


# ──────────────────────────────── 操作 ────────────────────────────────


def op_ping(_args: dict) -> dict:
    return {
        "pong": True,
        "protocol": PROTOCOL_VERSION,
        "version": AGENT_VERSION,
        # 自己这份源码的哈希。主机侧靠它判断「远端跑的是不是当前这版」，
        # 不用为此多发一次 sha256sum —— 重新 attach 的路径上每个 round-trip 都算数。
        "script_sha256": _SCRIPT_SHA256,
        "pid": os.getpid(),
        "started_at": _STARTED_AT,
        "uptime_s": int(time.time() - _STARTED_AT),
        "python": sys.version.split()[0],
        "cwd": os.getcwd(),
    }


def op_read(args: dict) -> dict:
    path = expand(args["path"])
    start_line = args.get("start_line")
    max_lines = args.get("max_lines")
    max_bytes = args.get("max_bytes") or DEFAULT_MAX_BYTES

    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            lines = fh.readlines()
    except OSError as exc:
        raise translate_oserror(exc, path)

    total_lines = len(lines)
    begin = max(0, (start_line - 1)) if start_line else 0
    end = begin + max_lines if max_lines else total_lines
    selected = lines[begin:end]

    truncated = end < total_lines or begin > 0
    content = "".join(selected)
    encoded = content.encode("utf-8")
    if len(encoded) > max_bytes:
        content = encoded[:max_bytes].decode("utf-8", errors="ignore")
        truncated = True

    return {"content": content, "total_lines": total_lines, "truncated": truncated}


def op_write(args: dict) -> dict:
    path = expand(args["path"])
    content = args["content"]
    if args.get("make_dirs"):
        ensure_parent(path)
    mode = "a" if args.get("append") else "w"
    try:
        with open(path, mode, encoding="utf-8") as fh:
            written = fh.write(content)
    except OSError as exc:
        raise translate_oserror(exc, path)
    # 返回的 path 就是入参的 path —— 见 docs/07 第 4 坑。
    return {"bytes": written, "path": args["path"]}


def op_edit(args: dict) -> dict:
    path = expand(args["path"])
    op = args["op"]
    kind = op["kind"]

    try:
        with open(path, "r", encoding="utf-8") as fh:
            original = fh.read()
    except OSError as exc:
        raise translate_oserror(exc, path)

    if kind == "literal":
        old, new = op["old"], op["new"]
        count = op.get("count", 0)
        occurrences = original.count(old) if count == 0 else min(original.count(old), count)
        updated = original.replace(old, new) if count == 0 else original.replace(old, new, count)
    elif kind == "regex":
        flags = 0
        for ch in op.get("flags", ""):
            flags |= {"i": re.IGNORECASE, "m": re.MULTILINE, "s": re.DOTALL, "x": re.VERBOSE}.get(ch, 0)
        try:
            pattern = re.compile(op["pattern"], flags)
        except re.error as exc:
            raise OpError("bad_pattern", "invalid regex %r: %s" % (op["pattern"], exc))
        updated, occurrences = pattern.subn(op["replacement"], original, count=op.get("count", 0))
    elif kind == "lines":
        lines = original.splitlines(keepends=True)
        start, end = op["start"], op["end"]
        if start < 1 or end < start or start > len(lines):
            raise OpError(
                "bad_range",
                "line range %d-%d is outside the file (%d lines)" % (start, end, len(lines)),
            )
        replacement = op["replacement"]
        if replacement and not replacement.endswith("\n"):
            replacement += "\n"
        updated = "".join(lines[: start - 1]) + replacement + "".join(lines[end:])
        occurrences = 1
    elif kind == "insert":
        lines = original.splitlines(keepends=True)
        before = op["before_line"]
        if before < 1 or before > len(lines) + 1:
            raise OpError(
                "bad_range",
                "cannot insert before line %d (file has %d lines)" % (before, len(lines)),
            )
        content = op["content"]
        if content and not content.endswith("\n"):
            content += "\n"
        updated = "".join(lines[: before - 1]) + content + "".join(lines[before - 1 :])
        occurrences = 1
    else:
        raise OpError("bad_request", "unknown edit kind %r" % kind)

    changed = updated != original
    if changed:
        # 原子替换：半路失败也不会留下一个被截断的文件。
        tmp = path + ".trestle.tmp"
        try:
            with open(tmp, "w", encoding="utf-8") as fh:
                fh.write(updated)
            os.replace(tmp, path)
        except OSError as exc:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise translate_oserror(exc, path)

    return {"changed": changed, "occurrences": occurrences, "path": args["path"]}


def _build_env(args: dict) -> dict:
    env = os.environ.copy()
    for pair in args.get("env") or []:
        env[str(pair[0])] = str(pair[1])
    return env


def op_shell(args: dict) -> dict:
    if args.get("detach"):
        return _shell_detached(args)
    return _shell_exec(args)


def _shell_exec(args: dict) -> dict:
    command = args["command"]
    cwd = expand(args["cwd"]) if args.get("cwd") else None
    timeout = args.get("timeout_secs")
    started = time.time()

    try:
        # start_new_session=True → setsid：拿到独立进程组，超时才能连孙进程一起杀掉。
        proc = subprocess.Popen(
            ["/bin/bash", "-lc", command],
            cwd=cwd,
            env=_build_env(args),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except OSError as exc:
        raise OpError("spawn_failed", "cannot start shell: %s" % exc)

    timed_out = False
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        # 杀**整个进程组**。只杀直接子进程的话孙进程会残留下来继续跑。
        _kill_group(proc.pid, signal.SIGKILL)
        try:
            stdout, stderr = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            stdout, stderr = b"", b""

    return {
        "exit_code": proc.returncode if proc.returncode is not None else -1,
        "stdout": stdout.decode("utf-8", errors="replace"),
        "stderr": stderr.decode("utf-8", errors="replace"),
        "timed_out": timed_out,
        "duration_ms": int((time.time() - started) * 1000),
    }


def _shell_detached(args: dict) -> dict:
    """脱离会话在后台跑。SSH 断了照跑，pid / 退出码 / 日志全部落盘。

    这里避开了上一代踩的两个坑：

    1. **不用 shell 的 ``&``**。``cd DIR && cmd &`` 会让调用方一直阻塞到任务结束——
       ``&`` 作用于整个 ``&&`` 列表，bash fork 出的子 shell 一直攥着调用方的 stdout 管道，
       读端要等任务结束才拿到 EOF。这里用 ``start_new_session`` 直接 fork，
       工作目录交给进程的 cwd，压根不经过 shell 的后台机制。
    2. **不用 ``$!`` 取 pid**。``setsid`` 会 fork，``$!`` 是 setsid 自己的 pid，
       它随即退出，于是 pid 立刻「死了」。这里 ``Popen`` 拿到的就是新会话首进程的 pid，
       而它同时是 pgid（会话首进程 pid == pgid），停止任务时按这个 pgid 杀整组。
    """
    command = args["command"]
    cwd = expand(args["cwd"]) if args.get("cwd") else None
    name = args.get("name") or "job"
    safe = re.sub(r"[^A-Za-z0-9._-]", "-", name)[:40]
    job_id = "%s-%d-%d" % (safe, int(time.time()), os.getpid() % 100000)

    job_dir = os.path.join(_JOBS_DIR, job_id)
    os.makedirs(job_dir, exist_ok=True)
    log_path = os.path.join(job_dir, "out.log")
    rc_path = os.path.join(job_dir, "rc")
    meta_path = os.path.join(job_dir, "meta.json")

    # 退出码由被包一层的 shell 自己落盘，这样 agent 死了也照样有退出码。
    wrapped = "%s\nprintf %%s $? > %s\n" % (command, _shquote(rc_path))

    try:
        logfile = open(log_path, "ab", buffering=0)
    except OSError as exc:
        raise translate_oserror(exc, log_path)

    try:
        proc = subprocess.Popen(
            ["/bin/bash", "-lc", wrapped],
            cwd=cwd,
            env=_build_env(args),
            stdin=subprocess.DEVNULL,
            stdout=logfile,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    except OSError as exc:
        logfile.close()
        raise OpError("spawn_failed", "cannot start background job: %s" % exc)
    finally:
        try:
            logfile.close()
        except OSError:
            pass

    meta = {
        "job_id": job_id,
        "name": name,
        "command": command,
        "cwd": cwd or os.getcwd(),
        "pid": proc.pid,
        "pgid": proc.pid,  # 会话首进程：pid == pgid
        "started_at": int(time.time()),
        "log_path": log_path,
        "rc_path": rc_path,
    }
    with open(meta_path, "w", encoding="utf-8") as fh:
        json.dump(meta, fh)

    # 收尸，避免僵尸进程堆积。父进程死掉也不影响任务本身——它已经在新会话里了。
    threading.Thread(target=_reap, args=(proc,), daemon=True).start()

    return {
        "pid": proc.pid,
        "pgid": proc.pid,
        "job_id": job_id,
        "log_path": log_path,
        "meta_path": meta_path,
        "rc_path": rc_path,
    }


def _reap(proc: "subprocess.Popen") -> None:
    try:
        proc.wait()
    except Exception:
        pass


def _shquote(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"


def _kill_group(pid: int, sig: int) -> bool:
    """按进程组杀。杀不到就退回杀单个进程。"""
    try:
        os.killpg(os.getpgid(pid), sig)
        return True
    except OSError:
        try:
            os.kill(pid, sig)
            return True
        except OSError:
            return False


def op_signal(args: dict) -> dict:
    """给一个进程组发信号。job 插件的 stop 靠它。"""
    pid = int(args["pid"])
    name = args.get("signal", "TERM").upper()
    sig = getattr(signal, "SIG" + name, None) if not name.startswith("SIG") else getattr(signal, name, None)
    if sig is None:
        raise OpError("bad_request", "unknown signal %r" % args.get("signal"))
    delivered = _kill_group(pid, sig)
    return {"delivered": delivered}


def op_stat(args: dict) -> dict:
    path = expand(args["path"])
    try:
        st = os.stat(path)
    except OSError as exc:
        raise translate_oserror(exc, path)
    return {
        "path": args["path"],
        "size": st.st_size,
        "mtime": int(st.st_mtime),
        "mode": st.st_mode & 0o7777,
        "is_dir": os.path.isdir(path),
        "exists": True,
    }


def op_exists(args: dict) -> dict:
    path = expand(args["path"])
    return {"exists": os.path.exists(path), "is_dir": os.path.isdir(path)}


def op_mkdirs(args: dict) -> dict:
    path = expand(args["path"])
    try:
        os.makedirs(path, exist_ok=True)
    except OSError as exc:
        raise translate_oserror(exc, path)
    return {"path": args["path"]}


def op_list_tree(args: dict) -> dict:
    """列出一棵目录树。目录同步时主机侧靠它做 size+mtime 比对。"""
    root = expand(args["path"])
    exclude = args.get("exclude") or []
    if not os.path.exists(root):
        raise OpError("not_found", "no such file or directory: %s" % args["path"])

    if os.path.isfile(root):
        st = os.stat(root)
        return {
            "root": args["path"],
            "is_dir": False,
            "entries": [{"rel": os.path.basename(root), "size": st.st_size, "mtime": int(st.st_mtime)}],
        }

    entries = []
    for dirpath, dirnames, filenames in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        rel_dir = "" if rel_dir == "." else rel_dir.replace(os.sep, "/")
        # 就地裁剪 dirnames，被排除的目录整棵不进去。
        dirnames[:] = [
            d for d in dirnames if not excluded("%s/%s" % (rel_dir, d) if rel_dir else d, exclude)
        ]
        for fn in filenames:
            rel = "%s/%s" % (rel_dir, fn) if rel_dir else fn
            if excluded(rel, exclude):
                continue
            try:
                st = os.stat(os.path.join(dirpath, fn))
            except OSError:
                continue  # 遍历途中消失的文件不该让整次列举失败
            entries.append({"rel": rel, "size": st.st_size, "mtime": int(st.st_mtime)})
    return {"root": args["path"], "is_dir": True, "entries": entries}


def op_put_chunk(args: dict) -> dict:
    """接收一块上传数据。

    先写 ``<path>.trestle.part``，收到 ``final`` 时校验 sha256 再原子改名——
    半路断掉不会在目标位置留下一个看起来完整的半截文件。
    """
    path = expand(args["path"])
    offset = int(args.get("offset", 0))
    data = base64.b64decode(args["data"]) if args.get("data") else b""
    part = path + ".trestle.part"

    if offset == 0:
        ensure_parent(path)
    try:
        with open(part, "r+b" if os.path.exists(part) else "wb") as fh:
            fh.seek(offset)
            fh.write(data)
    except OSError as exc:
        raise translate_oserror(exc, path)

    if not args.get("final"):
        return {"written": len(data), "offset": offset + len(data), "done": False}

    actual = sha256_file(part)
    expected = args.get("sha256")
    if expected and actual != expected:
        os.unlink(part)
        raise OpError(
            "checksum_mismatch",
            "upload of %s failed verification (expected %s, got %s); nothing was written"
            % (args["path"], expected, actual),
        )
    try:
        os.replace(part, path)
        if args.get("mode") is not None:
            os.chmod(path, int(args["mode"]))
        # 保留源文件的 mtime。增量同步靠 size+mtime 比对，如果这里用「写入时刻」，
        # 两台机器之间哪怕一秒的时钟偏差都会让下次同步误判成「变了」，白传一遍。
        if args.get("mtime") is not None:
            mtime = int(args["mtime"])
            os.utime(path, (mtime, mtime))
    except OSError as exc:
        raise translate_oserror(exc, path)

    st = os.stat(path)
    # 落地路径 == 入参路径。
    return {"written": len(data), "done": True, "sha256": actual, "size": st.st_size, "path": args["path"]}


def op_get_chunk(args: dict) -> dict:
    """送出一块下载数据。"""
    path = expand(args["path"])
    offset = int(args.get("offset", 0))
    length = min(int(args.get("length", MAX_CHUNK)), MAX_CHUNK)
    try:
        with open(path, "rb") as fh:
            fh.seek(offset)
            data = fh.read(length)
        size = os.path.getsize(path)
    except OSError as exc:
        raise translate_oserror(exc, path)
    return {
        "data": base64.b64encode(data).decode("ascii"),
        "offset": offset,
        "eof": offset + len(data) >= size,
        "size": size,
    }


def op_hash(args: dict) -> dict:
    path = expand(args["path"])
    try:
        return {"sha256": sha256_file(path), "path": args["path"]}
    except OSError as exc:
        raise translate_oserror(exc, path)


def op_shutdown(_args: dict) -> dict:
    """主机侧要求退出。用于版本不匹配时换掉旧 agent。"""
    threading.Timer(0.2, lambda: os._exit(0)).start()
    return {"bye": True}


OPS = {
    "ping": op_ping,
    "read": op_read,
    "write": op_write,
    "edit": op_edit,
    "shell": op_shell,
    "signal": op_signal,
    "stat": op_stat,
    "exists": op_exists,
    "mkdirs": op_mkdirs,
    "list_tree": op_list_tree,
    "put_chunk": op_put_chunk,
    "get_chunk": op_get_chunk,
    "hash": op_hash,
    "shutdown": op_shutdown,
}


# ─────────────────────────────── 主循环 ───────────────────────────────

_STARTED_AT = time.time()
_AGENT_DIR = expand(os.environ.get("TRESTLE_AGENT_DIR", "~/.trestle"))
_JOBS_DIR = os.path.join(_AGENT_DIR, "jobs")


def _self_sha256() -> str:
    try:
        return sha256_file(os.path.abspath(__file__))
    except OSError:
        return ""


_SCRIPT_SHA256 = _self_sha256()


class Responder:
    """一个客户端连接的写端。每条连接一把锁，帧不会交错。"""

    def __init__(self, write) -> None:
        self._write = write
        self._lock = threading.Lock()

    def send(self, payload: dict) -> None:
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
        with self._lock:
            try:
                self._write(line)
            except (BrokenPipeError, ValueError, OSError):
                pass  # 客户端走了；别让一条死连接把 agent 拖垮


def handle(responder: "Responder", request: dict) -> None:
    req_id = request.get("id")
    op_name = request.get("op")
    handler = OPS.get(op_name)
    if handler is None:
        responder.send(
            {
                "id": req_id,
                "ok": False,
                "error": {
                    "kind": "unknown_op",
                    "detail": "unknown op %r; known: %s" % (op_name, ", ".join(sorted(OPS))),
                },
            }
        )
        return
    try:
        responder.send({"id": req_id, "ok": True, "result": handler(request.get("args") or {})})
    except OpError as exc:
        responder.send({"id": req_id, "ok": False, "error": {"kind": exc.kind, "detail": exc.detail}})
    except KeyError as exc:
        responder.send(
            {
                "id": req_id,
                "ok": False,
                "error": {"kind": "bad_request", "detail": "missing field %s for op %r" % (exc, op_name)},
            }
        )
    except Exception as exc:  # noqa: BLE001 - agent 绝不能因为一个请求而整个死掉
        responder.send(
            {
                "id": req_id,
                "ok": False,
                "error": {
                    "kind": "internal",
                    "detail": "%s: %s" % (type(exc).__name__, exc),
                    "traceback": traceback.format_exc(limit=6),
                },
            }
        )


def serve_lines(read_line, responder: "Responder") -> None:
    """一条连接上的协议循环。"""
    # 就绪帧：主机侧读到它才认为通道可用，不靠 sleep 猜。
    responder.send({"id": 0, "ok": True, "result": op_ping({})})
    while True:
        raw = read_line()
        if not raw:
            break
        raw = raw.strip()
        if not raw:
            continue
        try:
            request = json.loads(raw)
        except ValueError as exc:
            responder.send({"id": None, "ok": False, "error": {"kind": "bad_frame", "detail": str(exc)}})
            continue
        # 每个请求一个线程：多路复用的意义就在于慢操作不挡住快操作。
        threading.Thread(target=handle, args=(responder, request), daemon=True).start()


def serve_stdio() -> int:
    responder = Responder(lambda line: (sys.stdout.write(line), sys.stdout.flush()))
    serve_lines(sys.stdin.readline, responder)
    return 0


def _serve_connection(conn) -> None:
    try:
        reader = conn.makefile("r", encoding="utf-8", newline="\n")
        def write(line):
            conn.sendall(line.encode("utf-8"))
        serve_lines(reader.readline, Responder(write))
    except OSError:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def serve_socket(sock_path: str) -> int:
    """绑 AF_UNIX socket 常驻。

    agent 因此**不依附于任何一条 SSH 连接**：网络断了、daemon 重启了、
    你合上笔记本了，它都还在；下次连上来直接接管，任务照跑、状态不丢。
    """
    import socket as _socket

    sock_path = expand(sock_path)
    ensure_parent(sock_path)

    if os.path.exists(sock_path):
        # 先探一下：真有 agent 在听就退位让贤，只是个陈旧的 socket 文件就清掉。
        probe = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        try:
            probe.settimeout(2)
            probe.connect(sock_path)
            probe.close()
            sys.stderr.write("another agent is already serving %s\n" % sock_path)
            return 3
        except OSError:
            try:
                os.unlink(sock_path)
            except OSError:
                pass
        finally:
            try:
                probe.close()
            except OSError:
                pass

    server = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    server.bind(sock_path)
    os.chmod(sock_path, 0o600)  # 同机其他用户不该能指挥这个 agent
    server.listen(16)

    with open(os.path.join(_AGENT_DIR, "agent.json"), "w", encoding="utf-8") as fh:
        json.dump(
            {
                "pid": os.getpid(),
                "sock": sock_path,
                "version": AGENT_VERSION,
                "protocol": PROTOCOL_VERSION,
                "started_at": int(_STARTED_AT),
            },
            fh,
        )

    while True:
        try:
            conn, _ = server.accept()
        except OSError:
            break
        threading.Thread(target=_serve_connection, args=(conn,), daemon=True).start()
    return 0


def main(argv: "list[str]") -> int:
    os.makedirs(_JOBS_DIR, exist_ok=True)
    if len(argv) >= 2 and argv[0] == "--serve":
        return serve_socket(argv[1])
    return serve_stdio()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
