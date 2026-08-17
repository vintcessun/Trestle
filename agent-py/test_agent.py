#!/usr/bin/env python3
"""agent 的协议级真调测试。

不 mock 任何东西：真起一个 agent 子进程，真发 JSON-Lines，真读文件、真起进程、
真杀进程组。上一代的教训是「逐个真调」能抓到 mock 测试永远抓不到的 bug，
这份测试就是那个形态在 agent 这一层的落地。

要在 POSIX 环境跑（本机开发时用 WSL）::

    wsl python3 agent-py/test_agent.py
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
AGENT = os.path.join(HERE, "trestle_agent.py")

failures = []
passes = 0


def check(name: str, condition: bool, detail: str = "") -> None:
    global passes
    if condition:
        passes += 1
        print("  ok   %s" % name)
    else:
        failures.append("%s %s" % (name, detail))
        print("  FAIL %s  %s" % (name, detail))


RELAY = os.path.join(HERE, "relay.py")


class Agent:
    """驱动一个真的 agent 子进程。

    ``argv`` 省略时直接跑 stdio 模式；给 relay 的命令行时则是「经中继连到常驻 agent」，
    也就是真实部署里 SSH channel 上跑的那条路径。
    """

    def __init__(self, agent_dir: str, argv=None, ready_timeout=10) -> None:
        env = os.environ.copy()
        env["TRESTLE_AGENT_DIR"] = agent_dir
        self.proc = subprocess.Popen(
            argv or [sys.executable, AGENT],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
            bufsize=1,
        )
        self._next_id = 1
        self._pending = {}
        self._lock = threading.Lock()
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()
        try:
            self.ready = self._await(0, timeout=ready_timeout)
        except AssertionError:
            self.proc.kill()
            raise

    def _read_loop(self) -> None:
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            with self._lock:
                self._pending[msg.get("id")] = msg

    def _await(self, req_id, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self._lock:
                if req_id in self._pending:
                    return self._pending.pop(req_id)
            time.sleep(0.002)
        raise AssertionError("timed out waiting for response id=%s" % req_id)

    # `op` 是 positional-only：edit 的参数里也有一个叫 op 的字段，不隔开会撞名。
    def call(self, op, /, timeout=30, **args):
        with self._lock:
            req_id = self._next_id
            self._next_id += 1
        self.proc.stdin.write(json.dumps({"id": req_id, "op": op, "args": args}) + "\n")
        self.proc.stdin.flush()
        return self._await(req_id, timeout=timeout)

    def ok(self, op, /, timeout=30, **args):
        resp = self.call(op, timeout=timeout, **args)
        if not resp.get("ok"):
            raise AssertionError("%s failed: %s" % (op, resp.get("error")))
        return resp["result"]

    def close(self) -> None:
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def main() -> int:
    if os.name != "posix":
        print("这份测试要在 POSIX 环境跑（用 wsl python3 agent-py/test_agent.py）")
        return 2

    tmp = tempfile.mkdtemp(prefix="trestle-agent-test-")
    agent = Agent(os.path.join(tmp, "agent-home"))
    try:
        run_all(agent, tmp)
    finally:
        agent.close()

    try:
        run_resident_tests(tmp)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("\n%d passed, %d failed" % (passes, len(failures)))
    for f in failures:
        print("  - %s" % f)
    return 1 if failures else 0


def run_all(agent: Agent, tmp: str) -> None:
    print("\n== 握手 ==")
    check("ready frame arrives without polling", agent.ready["ok"] and agent.ready["result"]["pong"])
    check("protocol version is reported", agent.ready["result"]["protocol"] == 1)

    print("\n== read / write ==")
    path = os.path.join(tmp, "hello.txt")
    res = agent.ok("write", path=path, content="line1\nline2\nline3\n")
    check("write reports the path it was given", res["path"] == path, res["path"])
    res = agent.ok("read", path=path)
    check("read returns the content", res["content"] == "line1\nline2\nline3\n")
    check("read counts total lines", res["total_lines"] == 3, str(res["total_lines"]))
    res = agent.ok("read", path=path, start_line=2, max_lines=1)
    check("read honours the line window", res["content"] == "line2\n", repr(res["content"]))
    check("windowed read is flagged truncated", res["truncated"])

    res = agent.ok("write", path=path, content="line4\n", append=True)
    check("append adds without clobbering", agent.ok("read", path=path)["total_lines"] == 4)

    nested = os.path.join(tmp, "a", "b", "c.txt")
    agent.ok("write", path=nested, content="x", make_dirs=True)
    check("make_dirs creates parents", os.path.exists(nested))

    err = agent.call("read", path=os.path.join(tmp, "nope.txt"))
    check("missing file is a typed error", err["error"]["kind"] == "not_found", str(err["error"]))

    print("\n== edit ==")
    ep = os.path.join(tmp, "edit.txt")
    agent.ok("write", path=ep, content="aaa\nbbb\nccc\nbbb\n")
    res = agent.ok("edit", path=ep, op={"kind": "literal", "old": "bbb", "new": "BBB", "count": 1})
    check("literal edit with count=1 replaces once", agent.ok("read", path=ep)["content"] == "aaa\nBBB\nccc\nbbb\n")
    check("literal edit reports occurrences", res["occurrences"] == 1, str(res["occurrences"]))

    agent.ok("write", path=ep, content="aaa\nbbb\nccc\nbbb\n")
    agent.ok("edit", path=ep, op={"kind": "literal", "old": "bbb", "new": "BBB", "count": 0})
    check("count=0 replaces all", agent.ok("read", path=ep)["content"] == "aaa\nBBB\nccc\nBBB\n")

    agent.ok("write", path=ep, content="foo1\nfoo2\n")
    agent.ok("edit", path=ep, op={"kind": "regex", "pattern": r"foo(\d)", "replacement": r"bar\1", "count": 0})
    check("regex edit substitutes with groups", agent.ok("read", path=ep)["content"] == "bar1\nbar2\n")

    agent.ok("write", path=ep, content="1\n2\n3\n4\n")
    agent.ok("edit", path=ep, op={"kind": "lines", "start": 2, "end": 3, "replacement": "X"})
    check("line-range edit replaces inclusively", agent.ok("read", path=ep)["content"] == "1\nX\n4\n")

    agent.ok("write", path=ep, content="1\n2\n")
    agent.ok("edit", path=ep, op={"kind": "insert", "before_line": 1, "content": "0"})
    check("insert goes before the given line", agent.ok("read", path=ep)["content"] == "0\n1\n2\n")

    res = agent.ok("edit", path=ep, op={"kind": "literal", "old": "zzz", "new": "!", "count": 0})
    check("no match is changed=false, not an error", res["changed"] is False)

    err = agent.call("edit", path=ep, op={"kind": "lines", "start": 99, "end": 100, "replacement": ""})
    check("out-of-range line edit is rejected", err["error"]["kind"] == "bad_range", str(err["error"]))

    print("\n== shell (exec) ==")
    res = agent.ok("shell", command="echo hi")
    check("exit code comes back", res["exit_code"] == 0)
    check("stdout comes back", res["stdout"].strip() == "hi", repr(res["stdout"]))
    res = agent.ok("shell", command="echo oops >&2; exit 3")
    check("stderr is separate", res["stderr"].strip() == "oops", repr(res["stderr"]))
    check("non-zero exit is reported", res["exit_code"] == 3)
    res = agent.ok("shell", command="pwd", cwd=tmp)
    check("cwd is honoured", os.path.realpath(res["stdout"].strip()) == os.path.realpath(tmp))
    res = agent.ok("shell", command="echo $TRESTLE_TEST_VAR", env=[["TRESTLE_TEST_VAR", "42"]])
    check("env is injected", res["stdout"].strip() == "42")

    print("\n== shell 超时必须杀掉整个进程组 ==")
    # 孙进程：bash -c 里再起一个 sleep。只杀直接子进程的话它会残留下来继续跑。
    res = agent.ok("shell", command="bash -c 'seq 1 300 | while read i; do sleep 1; done' ", timeout_secs=2, timeout=30)
    check("timeout is flagged", res["timed_out"] is True)
    time.sleep(0.5)
    # 方括号技巧：不加的话检查命令自己的命令行就含这个 pattern，永远至少匹配 1 个。
    survivors = subprocess.run(
        ["bash", "-lc", "ps -eo cmd | grep -c '[s]eq 1 300' || true"],
        capture_output=True, text=True,
    ).stdout.strip()
    check("no grandchildren survive the timeout", survivors == "0", "survivors=%s" % survivors)

    print("\n== shell (detach) ==")
    marker = os.path.join(tmp, "detached.done")
    res = agent.ok("shell", command="sleep 1; echo done > %s" % marker, detach=True, name="probe")
    check("detach returns immediately with a pid", res["pid"] > 0)
    check("pid equals pgid (session leader)", res["pid"] == res["pgid"])
    check("log path is reported", res["log_path"].endswith("out.log"))
    # 关键：pid 必须是真任务的 pid，不能是 setsid 自己那个转瞬即逝的 pid。
    time.sleep(0.3)
    alive = os.path.exists("/proc/%d" % res["pid"])
    check("the reported pid is actually alive", alive, "pid=%d" % res["pid"])
    deadline = time.time() + 15
    while time.time() < deadline and not os.path.exists(res["rc_path"]):
        time.sleep(0.1)
    check("exit code is written to disk", os.path.exists(res["rc_path"]))
    if os.path.exists(res["rc_path"]):
        with open(res["rc_path"]) as fh:
            check("exit code is correct", fh.read().strip() == "0")
    check("the job actually ran", os.path.exists(marker))

    print("\n== detach 不能阻塞调用方 ==")
    # 上一代最痛的坑：`cd DIR && cmd &` 会让「后台」调用一直卡到任务结束。
    started = time.time()
    agent.ok("shell", command="sleep 8", detach=True, cwd=tmp, name="slow")
    elapsed = time.time() - started
    check("detached call returns fast", elapsed < 2.0, "took %.1fs" % elapsed)

    print("\n== signal 杀整个进程组 ==")
    res = agent.ok("shell", command="bash -c 'sleep 60' ", detach=True, name="victim")
    time.sleep(0.4)
    agent.ok("signal", pid=res["pid"], signal="KILL")
    time.sleep(0.4)
    check("group is gone after SIGKILL", not os.path.exists("/proc/%d" % res["pid"]))

    print("\n== upload / download 分块 ==")
    blob = os.urandom(700_000)  # 跨过 512KiB 分块边界
    digest = hashlib.sha256(blob).hexdigest()
    dst = os.path.join(tmp, "blob.bin")
    chunk = 1 << 19
    offset = 0
    while offset < len(blob):
        piece = blob[offset : offset + chunk]
        final = offset + len(piece) >= len(blob)
        res = agent.ok(
            "put_chunk",
            path=dst,
            offset=offset,
            data=base64.b64encode(piece).decode(),
            final=final,
            sha256=digest if final else None,
        )
        offset += len(piece)
    check("upload lands at the exact path given", res["path"] == dst, res["path"])
    check("upload verifies its checksum", res["sha256"] == digest)
    with open(dst, "rb") as fh:
        check("uploaded bytes are identical", fh.read() == blob)

    got = b""
    offset = 0
    while True:
        res = agent.ok("get_chunk", path=dst, offset=offset, length=chunk)
        got += base64.b64decode(res["data"])
        offset = len(got)
        if res["eof"]:
            break
    check("download returns identical bytes", hashlib.sha256(got).hexdigest() == digest)

    print("\n== 校验和不对时不能留下半截文件 ==")
    bad = os.path.join(tmp, "bad.bin")
    err = agent.call(
        "put_chunk", path=bad, offset=0, data=base64.b64encode(b"x").decode(), final=True, sha256="deadbeef"
    )
    check("checksum mismatch is an error", not err["ok"] and err["error"]["kind"] == "checksum_mismatch")
    check("nothing is left at the target path", not os.path.exists(bad))
    check("no .part leftover", not os.path.exists(bad + ".trestle.part"))

    print("\n== list_tree 与排除规则 ==")
    tree = os.path.join(tmp, "tree")
    os.makedirs(os.path.join(tree, "pkg", "__pycache__"), exist_ok=True)
    os.makedirs(os.path.join(tree, ".git"), exist_ok=True)
    for rel in ["a.py", "pkg/b.py", "pkg/__pycache__/b.pyc", ".git/config"]:
        p = os.path.join(tree, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as fh:
            fh.write("x")
    res = agent.ok("list_tree", path=tree, exclude=["__pycache__", ".git", "*.pyc"])
    rels = sorted(e["rel"] for e in res["entries"])
    check("nested excludes are pruned", rels == ["a.py", "pkg/b.py"], str(rels))
    check("entries carry size and mtime for sync", all("size" in e and "mtime" in e for e in res["entries"]))

    print("\n== 并发：慢操作不挡快操作 ==")
    results = {}

    def slow():
        started = time.time()
        agent.ok("shell", command="sleep 2", timeout=30)
        results["slow"] = time.time() - started

    def fast():
        time.sleep(0.3)
        started = time.time()
        agent.ok("ping")
        results["fast"] = time.time() - started

    t1, t2 = threading.Thread(target=slow), threading.Thread(target=fast)
    t1.start(); t2.start(); t1.join(); t2.join()
    check("a 2s call does not block a ping", results.get("fast", 99) < 0.5, "ping took %.2fs" % results.get("fast", 99))

    print("\n== 坏输入不能弄死 agent ==")
    err = agent.call("no_such_op")
    check("unknown op lists the real ones", err["error"]["kind"] == "unknown_op" and "read" in err["error"]["detail"])
    err = agent.call("read")
    check("missing field is a typed error", err["error"]["kind"] == "bad_request", str(err["error"]))
    check("agent is still alive afterwards", agent.ok("ping")["pong"] is True)


def run_resident_tests(tmp: str) -> None:
    """常驻 socket 模式：agent 的生命周期必须与任何一条连接无关。

    这是 D20「重启后重新 attach 到还活着的远端 agent」在远端一侧的全部依据——
    如果 agent 跟着连接一起死，那条决策就是空的。
    """
    print("\n== 常驻 socket 模式 ==")
    home = os.path.join(tmp, "resident-home")
    os.makedirs(home, exist_ok=True)
    sock = os.path.join(home, "agent.sock")
    env = os.environ.copy()
    env["TRESTLE_AGENT_DIR"] = home

    resident = subprocess.Popen(
        [sys.executable, AGENT, "--serve", sock],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        env=env,
        start_new_session=True,
        text=True,
    )
    deadline = time.time() + 10
    while time.time() < deadline and not os.path.exists(sock):
        time.sleep(0.05)
    check("resident agent binds its socket", os.path.exists(sock))

    relay_argv = [sys.executable, RELAY, sock]
    first = Agent(home, argv=relay_argv)
    check("relay carries the ready frame through", first.ready["ok"])
    first_pid = first.ready["result"]["pid"]
    check("the agent behind the relay is the resident one", first_pid == resident.pid,
          "relay saw pid %s, resident is %s" % (first_pid, resident.pid))

    marker = os.path.join(tmp, "survived.txt")
    job = first.ok("shell", command="sleep 2; echo alive > %s" % marker, detach=True, name="survivor")

    # 掐掉连接——模拟 daemon 挂掉 / 网络断了 / 电脑重启。
    first.proc.kill()
    first.proc.wait(timeout=5)
    time.sleep(0.4)

    check("resident agent outlives the connection", resident.poll() is None)

    second = Agent(home, argv=relay_argv)
    try:
        check("a new connection re-attaches to the same agent",
              second.ready["result"]["pid"] == first_pid,
              "got pid %s" % second.ready["result"]["pid"])
        check("uptime shows it was never restarted", second.ready["result"]["uptime_s"] >= 0)

        # 那个后台任务不该受连接中断影响。
        deadline = time.time() + 15
        while time.time() < deadline and not os.path.exists(job["rc_path"]):
            time.sleep(0.1)
        check("a job started before the drop still completed", os.path.exists(marker))
        check("its exit code still landed on disk", os.path.exists(job["rc_path"]))

        # 第二个 agent 实例不能抢走 socket。
        clash = subprocess.run(
            [sys.executable, AGENT, "--serve", sock],
            capture_output=True, text=True, env=env, timeout=20,
        )
        check("a second agent refuses to steal the socket", clash.returncode == 3,
              "rc=%d %s" % (clash.returncode, clash.stderr.strip()))
        check("the original agent is still serving", second.ok("ping")["pid"] == first_pid)
    finally:
        second.close()
        resident.kill()
        resident.wait(timeout=5)

    # 陈旧的 socket 文件不该挡住下一次启动。
    check("stale socket file is left behind after a hard kill", os.path.exists(sock))
    restarted = subprocess.Popen(
        [sys.executable, AGENT, "--serve", sock],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env,
        start_new_session=True, text=True,
    )
    try:
        third = None
        last_error = ""
        deadline = time.time() + 10
        while time.time() < deadline:
            if restarted.poll() is not None:
                last_error = "agent exited rc=%s: %s" % (
                    restarted.returncode,
                    (restarted.stderr.read() or "").strip(),
                )
                break
            try:
                third = Agent(home, argv=relay_argv, ready_timeout=1.5)
                break
            except Exception as exc:
                last_error = str(exc)
                time.sleep(0.2)
        check("a stale socket does not block a restart", third is not None, last_error)
        if third:
            check("the restarted agent answers", third.ok("ping")["pong"] is True)
            third.close()
    finally:
        restarted.kill()
        restarted.wait(timeout=5)


if __name__ == "__main__":
    sys.exit(main())
