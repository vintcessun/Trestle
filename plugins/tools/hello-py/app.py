"""`hello-py`：验证 Python 也能写 Trestle 插件。

这个插件存在的意义只有一个：证明「Python 口子」不是一张白条。它做的事很少
（在目标机器上跑一条命令回显），但走的是和 Rust 插件**完全一样**的接口——
同一份 `wit/trestle.wit`，同一个 `tool-plugin` 世界。

⚠️ 代价是实打实的，写进这里免得以后有人以为两条路等价：
componentize-py 产出的组件是几十 MB（Rust 插件一百多 KB），实例化也慢得多。
日常插件用 Rust；这条路留给「用 Python 写明显更顺手」的场合。

构建：

    componentize-py -d ../../../wit -w tool-plugin componentize app -o hello-py.wasm
"""

import json

import wit_world
from wit_world.imports import base, host_services as host
from wit_world.imports.types import Error, ErrorKind


def _err(kind: ErrorKind, detail: str, remedy: str = "") -> Error:
    return Error(kind=kind, detail=detail, remedy=remedy)


class WitWorld(wit_world.WitWorld):
    def list_tools(self) -> str:
        return json.dumps(
            [
                {
                    "name": "hello_py",
                    "description": "Python 写的示例工具：在目标机器上跑一条命令并回显。",
                    "input_schema": {
                        "type": "object",
                        # target 必填 —— 没有默认机，这条对 Python 插件同样成立。
                        "required": ["target"],
                        "properties": {
                            "target": {"type": "string", "description": "机器名，必填"},
                            "message": {"type": "string", "default": "hello from python"},
                        },
                    },
                }
            ],
            ensure_ascii=False,
        )

    def call(self, tool: str, args: str) -> str:
        if tool != "hello_py":
            raise _err(ErrorKind.NOT_FOUND, f"unknown tool '{tool}'", "hello_py")

        v = json.loads(args)
        target = v.get("target")
        if not target:
            raise _err(
                ErrorKind.INVALID_REQUEST,
                "this tool needs a `target`; there is no default machine",
                "trestle targets",
            )
        message = v.get("message", "hello from python")

        out = base.call(
            target,
            "shell",
            json.dumps({"command": "echo " + _shell_quote(message), "timeout_secs": 20}),
        )
        host.emit("info", "hello_py", json.dumps({"target": target}))
        said = json.loads(out).get("stdout", "").strip()
        return json.dumps(
            {"target": target, "said": said, "written_in": "python"}, ensure_ascii=False
        )

    def on_tick(self, name: str, payload: str) -> None:
        pass

    def ui_panel(self) -> str:
        return ""

    def config_schema(self) -> str:
        return json.dumps({"type": "object", "properties": {}})


def _shell_quote(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"
