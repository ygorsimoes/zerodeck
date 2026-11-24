from __future__ import annotations

import argparse
import asyncio
import importlib.util
import json
import os
import signal
import sys
from pathlib import Path
from typing import Any, Optional

from .decky_api import EmitCallback, build_decky_module
from .ipc import JsonLineTransport


class PluginRunner:
    """Loads a Decky `main.py` plugin and dispatches v0/v1 JSON IPC calls."""

    def __init__(
        self,
        entrypoint: Path,
        plugin_name: str,
        api_version: int,
        emit_sink: EmitCallback,
    ) -> None:
        self.entrypoint = entrypoint
        self.plugin_name = plugin_name
        self.api_version = api_version
        self.emit_sink = emit_sink
        self.plugin: Any = None
        self.uninstalling = False

    def load(self) -> None:
        plugin_dir = os.getenv("DECKY_PLUGIN_DIR", str(self.entrypoint.parent))
        py_modules_dir = str(Path(plugin_dir) / "py_modules")
        if py_modules_dir not in sys.path:
            sys.path.append(py_modules_dir)

        decky_mod = build_decky_module(self.plugin_name, self.emit_sink)
        sys.modules["decky"] = decky_mod
        sys.modules["decky_plugin"] = decky_mod

        spec = importlib.util.spec_from_file_location("_zerodeck_plugin_main", self.entrypoint)
        if spec is None or spec.loader is None:
            raise RuntimeError(f"cannot load plugin entrypoint: {self.entrypoint}")

        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        plugin_cls = getattr(module, "Plugin")
        self.plugin = plugin_cls() if self.api_version > 0 else plugin_cls

    async def start_lifecycle(self) -> None:
        if hasattr(self.plugin, "_migration"):
            if self.api_version > 0:
                await self.plugin._migration()
            else:
                await self.plugin._migration(self.plugin)
        if hasattr(self.plugin, "_main"):
            if self.api_version > 0:
                asyncio.create_task(self.plugin._main())
            else:
                asyncio.create_task(self.plugin._main(self.plugin))

    async def stop_lifecycle(self) -> None:
        if hasattr(self.plugin, "_unload"):
            if self.api_version > 0:
                await self.plugin._unload()
            else:
                await self.plugin._unload(self.plugin)
        if self.uninstalling and hasattr(self.plugin, "_uninstall"):
            if self.api_version > 0:
                await self.plugin._uninstall()
            else:
                await self.plugin._uninstall(self.plugin)

    async def handle_message(self, raw: str) -> Optional[str]:
        data = json.loads(raw)
        if "uninstall" in data:
            self.uninstalling = bool(data.get("uninstall"))
            return None

        call_id = str(data["id"])
        response: dict[str, Any] = {
            "type": 1,
            "id": call_id,
            "success": True,
            "res": None,
        }
        try:
            method_name = str(data["method"])
            if method_name.startswith("_"):
                raise PermissionError(f"cannot call private method {method_name}")
            method = getattr(self.plugin, method_name)

            if data.get("legacy"):
                if self.api_version > 0:
                    raise RuntimeError("legacy kwargs call is disallowed on api_version > 0")
                kwargs = data.get("args") or {}
                response["res"] = await method(self.plugin, **kwargs)
            else:
                if self.api_version < 1:
                    raise RuntimeError("api_version >= 1 required for positional calls")
                args = data.get("args") or []
                response["res"] = await method(*args)
        except Exception as exc:  # noqa: BLE001
            response["success"] = False
            response["res"] = str(exc)

        return json.dumps(response, ensure_ascii=False)


async def _run_worker(args: argparse.Namespace) -> None:
    if args.fd is not None:
        transport = await JsonLineTransport.from_fd(args.fd)
    elif args.socket_path is not None:
        transport = await JsonLineTransport.from_socket_path(args.socket_path)
    else:
        raise ValueError("either --fd or --socket-path must be provided")

    runner = PluginRunner(
        entrypoint=Path(args.entrypoint),
        plugin_name=args.plugin_name,
        api_version=args.api_version,
        emit_sink=transport.send_json,
    )
    runner.load()
    await runner.start_lifecycle()

    stop_event = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, stop_event.set)
        except NotImplementedError:
            pass

    serve_task = asyncio.create_task(transport.serve(runner.handle_message))
    stop_task = asyncio.create_task(stop_event.wait())
    done, pending = await asyncio.wait(
        [serve_task, stop_task],
        return_when=asyncio.FIRST_COMPLETED,
    )
    for task in pending:
        task.cancel()
    for task in done:
        if task is serve_task and not task.cancelled():
            exc = task.exception()
            if exc is not None:
                raise exc

    await runner.stop_lifecycle()
    await transport.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="ZeroDeck isolated Python plugin worker")
    parser.add_argument("--entrypoint", required=True)
    parser.add_argument("--plugin-name", required=True)
    parser.add_argument("--api-version", type=int, default=0)
    parser.add_argument("--fd", type=int, default=None)
    parser.add_argument("--socket-path", default=None)
    parsed = parser.parse_args()
    asyncio.run(_run_worker(parsed))


if __name__ == "__main__":
    main()
