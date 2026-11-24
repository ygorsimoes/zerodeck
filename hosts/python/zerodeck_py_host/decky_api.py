from __future__ import annotations

import logging
import os
import shutil
import time
import types
from pathlib import Path
from typing import Any, Awaitable, Callable

EmitCallback = Callable[[dict[str, Any]], Awaitable[None]]


def _migrate_any(target_dir: str, *files_or_directories: str) -> dict[str, str]:
    target = Path(target_dir)
    target.mkdir(parents=True, exist_ok=True)
    mapping: dict[str, str] = {}

    for raw_item in files_or_directories:
        src = Path(raw_item)
        if not src.exists():
            mapping[raw_item] = ""
            continue
        if src.is_dir():
            shutil.copytree(src, target, dirs_exist_ok=True)
            shutil.rmtree(src)
            mapping[raw_item] = str(target)
        else:
            dest = target / src.name
            shutil.copy2(src, dest)
            src.unlink()
            mapping[raw_item] = str(dest)

    return mapping


def _build_logger(plugin_name: str, log_dir: str) -> tuple[logging.Logger, str]:
    log_file = ""
    logger = logging.getLogger(f"zerodeck.plugin.{plugin_name}")
    logger.handlers.clear()
    logger.setLevel(logging.INFO)
    logger.propagate = False

    formatter = logging.Formatter("[%(asctime)s][%(levelname)s]: %(message)s")
    stream_handler = logging.StreamHandler()
    stream_handler.setFormatter(formatter)
    logger.addHandler(stream_handler)

    if log_dir:
        log_path = Path(log_dir)
        log_path.mkdir(parents=True, exist_ok=True)
        existing_logs = sorted(
            (p for p in log_path.iterdir() if p.is_file() and p.name.endswith(".log")),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
        for stale in existing_logs[4:]:
            try:
                stale.unlink()
            except OSError:
                pass

        timestamp = time.strftime("%Y-%m-%d %H.%M.%S")
        target_log = log_path / f"{timestamp}.log"
        log_file = str(target_log)
        file_handler = logging.FileHandler(target_log, encoding="utf-8")
        file_handler.setFormatter(formatter)
        logger.addHandler(file_handler)

    return logger, log_file


def build_decky_module(plugin_name: str, emit_sink: EmitCallback) -> types.ModuleType:
    """Builds a `decky` / `decky_plugin` module populated from the active worker environment."""
    mod = types.ModuleType("decky")
    settings_dir = os.getenv("DECKY_PLUGIN_SETTINGS_DIR", "")
    runtime_dir = os.getenv("DECKY_PLUGIN_RUNTIME_DIR", "")
    log_dir = os.getenv("DECKY_PLUGIN_LOG_DIR", "")
    logger, log_file = _build_logger(plugin_name, log_dir)

    async def emit(event: str, *args: Any) -> None:
        await emit_sink({"type": 2, "event": event, "args": list(args)})

    def migrate_any(target_dir: str, *files_or_directories: str) -> dict[str, str]:
        return _migrate_any(target_dir, *files_or_directories)

    def migrate_settings(*files_or_directories: str) -> dict[str, str]:
        return _migrate_any(settings_dir, *files_or_directories)

    def migrate_runtime(*files_or_directories: str) -> dict[str, str]:
        return _migrate_any(runtime_dir, *files_or_directories)

    def migrate_logs(*files_or_directories: str) -> dict[str, str]:
        return _migrate_any(log_dir, *files_or_directories)

    mod.__dict__.update(
        {
            "__version__": "1.0.0",
            "HOME": os.getenv("HOME", ""),
            "USER": os.getenv("USER", ""),
            "DECKY_VERSION": os.getenv("DECKY_VERSION", ""),
            "DECKY_USER": os.getenv("DECKY_USER", ""),
            "DECKY_USER_HOME": os.getenv("DECKY_USER_HOME", ""),
            "DECKY_HOME": os.getenv("DECKY_HOME", ""),
            "DECKY_PLUGIN_SETTINGS_DIR": settings_dir,
            "DECKY_PLUGIN_RUNTIME_DIR": runtime_dir,
            "DECKY_PLUGIN_LOG_DIR": log_dir,
            "DECKY_PLUGIN_LOG": log_file,
            "DECKY_PLUGIN_DIR": os.getenv("DECKY_PLUGIN_DIR", ""),
            "DECKY_PLUGIN_NAME": os.getenv("DECKY_PLUGIN_NAME", plugin_name),
            "DECKY_PLUGIN_VERSION": os.getenv("DECKY_PLUGIN_VERSION", ""),
            "DECKY_PLUGIN_AUTHOR": os.getenv("DECKY_PLUGIN_AUTHOR", ""),
            "logger": logger,
            "emit": emit,
            "migrate_any": migrate_any,
            "migrate_settings": migrate_settings,
            "migrate_runtime": migrate_runtime,
            "migrate_logs": migrate_logs,
        }
    )
    return mod
