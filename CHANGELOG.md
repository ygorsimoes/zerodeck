# Changelog

## [0.1.1](https://github.com/ygorsimoes/zerodeck/compare/v0.1.0...v0.1.1) (2026-11-28)


### ✨ Features

* **foundation:** ZeroDeck Rust monorepo, Python plugin host, TS 7 frontend & Docker setup ([#1](https://github.com/ygorsimoes/zerodeck/issues/1)) ([ccfa3c9](https://github.com/ygorsimoes/zerodeck/commit/ccfa3c93d80b58744d6f5b48788481e018b1d149))
* **plugin-host:** implement Phase 2 supervisor, privilege isolation, and Python parity ([#16](https://github.com/ygorsimoes/zerodeck/issues/16)) ([83eaa77](https://github.com/ygorsimoes/zerodeck/commit/83eaa7705e43d7a669b378bf029e7c1f664da731))
* **plugins:** enforce privileged launches and qualify Stable releases ([#24](https://github.com/ygorsimoes/zerodeck/issues/24)) ([940d6be](https://github.com/ygorsimoes/zerodeck/commit/940d6beed050cfce422c6965a6f210015a168d6e))
* **release:** configure Release Please, GHCR/tarball publishing, PR lint, and OSS docs ([#7](https://github.com/ygorsimoes/zerodeck/issues/7)) ([18d4c01](https://github.com/ygorsimoes/zerodeck/commit/18d4c016244022c7fbfa2eb96a83f0d8fb5bcc63))
* **server,core:** implement Phase 3 full server, FilePicker, and plugin installer ([69e6cd1](https://github.com/ygorsimoes/zerodeck/commit/69e6cd1ac43578eaff85d85ec99d76e31a201c90))
* **updater:** add native update, recovery and CI-signed release channels ([#23](https://github.com/ygorsimoes/zerodeck/issues/23)) ([6737c80](https://github.com/ygorsimoes/zerodeck/commit/6737c808bdb47c253088161549d10ba0031f8cab))


### 🐛 Bug Fixes

* all-or-nothing plugin extraction, WebSocket limits/keepalive/FULL_SYNC, reconnect backoff ([#11](https://github.com/ygorsimoes/zerodeck/issues/11)) ([72cc07f](https://github.com/ygorsimoes/zerodeck/commit/72cc07fdb3b9af08b066bc75c0e630653c4a9d5e))
* block DNS rebinding, redact auth token, harden atomic writes and CI ([#10](https://github.com/ygorsimoes/zerodeck/issues/10)) ([8b5cd49](https://github.com/ygorsimoes/zerodeck/commit/8b5cd49e3a49f0be85b5d90a9430f2de29f11e6d))
* close out review findings (auth on /ws and /fetch, frontend boot retry, fuzzing, release provenance) ([#14](https://github.com/ygorsimoes/zerodeck/issues/14)) ([8a0efd2](https://github.com/ygorsimoes/zerodeck/commit/8a0efd2a53f8650eb92d3e19b0e516866926066d))
* harden plugin discovery, Python worker IPC, WS reconnect, and systemd unit ([#9](https://github.com/ygorsimoes/zerodeck/issues/9)) ([13bd2ea](https://github.com/ygorsimoes/zerodeck/commit/13bd2eaa06843c43fe496c7f1a3259a964a57ecb))
* IPv6/hostname binding and healthcheck, deterministic discovery, daemon e2e tests, CI Python matrix ([#12](https://github.com/ygorsimoes/zerodeck/issues/12)) ([54b592a](https://github.com/ygorsimoes/zerodeck/commit/54b592ae0ec7127f2b9b800962ec37dc5e711b38))
* **py-host:** report exception type and traceback for failed plugin calls ([#13](https://github.com/ygorsimoes/zerodeck/issues/13)) ([e5812c5](https://github.com/ygorsimoes/zerodeck/commit/e5812c5b87bb99ee51f7e5cd4aae23b306a88c54))
* security and robustness hardening ([#18](https://github.com/ygorsimoes/zerodeck/issues/18)) ([bd13577](https://github.com/ygorsimoes/zerodeck/commit/bd1357783f1bec934de5cae8db7de4bf8a59e415))


### ⚡ Performance Improvements

* stop blocking the async runtime ([#19](https://github.com/ygorsimoes/zerodeck/issues/19)) ([cd768dc](https://github.com/ygorsimoes/zerodeck/commit/cd768dc60bd1543188cf370eb58ac9fe302cd4f0))


### 📚 Documentation

* add ZeroDeck foundation design spec and implementation plan ([2c8fafc](https://github.com/ygorsimoes/zerodeck/commit/2c8fafcbae19b8b6f0b9c103d30dc6f67cc0ad13))
