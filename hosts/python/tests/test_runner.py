from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

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


if __name__ == "__main__":
    unittest.main()
