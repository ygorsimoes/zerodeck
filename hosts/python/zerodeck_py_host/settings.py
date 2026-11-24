from __future__ import annotations

import json
import logging
import os
import threading
import uuid
from pathlib import Path
from typing import Any, cast

_LOG = logging.getLogger("zerodeck.settings")


class SettingsManager:
    """Clean-room JSON settings manager compatible with Decky's `settings.SettingsManager`."""

    def __init__(
        self,
        name: str,
        settings_directory: str | Path | None = None,
        default_settings: dict[str, Any] | None = None,
    ) -> None:
        base_dir = (
            Path(settings_directory)
            if settings_directory is not None
            else Path(os.getenv("DECKY_PLUGIN_SETTINGS_DIR", "."))
        )
        self.name = name
        self.settings_directory = base_dir
        self.path = base_dir / f"{name}.json"
        self._defaults: dict[str, Any] = dict(default_settings) if default_settings else {}
        self._lock = threading.Lock()
        self.settings: dict[str, Any] = {}
        self.read()

    def read(self) -> dict[str, Any]:
        """Reads settings from disk, preserving corrupt files without overwriting them."""
        with self._lock:
            if not self.path.exists():
                self.settings = dict(self._defaults)
                return self.settings

            try:
                raw = self.path.read_text(encoding="utf-8")
                parsed: Any = json.loads(raw)
            except (OSError, ValueError) as exc:
                _LOG.error("failed to read settings file %s: %s", self.path, exc)
                self.settings = dict(self._defaults)
                return self.settings

            if not isinstance(parsed, dict):
                _LOG.error("settings file %s must contain a JSON object", self.path)
                self.settings = dict(self._defaults)
                return self.settings

            merged = dict(self._defaults)
            merged.update(cast(dict[str, Any], parsed))
            self.settings = merged
            return self.settings

    def commit(self) -> None:
        """Atomically writes current settings to disk via sibling temp file, fsync, and replace."""
        with self._lock:
            self.settings_directory.mkdir(parents=True, exist_ok=True)
            payload = json.dumps(self.settings, indent=2, ensure_ascii=False) + "\n"
            tmp_path = self.settings_directory / f".{self.path.name}.tmp.{uuid.uuid4()}"
            try:
                with open(tmp_path, "w", encoding="utf-8") as handle:
                    handle.write(payload)
                    handle.flush()
                    os.fsync(handle.fileno())
                os.replace(tmp_path, self.path)
            except BaseException:
                try:
                    tmp_path.unlink(missing_ok=True)
                except OSError:
                    pass
                raise

    def getSetting(self, key: str, default: Any = None) -> Any:  # noqa: N802
        with self._lock:
            return self.settings.get(key, default)

    def setSetting(self, key: str, value: Any) -> None:  # noqa: N802
        with self._lock:
            self.settings[key] = value
        self.commit()
