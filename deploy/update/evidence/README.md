# Stable hardware evidence

Stable TUF promotion is blocked until a completed hardware evidence record for the exact release
lives in this directory on the default branch.

```bash
cp deploy/update/COMPATIBILITY.md deploy/update/evidence/v0.1.0.md
# Fill every field from observed Steam Deck / SteamOS hardware, then (using the release tag, the
# signed build ID and the managed archive SHA-256):
just verify-stable-evidence v0.1.0 <build-id> <managed-sha256>
```

`verify-stable-evidence` refuses a record that still contains `PENDING`, names a different tag,
version, build ID or managed SHA-256, lacks an operator/reviewer/date, or does not contain the exact
line:

```text
Stable eligible: PASS
```

The promotion workflow checks out the default branch, verifies the immutable published release and
the evidence record from that branch, and only then requests TUF signing for the `stable` channel.
A VM-only run or an unrecorded Deck session is not Stable evidence. Keep the hardware gate
**PENDING** until it is actually observed.
