#!/usr/bin/env python3
"""把 stdin/stdout 与 agent 的 Unix socket 对接起来。

SSH channel 上跑的就是这个：它不认识协议，只搬字节。之所以要它，是因为 agent 常驻在
socket 上（生命周期与任何一条 SSH 连接无关），而 SSH channel 只有 stdio。

**只用标准库、不经 uv**——它要尽可能快地起来，因为每条新连接都要跑一次。

    python3 relay.py ~/.trestle/agent.sock
"""

import os
import socket
import sys
import threading

BUF = 65536


def main() -> int:
    if len(sys.argv) != 2:
        sys.stderr.write("usage: relay.py <socket-path>\n")
        return 2

    path = os.path.expanduser(sys.argv[1])
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(path)
    except OSError as exc:
        # 主机侧靠这个退出码区分「agent 没起来」与「别的错」，进而决定要不要部署。
        sys.stderr.write("trestle-relay: cannot connect to %s: %s\n" % (path, exc))
        return 4

    def pump_up() -> None:
        """stdin → socket"""
        try:
            while True:
                data = os.read(0, BUF)
                if not data:
                    break
                sock.sendall(data)
        except OSError:
            pass
        finally:
            try:
                sock.shutdown(socket.SHUT_WR)
            except OSError:
                pass

    threading.Thread(target=pump_up, daemon=True).start()

    # socket → stdout，在主线程跑：它一结束进程就该退出。
    try:
        while True:
            data = sock.recv(BUF)
            if not data:
                break
            os.write(1, data)
    except OSError:
        pass
    finally:
        try:
            sock.close()
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
