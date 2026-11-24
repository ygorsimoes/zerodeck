from __future__ import annotations

import asyncio
import json
import socket
from typing import Any, Awaitable, Callable, Optional


class JsonLineTransport:
    """Newline-delimited JSON transport over an inherited FD or Unix socket path."""

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> None:
        self._reader = reader
        self._writer = writer
        self._write_lock = asyncio.Lock()

    @classmethod
    async def from_fd(cls, fd: int) -> JsonLineTransport:
        sock = socket.fromfd(fd, socket.AF_UNIX, socket.SOCK_STREAM)
        sock.setblocking(False)
        reader, writer = await asyncio.open_connection(sock=sock)
        return cls(reader, writer)

    @classmethod
    async def from_socket_path(cls, socket_path: str) -> JsonLineTransport:
        reader, writer = await asyncio.open_unix_connection(socket_path)
        return cls(reader, writer)

    async def send_json(self, payload: dict[str, Any]) -> None:
        line = json.dumps(payload, ensure_ascii=False) + "\n"
        async with self._write_lock:
            self._writer.write(line.encode("utf-8"))
            await self._writer.drain()

    async def serve(self, on_message: Callable[[str], Awaitable[Optional[str]]]) -> None:
        while True:
            raw = await self._reader.readline()
            if not raw:
                break
            line = raw.decode("utf-8").strip()
            if not line:
                continue
            reply = await on_message(line)
            if reply is not None:
                async with self._write_lock:
                    if not reply.endswith("\n"):
                        reply += "\n"
                    self._writer.write(reply.encode("utf-8"))
                    await self._writer.drain()

    async def close(self) -> None:
        self._writer.close()
        try:
            await self._writer.wait_closed()
        except OSError:
            pass
