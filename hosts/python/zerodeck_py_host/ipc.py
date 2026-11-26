from __future__ import annotations

import asyncio
import json
import socket
from collections.abc import Awaitable, Callable
from contextlib import suppress
from typing import Any

IPC_READER_LIMIT = 4 * 1024 * 1024  # 4 MiB line buffer for large plugin IPC payloads


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
        """Takes ownership of an inherited socket FD; closing the transport closes it."""
        # `socket.fromfd` would dup() the descriptor and leak the inherited original.
        sock = socket.socket(fileno=fd)
        sock.setblocking(False)
        reader, writer = await asyncio.open_connection(sock=sock, limit=IPC_READER_LIMIT)
        return cls(reader, writer)

    @classmethod
    async def from_socket_path(cls, socket_path: str) -> JsonLineTransport:
        reader, writer = await asyncio.open_unix_connection(
            socket_path,
            limit=IPC_READER_LIMIT,
        )
        return cls(reader, writer)

    async def send_json(self, payload: dict[str, Any]) -> None:
        line = json.dumps(payload, ensure_ascii=False) + "\n"
        async with self._write_lock:
            self._writer.write(line.encode("utf-8"))
            await self._writer.drain()

    async def serve(self, on_message: Callable[[str], Awaitable[str | None]]) -> None:
        """Reads lines until EOF, dispatching each one concurrently so slow calls never block."""
        in_flight: set[asyncio.Task[None]] = set()

        async def dispatch(line: str) -> None:
            reply = await on_message(line)
            if reply is not None:
                if not reply.endswith("\n"):
                    reply += "\n"
                async with self._write_lock:
                    self._writer.write(reply.encode("utf-8"))
                    await self._writer.drain()

        try:
            while True:
                raw = await self._reader.readline()
                if not raw:
                    break
                line = raw.decode("utf-8").strip()
                if not line:
                    continue
                task = asyncio.create_task(dispatch(line))
                in_flight.add(task)
                task.add_done_callback(in_flight.discard)
            if in_flight:
                await asyncio.gather(*in_flight)
        finally:
            for task in in_flight:
                task.cancel()

    async def close(self) -> None:
        self._writer.close()
        with suppress(OSError):
            await self._writer.wait_closed()
