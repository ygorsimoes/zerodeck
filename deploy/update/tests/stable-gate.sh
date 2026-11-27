#!/usr/bin/env bash
# Stable promotion evidence gate: only an exact, complete PASS record is accepted.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-stable-gate.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT

tag="v0.1.0"
version="0.1.0"
build_id="v0.1.0-deadbeef"
digest="$(printf 'a%.0s' {1..64})"
gate="${repo_root}/deploy/update/verify-stable-evidence.sh"

write_evidence() {
    local path="$1" eligible="$2" pending="$3" recorded_digest="$4"
    cat >"${path}" <<EOF
# ZeroDeck loader compatibility and release evidence

## Release identity

| Field | Value |
|---|---|
| Release tag | ${tag} |
| Loader version / build ID | ${version} / ${build_id} |
| Managed asset SHA-256 (generation identity) | ${recorded_digest} |

## Lifecycle results

| Scenario | Result | Observed active digest / identity | Duration | Evidence |
|---|---|---|---|---|
| Clean bootstrap with \`--start\` | PASS | ${recorded_digest} | 4 s | journal:test |

## Sign-off

- Operator: deck-tester
- Independent reviewer: second-reviewer
- Date: 2026-11-26
- Stable eligible: ${eligible}
EOF
    if [[ "${pending}" == "yes" ]]; then
        printf '\n| Compression comparison | PENDING |\n' >>"${path}"
    fi
}

write_evidence "${tmp}/pass.md" "PASS" no "${digest}"
bash "${gate}" --tag "${tag}" --version "${version}" --build-id "${build_id}" \
    --sha256 "${digest}" --evidence "${tmp}/pass.md" >"${tmp}/pass.log"
grep -q "complete PASS evidence" "${tmp}/pass.log"

expect_failure() {
    local label="$1"
    shift
    if "$@" >"${tmp}/${label}.log" 2>&1; then
        echo "error: ${label} unexpectedly passed" >&2
        cat "${tmp}/${label}.log" >&2
        exit 1
    fi
}

write_evidence "${tmp}/pending.md" "PASS" yes "${digest}"
expect_failure pending bash "${gate}" --tag "${tag}" --version "${version}" \
    --build-id "${build_id}" --sha256 "${digest}" --evidence "${tmp}/pending.md"
grep -q "PENDING" "${tmp}/pending.log"

write_evidence "${tmp}/not-eligible.md" "PENDING" no "${digest}"
expect_failure not-eligible bash "${gate}" --tag "${tag}" --version "${version}" \
    --build-id "${build_id}" --sha256 "${digest}" --evidence "${tmp}/not-eligible.md"

write_evidence "${tmp}/wrong-digest.md" "PASS" no "$(printf 'b%.0s' {1..64})"
expect_failure wrong-digest bash "${gate}" --tag "${tag}" --version "${version}" \
    --build-id "${build_id}" --sha256 "${digest}" --evidence "${tmp}/wrong-digest.md"
grep -q "Managed asset SHA-256" "${tmp}/wrong-digest.log"

# Evidence for a prerelease of the same version must not satisfy the stable tag.
write_evidence "${tmp}/rc.md" "PASS" no "${digest}"
sed -i.bak "s/^| Release tag | ${tag} |/| Release tag | ${tag}-rc.1 |/" "${tmp}/rc.md"
expect_failure prerelease-tag bash "${gate}" --tag "${tag}" --version "${version}" \
    --build-id "${build_id}" --sha256 "${digest}" --evidence "${tmp}/rc.md"
grep -q "Release tag" "${tmp}/prerelease-tag.log"

expect_failure missing-evidence bash "${gate}" --tag "${tag}" --version "${version}" \
    --build-id "${build_id}" --sha256 "${digest}" --evidence "${tmp}/absent.md"

echo "stable evidence gate tests passed"
