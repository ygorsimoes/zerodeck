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


if __name__ == "__main__":
    unittest.main()
