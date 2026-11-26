from __future__ import annotations

import argparse
import asyncio
import importlib.util
import inspect
import json
import os
import signal
import sys
import threading
import traceback
from collections.abc import Awaitable, Callable
from contextlib import suppress
from pathlib import Path
from typing import Any, cast

from . import helpers as helpers_mod
from . import settings as settings_mod
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
        stop_event: asyncio.Event | None = None,
    ) -> None:
        self.entrypoint = entrypoint
        self.plugin_name = plugin_name
        self.api_version = api_version
        self.emit_sink = emit_sink
        self.plugin: Any = None
        self.uninstalling = False
        self.main_task: asyncio.Task[Any] | None = None
        self.main_thread: threading.Thread | None = None
        self._stopping = False
        self._decky_logger: Any = None
        self._stop_event: asyncio.Event | None = stop_event

    def load(self) -> None:
        plugin_dir = os.getenv("DECKY_PLUGIN_DIR", str(self.entrypoint.parent))
        py_modules_dir = Path(plugin_dir) / "py_modules"
        if py_modules_dir.is_dir():
            py_modules_str = str(py_modules_dir)
            if py_modules_str in sys.path:
                sys.path.remove(py_modules_str)
            sys.path.insert(0, py_modules_str)

        decky_mod = build_decky_module(self.plugin_name, self.emit_sink)
        self._decky_logger = decky_mod.logger
        sys.modules["decky"] = decky_mod
        sys.modules["decky_plugin"] = decky_mod
        sys.modules["settings"] = settings_mod
        sys.modules["helpers"] = helpers_mod

        spec = importlib.util.spec_from_file_location("_zerodeck_plugin_main", self.entrypoint)
        if spec is None or spec.loader is None:
            raise RuntimeError(f"cannot load plugin entrypoint: {self.entrypoint}")

        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        plugin_cls = cast(Any, module).Plugin
        self.plugin = plugin_cls() if self.api_version > 0 else plugin_cls

    def _resolve_callable(self, method_name: str) -> tuple[Callable[..., Any], tuple[Any, ...]]:
        method = cast(Callable[..., Any], getattr(self.plugin, method_name))
        if self.api_version == 0:
            raw_attr = inspect.getattr_static(self.plugin, method_name)
            if not isinstance(raw_attr, (staticmethod, classmethod)):
                return method, (self.plugin,)
        return method, ()

    async def _invoke(
        self,
        method_name: str,
        args: tuple[Any, ...] = (),
        kwargs: dict[str, Any] | None = None,
    ) -> Any:
        call_kwargs = kwargs or {}
        callable_fn, prefix = self._resolve_callable(method_name)
        full_args = (*prefix, *args)
        if inspect.iscoroutinefunction(callable_fn):
            result = callable_fn(*full_args, **call_kwargs)
        else:
            result = await asyncio.to_thread(callable_fn, *full_args, **call_kwargs)
        if inspect.isawaitable(result):
            return await result
        return result

    async def start_lifecycle(self) -> None:
        if hasattr(self.plugin, "_migration"):
            try:
                await self._invoke("_migration")
            except Exception:
                if self._decky_logger is not None:
                    self._decky_logger.exception("plugin _migration failed")
                raise
        if hasattr(self.plugin, "_main"):
            callable_fn, prefix = self._resolve_callable("_main")
            if inspect.iscoroutinefunction(callable_fn):
                coro = cast(Awaitable[Any], callable_fn(*prefix))
                if inspect.iscoroutine(coro):
                    self.main_task = asyncio.create_task(coro)
                else:

                    async def _await_task(awaitable: Awaitable[Any]) -> Any:
                        return await awaitable

                    self.main_task = asyncio.create_task(_await_task(coro))
                self.main_task.add_done_callback(self._log_main_task_result)
            else:
                # A synchronous _main cannot be cancelled. Running it in a daemon thread (not the
                # default executor, which asyncio.run waits for) lets the worker exit on shutdown.
                self.main_thread = threading.Thread(
                    target=self._run_sync_main,
                    args=(callable_fn, prefix, asyncio.get_running_loop()),
                    name=f"{self.plugin_name}-main",
                    daemon=True,
                )
                self.main_thread.start()
            await asyncio.sleep(0)

    def _run_sync_main(
        self,
        callable_fn: Callable[..., Any],
        prefix: tuple[Any, ...],
        loop: asyncio.AbstractEventLoop,
    ) -> None:
        try:
            result = callable_fn(*prefix)
            if inspect.isawaitable(result):
                # A plain function returning a coroutine (e.g. `async def _main` behind an
                # ordinary decorator): run it on the event loop like an async _main.
                loop.call_soon_threadsafe(self._adopt_main_awaitable, result)
        except Exception as exc:
            if self._decky_logger is not None:
                self._decky_logger.error(
                    "plugin _main task failed",
                    exc_info=(type(exc), exc, exc.__traceback__),
                )

    def _adopt_main_awaitable(self, awaitable: Awaitable[Any]) -> None:
        if self._stopping:
            if inspect.iscoroutine(awaitable):
                awaitable.close()
            return

        async def _await_main() -> Any:
            return await awaitable

        self.main_task = asyncio.get_running_loop().create_task(_await_main())
        self.main_task.add_done_callback(self._log_main_task_result)

    def _log_main_task_result(self, task: asyncio.Task[Any]) -> None:
        if task.cancelled():
            return
        exc = task.exception()
        if exc is not None and self._decky_logger is not None:
            self._decky_logger.error(
                "plugin _main task failed",
                exc_info=(type(exc), exc, exc.__traceback__),
            )

    async def stop_lifecycle(self) -> None:
        self._stopping = True
        if self.main_task is not None and not self.main_task.done():
            self.main_task.cancel()
            with suppress(asyncio.CancelledError):
                await self.main_task
        self.main_task = None

        if hasattr(self.plugin, "_unload"):
            await self._invoke("_unload")
        if self.uninstalling and hasattr(self.plugin, "_uninstall"):
            await self._invoke("_uninstall")

    async def handle_message(self, raw: str) -> str | None:
        call_id = ""
        response: dict[str, Any] = {
            "type": 1,
            "id": call_id,
            "success": True,
            "res": None,
        }
        try:
            raw_data: Any = json.loads(raw)
            if not isinstance(raw_data, dict):
                raise ValueError("IPC payload must be a JSON object")
            data = cast(dict[str, Any], raw_data)

            if "uninstall" in data:
                self.uninstalling = bool(data.get("uninstall"))
                if self._stop_event is not None:
                    self._stop_event.set()
                return None

            call_id = str(data["id"])
            response["id"] = call_id
            method_name = str(data["method"])
            if method_name.startswith("_"):
                raise PermissionError(f"cannot call private method {method_name}")

            raw_args = data.get("args")
            if data.get("legacy"):
                if self.api_version > 0:
                    raise RuntimeError("legacy kwargs call is disallowed on api_version > 0")
                if raw_args is None:
                    kwargs: dict[str, Any] = {}
                elif isinstance(raw_args, dict):
                    kwargs = cast(dict[str, Any], raw_args)
                else:
                    raise ValueError("legacy args must be a JSON object")
                response["res"] = await self._invoke(method_name, kwargs=kwargs)
            else:
                if self.api_version < 1:
                    raise RuntimeError("api_version >= 1 required for positional calls")
                if raw_args is None:
                    args: list[Any] = []
                elif isinstance(raw_args, list):
                    args = cast(list[Any], raw_args)
                else:
                    raise ValueError("args must be a JSON array")
                response["res"] = await self._invoke(method_name, args=tuple(args))
        except Exception as exc:
            response["success"] = False
            response["res"] = str(exc)
            response["error"] = _error_detail(exc)

        try:
            return json.dumps(response, ensure_ascii=False)
        except (TypeError, ValueError) as exc:
            return json.dumps(
                {
                    "type": 1,
                    "id": response["id"],
                    "success": False,
                    "res": f"plugin result is not JSON serializable: {exc}",
                    "error": _error_detail(exc),
                },
                ensure_ascii=False,
            )


def _error_detail(exc: BaseException) -> dict[str, str]:
    """Describes a failed call for the daemon: exception class name plus formatted traceback."""
    return {
        "name": type(exc).__name__,
        "traceback": "".join(traceback.format_exception(type(exc), exc, exc.__traceback__)),
    }


async def _run_worker(args: argparse.Namespace) -> None:
    if args.fd is not None:
        transport = await JsonLineTransport.from_fd(args.fd)
    elif args.socket_path is not None:
        transport = await JsonLineTransport.from_socket_path(args.socket_path)
    else:
        raise ValueError("either --fd or --socket-path must be provided")

    stop_event = asyncio.Event()
    runner = PluginRunner(
        entrypoint=Path(args.entrypoint),
        plugin_name=args.plugin_name,
        api_version=args.api_version,
        emit_sink=transport.send_json,
        stop_event=stop_event,
    )
    try:
        runner.load()
        await runner.start_lifecycle()

        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            with suppress(NotImplementedError):
                loop.add_signal_handler(sig, stop_event.set)

        serve_task = asyncio.create_task(transport.serve(runner.handle_message))
        stop_task = asyncio.create_task(stop_event.wait())
        done, pending = await asyncio.wait(
            [serve_task, stop_task],
            return_when=asyncio.FIRST_COMPLETED,
        )
        for pending_task in pending:
            pending_task.cancel()
            with suppress(asyncio.CancelledError):
                await pending_task
        for task in done:
            if task is serve_task and not task.cancelled():
                exc = task.exception()
                if exc is not None:
                    raise exc

        await runner.stop_lifecycle()
    finally:
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
