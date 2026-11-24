from __future__ import annotations

import asyncio
import json
import os
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zerodeck_py_host.ipc import JsonLineTransport
from zerodeck_py_host.runner import PluginRunner
from zerodeck_py_host.settings import SettingsManager


class PluginRunnerTests(unittest.IsolatedAsyncioTestCase):
    async def test_api_version_0_legacy_kwargs_dispatch_and_decky_plugin_alias(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "legacy_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import decky_plugin",
                        "",
                        "class Plugin:",
                        "    async def greet(self, name='world'):",
                        "        return f'hello {name} from {decky_plugin.DECKY_PLUGIN_NAME}'",
                    ]
                ),
                encoding="utf-8",
            )

            emitted: list[dict[str, object]] = []

            async def emit_sink(payload: dict[str, object]) -> None:
                emitted.append(payload)

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
                "DECKY_PLUGIN_SETTINGS_DIR": str(root / "settings"),
                "DECKY_PLUGIN_RUNTIME_DIR": str(root / "data"),
                "DECKY_PLUGIN_NAME": "LegacyDemo",
                "DECKY_PLUGIN_AUTHOR": "Tester",
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(
                    entrypoint=entrypoint,
                    plugin_name="LegacyDemo",
                    api_version=0,
                    emit_sink=emit_sink,
                )
                runner.load()
                raw_reply = await runner.handle_message(
                    json.dumps(
                        {
                            "type": 0,
                            "id": "call-0",
                            "method": "greet",
                            "args": {"name": "deck"},
                            "legacy": True,
                        }
                    )
                )

            assert raw_reply is not None
            reply = json.loads(raw_reply)
            self.assertEqual(
                reply,
                {
                    "type": 1,
                    "id": "call-0",
                    "success": True,
                    "res": "hello deck from LegacyDemo",
                },
            )
            self.assertEqual(emitted, [])

    async def test_api_version_1_positional_args_and_emit_event(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "modern_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import decky",
                        "",
                        "class Plugin:",
                        "    async def add_and_emit(self, a, b):",
                        "        await decky.emit('tick', a + b)",
                        "        return a + b",
                    ]
                ),
                encoding="utf-8",
            )

            emitted: list[dict[str, object]] = []

            async def emit_sink(payload: dict[str, object]) -> None:
                emitted.append(payload)

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
                "DECKY_PLUGIN_SETTINGS_DIR": str(root / "settings"),
                "DECKY_PLUGIN_RUNTIME_DIR": str(root / "data"),
                "DECKY_PLUGIN_NAME": "ModernDemo",
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(
                    entrypoint=entrypoint,
                    plugin_name="ModernDemo",
                    api_version=1,
                    emit_sink=emit_sink,
                )
                runner.load()
                raw_reply = await runner.handle_message(
                    json.dumps(
                        {
                            "type": 0,
                            "id": "call-1",
                            "method": "add_and_emit",
                            "args": [40, 2],
                            "legacy": False,
                        }
                    )
                )

            assert raw_reply is not None
            reply = json.loads(raw_reply)
            self.assertEqual(
                reply,
                {"type": 1, "id": "call-1", "success": True, "res": 42},
            )
            self.assertEqual(
                emitted,
                [{"type": 2, "event": "tick", "args": [42]}],
            )

    async def test_py_modules_is_added_to_sys_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "mod_plugin"
            py_mods = plugin_dir / "py_modules"
            log_dir = root / "logs"
            py_mods.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            (py_mods / "helper_mod.py").write_text("VALUE = 'from_py_modules'\n", encoding="utf-8")
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import helper_mod",
                        "",
                        "class Plugin:",
                        "    async def read_helper(self):",
                        "        return helper_mod.VALUE",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(
                    entrypoint=entrypoint,
                    plugin_name="ModDemo",
                    api_version=1,
                    emit_sink=emit_sink,
                )
                runner.load()
                raw_reply = await runner.handle_message(
                    json.dumps(
                        {
                            "type": 0,
                            "id": "call-2",
                            "method": "read_helper",
                            "args": [],
                        }
                    )
                )

            assert raw_reply is not None
            reply = json.loads(raw_reply)
            self.assertEqual(reply["res"], "from_py_modules")
            self.assertTrue(reply["success"])

    async def test_python_module_entrypoint_help_exits_zero(self) -> None:
        env = dict(os.environ)
        env["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])
        proc = subprocess.run(
            [sys.executable, "-m", "zerodeck_py_host", "--help"],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=proc.stderr)

    async def test_sync_method_malformed_json_and_lifecycle_task_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "sync_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import asyncio",
                        "",
                        "class Plugin:",
                        "    def __init__(self):",
                        "        self.cancelled = False",
                        "",
                        "    async def _main(self):",
                        "        try:",
                        "            await asyncio.sleep(60)",
                        "        except asyncio.CancelledError:",
                        "            self.cancelled = True",
                        "            raise",
                        "",
                        "    def ping_sync(self):",
                        "        return 'pong-sync'",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(
                    entrypoint=entrypoint,
                    plugin_name="SyncDemo",
                    api_version=1,
                    emit_sink=emit_sink,
                )
                runner.load()
                await runner.start_lifecycle()
                self.assertIsNotNone(runner.main_task)

                sync_reply_raw = await runner.handle_message(
                    json.dumps({"type": 0, "id": "sync-1", "method": "ping_sync", "args": []})
                )
                assert sync_reply_raw is not None
                sync_reply = json.loads(sync_reply_raw)
                self.assertEqual(sync_reply["res"], "pong-sync")
                self.assertTrue(sync_reply["success"])

                bad_reply_raw = await runner.handle_message("{not valid json")
                assert bad_reply_raw is not None
                bad_reply = json.loads(bad_reply_raw)
                self.assertFalse(bad_reply["success"])

                await runner.stop_lifecycle()
                self.assertTrue(runner.plugin.cancelled)

    async def test_non_json_serializable_result_returns_error_reply(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "set_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "class Plugin:",
                        "    async def returns_set(self):",
                        "        return {1, 2}",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(
                    entrypoint=entrypoint,
                    plugin_name="SetDemo",
                    api_version=1,
                    emit_sink=emit_sink,
                )
                runner.load()
                raw_reply = await runner.handle_message(
                    json.dumps({"type": 0, "id": "set-1", "method": "returns_set", "args": []})
                )

            assert raw_reply is not None
            reply = json.loads(raw_reply)
            self.assertEqual(reply["id"], "set-1")
            self.assertFalse(reply["success"])
            self.assertIn("JSON", reply["res"])

    async def test_failed_call_reports_exception_type_and_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "raising_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "class Plugin:",
                        "    async def explode(self):",
                        "        raise ValueError('bad input')",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(
                    entrypoint=entrypoint,
                    plugin_name="RaisingDemo",
                    api_version=1,
                    emit_sink=emit_sink,
                )
                runner.load()
                raw_reply = await runner.handle_message(
                    json.dumps({"type": 0, "id": "boom-1", "method": "explode", "args": []})
                )
                raw_private = await runner.handle_message(
                    json.dumps({"type": 0, "id": "boom-2", "method": "_main", "args": []})
                )

            assert raw_reply is not None and raw_private is not None
            reply = json.loads(raw_reply)
            self.assertEqual(reply["id"], "boom-1")
            self.assertFalse(reply["success"])
            self.assertEqual(reply["res"], "bad input")
            self.assertEqual(reply["error"]["name"], "ValueError")
            self.assertIn("Traceback", reply["error"]["traceback"])
            self.assertIn("explode", reply["error"]["traceback"])

            private = json.loads(raw_private)
            self.assertEqual(private["error"]["name"], "PermissionError")

    async def test_json_line_transport_dispatches_messages_concurrently(self) -> None:
        left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            transport = await JsonLineTransport.from_fd(left.detach())

            fast_seen = asyncio.Event()

            async def on_msg(line: str) -> str | None:
                if json.loads(line)["id"] == "slow":
                    # Only completes if the "fast" call is dispatched while this one is pending.
                    await asyncio.wait_for(fast_seen.wait(), timeout=2)
                    return json.dumps({"id": "slow"})
                fast_seen.set()
                return json.dumps({"id": "fast"})

            def send_and_close() -> None:
                right.setblocking(True)
                right.sendall(b'{"id": "slow"}\n{"id": "fast"}\n')
                right.shutdown(socket.SHUT_WR)

            await asyncio.gather(asyncio.to_thread(send_and_close), transport.serve(on_msg))
            await transport.close()

            right.setblocking(True)
            replies = right.recv(4096).decode("utf-8").splitlines()
            self.assertEqual([json.loads(r)["id"] for r in replies], ["fast", "slow"])
        finally:
            right.close()

    async def test_json_line_transport_closes_the_inherited_fd(self) -> None:
        left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            inherited_fd = left.detach()
            transport = await JsonLineTransport.from_fd(inherited_fd)
            await transport.close()

            # The transport owns the inherited descriptor, so closing it must not leave it open.
            with self.assertRaises(OSError):
                os.fstat(inherited_fd)
        finally:
            right.close()

    async def test_json_line_transport_handles_payloads_above_64kib(self) -> None:
        left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            transport = await JsonLineTransport.from_fd(left.detach())

            large_text = "x" * 100_000
            payload = (json.dumps({"type": 0, "id": "big", "data": large_text}) + "\n").encode(
                "utf-8"
            )

            def send_and_close() -> None:
                right.setblocking(True)
                right.sendall(payload)
                right.shutdown(socket.SHUT_WR)

            received: list[str] = []

            async def on_msg(line: str) -> str | None:
                received.append(line)
                return None

            await asyncio.gather(
                asyncio.to_thread(send_and_close),
                transport.serve(on_msg),
            )
            await transport.close()
            self.assertEqual(len(received), 1)
            self.assertEqual(len(json.loads(received[0])["data"]), 100_000)
        finally:
            right.close()

    async def test_settings_module_roundtrips_atomically_and_preserves_corrupt_json(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "settings_plugin"
            settings_dir = root / "settings"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            settings_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)

            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "from settings import SettingsManager",
                        "",
                        "class Plugin:",
                        "    def __init__(self):",
                        "        self.cfg = SettingsManager(",
                        "            'settings', default_settings={'flag': True}",
                        "        )",
                        "",
                        "    async def get_flag(self):",
                        "        return self.cfg.getSetting('flag', False)",
                        "",
                        "    async def set_flag(self, val):",
                        "        self.cfg.setSetting('flag', val)",
                        "        return self.cfg.getSetting('flag')",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_SETTINGS_DIR": str(settings_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(entrypoint, "SettingsDemo", 1, emit_sink)
                runner.load()
                r1 = await runner.handle_message(
                    json.dumps({"type": 0, "id": "s1", "method": "get_flag", "args": []})
                )
                r2 = await runner.handle_message(
                    json.dumps({"type": 0, "id": "s2", "method": "set_flag", "args": [False]})
                )

                assert r1 is not None and r2 is not None
                self.assertTrue(json.loads(r1)["res"])
                self.assertFalse(json.loads(r2)["res"])

                on_disk = json.loads((settings_dir / "settings.json").read_text(encoding="utf-8"))
                self.assertEqual(on_disk, {"flag": False})

                # Corrupt JSON must not be silently truncated or overwritten by read().
                corrupt_path = settings_dir / "corrupt.json"
                corrupt_path.write_text("{broken json", encoding="utf-8")
                self.assertIs(sys.modules["settings"].SettingsManager, SettingsManager)

                corrupt_mgr = SettingsManager("corrupt", settings_directory=settings_dir)
                self.assertEqual(corrupt_mgr.settings, {})
                self.assertEqual(corrupt_path.read_text(encoding="utf-8"), "{broken json")

    async def test_helpers_module_exposes_ssl_context_homebrew_path_and_user(self) -> None:
        import ssl

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "helpers_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)

            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "from helpers import get_homebrew_path, get_ssl_context, get_user",
                        "",
                        "class Plugin:",
                        "    async def info(self):",
                        "        ctx = get_ssl_context()",
                        "        return {",
                        "            'is_ctx': hasattr(ctx, 'wrap_socket'),",
                        "            'home': get_homebrew_path(),",
                        "            'user': get_user(),",
                        "        }",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
                "DECKY_HOME": "/home/deck/homebrew",
                "DECKY_USER": "deck",
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(entrypoint, "HelpersDemo", 1, emit_sink)
                runner.load()
                raw_reply = await runner.handle_message(
                    json.dumps({"type": 0, "id": "h1", "method": "info", "args": []})
                )

            assert raw_reply is not None
            reply = json.loads(raw_reply)
            self.assertTrue(reply["success"], msg=reply)
            self.assertTrue(reply["res"]["is_ctx"])
            self.assertEqual(reply["res"]["home"], "/home/deck/homebrew")
            self.assertEqual(reply["res"]["user"], "deck")
            self.assertIsInstance(
                sys.modules["helpers"].get_ssl_context(),
                ssl.SSLContext,
            )

    async def test_api_version_0_supports_classmethod_staticmethod_and_plain_methods(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "v0_methods"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)

            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "class Plugin:",
                        "    PREFIX = 'ok'",
                        "",
                        "    async def _main(self):",
                        "        Plugin.MAIN_CALLED = True",
                        "",
                        "    @classmethod",
                        "    async def cls_method(cls, value='x'):",
                        "        return f'{cls.PREFIX}:{value}'",
                        "",
                        "    @staticmethod",
                        "    async def static_method(value='y'):",
                        "        return f'static:{value}'",
                        "",
                        "    async def plain_method(self, value='z'):",
                        "        return f'{self.PREFIX}-plain:{value}'",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(entrypoint, "V0Methods", 0, emit_sink)
                runner.load()
                await runner.start_lifecycle()
                c = await runner.handle_message(
                    json.dumps(
                        {
                            "type": 0,
                            "id": "c1",
                            "method": "cls_method",
                            "args": {"value": "1"},
                            "legacy": True,
                        }
                    )
                )
                s = await runner.handle_message(
                    json.dumps(
                        {
                            "type": 0,
                            "id": "s1",
                            "method": "static_method",
                            "args": {"value": "2"},
                            "legacy": True,
                        }
                    )
                )
                p = await runner.handle_message(
                    json.dumps(
                        {
                            "type": 0,
                            "id": "p1",
                            "method": "plain_method",
                            "args": {"value": "3"},
                            "legacy": True,
                        }
                    )
                )

            assert c is not None and s is not None and p is not None
            self.assertEqual(json.loads(c)["res"], "ok:1")
            self.assertEqual(json.loads(s)["res"], "static:2")
            self.assertEqual(json.loads(p)["res"], "ok-plain:3")

    async def test_blocking_sync_method_does_not_block_concurrent_calls_or_emit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "blocking_sync"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)

            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import threading",
                        "import time",
                        "import decky",
                        "",
                        "class Plugin:",
                        "    def __init__(self):",
                        "        self.started = threading.Event()",
                        "        self.release = threading.Event()",
                        "",
                        "    def slow_sync(self):",
                        "        self.started.set()",
                        "        assert self.release.wait(timeout=2.0), 'fast call never ran'",
                        "        return 'done-sync'",
                        "",
                        "    async def fast_async(self):",
                        "        self.release.set()",
                        "        await decky.emit('fast', 1)",
                        "        return 'done-fast'",
                    ]
                ),
                encoding="utf-8",
            )

            emitted: list[dict[str, object]] = []

            async def emit_sink(payload: dict[str, object]) -> None:
                emitted.append(payload)

            env = {
                "DECKY_PLUGIN_DIR": str(plugin_dir),
                "DECKY_PLUGIN_LOG_DIR": str(log_dir),
            }
            with patch.dict(os.environ, env, clear=False):
                runner = PluginRunner(entrypoint, "SyncThreadDemo", 1, emit_sink)
                runner.load()

                slow_task = asyncio.create_task(
                    runner.handle_message(
                        json.dumps({"type": 0, "id": "slow", "method": "slow_sync", "args": []})
                    )
                )
                # Wait until slow_sync is actively executing inside a worker thread.
                while not runner.plugin.started.is_set():
                    await asyncio.sleep(0.01)

                fast_raw = await asyncio.wait_for(
                    runner.handle_message(
                        json.dumps({"type": 0, "id": "fast", "method": "fast_async", "args": []})
                    ),
                    timeout=1.0,
                )
                slow_raw = await asyncio.wait_for(slow_task, timeout=1.0)

            assert fast_raw is not None and slow_raw is not None
            self.assertEqual(json.loads(fast_raw)["res"], "done-fast")
            self.assertEqual(json.loads(slow_raw)["res"], "done-sync")
            self.assertEqual(emitted, [{"type": 2, "event": "fast", "args": [1]}])

    async def test_py_modules_precedes_existing_sys_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "precedence_plugin"
            py_mods = plugin_dir / "py_modules"
            other_dir = root / "other"
            log_dir = root / "logs"
            py_mods.mkdir(parents=True)
            other_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            (other_dir / "shared_dep.py").write_text("SOURCE = 'other'\n", encoding="utf-8")
            (py_mods / "shared_dep.py").write_text("SOURCE = 'py_modules'\n", encoding="utf-8")
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import shared_dep",
                        "class Plugin:",
                        "    async def src(self):",
                        "        return shared_dep.SOURCE",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            sys.path.insert(0, str(other_dir))
            try:
                with patch.dict(
                    os.environ,
                    {"DECKY_PLUGIN_DIR": str(plugin_dir), "DECKY_PLUGIN_LOG_DIR": str(log_dir)},
                    clear=False,
                ):
                    runner = PluginRunner(entrypoint, "PrecedenceDemo", 1, emit_sink)
                    runner.load()
                    raw = await runner.handle_message(
                        json.dumps({"type": 0, "id": "p", "method": "src", "args": []})
                    )
                assert raw is not None
                self.assertEqual(json.loads(raw)["res"], "py_modules")
            finally:
                if str(other_dir) in sys.path:
                    sys.path.remove(str(other_dir))
                sys.modules.pop("shared_dep", None)

    async def test_migration_and_main_failures_are_logged_to_decky_logger(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "fail_lifecycle"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "class Plugin:",
                        "    async def _migration(self):",
                        "        raise RuntimeError('migration boom')",
                    ]
                ),
                encoding="utf-8",
            )

            async def emit_sink(_payload: dict[str, object]) -> None:
                return None

            with patch.dict(
                os.environ,
                {"DECKY_PLUGIN_DIR": str(plugin_dir), "DECKY_PLUGIN_LOG_DIR": str(log_dir)},
                clear=False,
            ):
                runner = PluginRunner(entrypoint, "FailLifecycle", 1, emit_sink)
                runner.load()
                with self.assertRaises(RuntimeError):
                    await runner.start_lifecycle()

                log_files = list(log_dir.glob("*.log"))
                self.assertTrue(log_files, "expected plugin log file")
                log_text = log_files[0].read_text(encoding="utf-8")
                self.assertIn("migration boom", log_text)

                # Now test _main background task exception logging.
                entrypoint.write_text(
                    "\n".join(
                        [
                            "class Plugin:",
                            "    async def _main(self):",
                            "        raise ValueError('main boom')",
                        ]
                    ),
                    encoding="utf-8",
                )
                runner2 = PluginRunner(entrypoint, "FailMain", 1, emit_sink)
                runner2.load()
                await runner2.start_lifecycle()
                await asyncio.sleep(0.05)

                all_logs = "\n".join(p.read_text(encoding="utf-8") for p in log_dir.glob("*.log"))
                self.assertIn("main boom", all_logs)

    async def test_control_shutdown_does_not_wait_for_blocked_rpc(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plugin_dir = root / "blocked_rpc_plugin"
            log_dir = root / "logs"
            plugin_dir.mkdir(parents=True)
            log_dir.mkdir(parents=True)
            flag_file = root / "unloaded.txt"
            entrypoint = plugin_dir / "main.py"
            entrypoint.write_text(
                "\n".join(
                    [
                        "import asyncio",
                        "from pathlib import Path",
                        "",
                        "class Plugin:",
                        "    async def stuck(self):",
                        "        await asyncio.sleep(60)",
                        "        return 'never'",
                        "",
                        "    async def _unload(self):",
                        f"        Path({str(flag_file)!r}).write_text(",
                        "            'unloaded', encoding='utf-8'",
                        "        )",
                    ]
                ),
                encoding="utf-8",
            )

            left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
            proc: subprocess.Popen[bytes] | None = None
            try:
                env = dict(os.environ)
                env["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])
                env["DECKY_PLUGIN_DIR"] = str(plugin_dir)
                env["DECKY_PLUGIN_LOG_DIR"] = str(log_dir)
                os.set_inheritable(left.fileno(), True)
                proc = subprocess.Popen(
                    [
                        sys.executable,
                        "-m",
                        "zerodeck_py_host",
                        "--entrypoint",
                        str(entrypoint),
                        "--plugin-name",
                        "BlockedRpc",
                        "--api-version",
                        "1",
                        "--fd",
                        str(left.fileno()),
                    ],
                    pass_fds=(left.fileno(),),
                    env=env,
                )
                left.close()

                right.setblocking(True)
                call_line = json.dumps(
                    {
                        "type": 0,
                        "id": "stuck-1",
                        "method": "stuck",
                        "args": [],
                        "legacy": False,
                    }
                ).encode("utf-8") + b"\n"
                right.sendall(call_line)
                await asyncio.sleep(0.1)
                right.sendall(b'{"uninstall": false}\n')
                right.shutdown(socket.SHUT_WR)

                rc = await asyncio.wait_for(asyncio.to_thread(proc.wait), timeout=2.0)
                self.assertEqual(rc, 0)
                self.assertEqual(flag_file.read_text(encoding="utf-8"), "unloaded")
            finally:
                if proc is not None and proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=2)
                right.close()


if __name__ == "__main__":
    unittest.main()
