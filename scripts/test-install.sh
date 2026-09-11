#!/usr/bin/env bash
set -euo pipefail

installer="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/install.sh"
bash_bin="$(command -v bash)"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
    verifier=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    verifier=(shasum -a 256)
else
    fail "Tests require sha256sum or shasum"
fi
verifier[0]="$(command -v "${verifier[0]}")"

mkdir -p "$tmpdir/tools" "$tmpdir/fixture"
archive="tgrep-v0.0.0-aarch64-apple-darwin.tar.gz"
printf '#!%s\nprintf "tgrep 0.0.0\\n"\n' "$bash_bin" > "$tmpdir/fixture/tgrep"
tar czf "$tmpdir/fixture/$archive" -C "$tmpdir/fixture" tgrep
(cd "$tmpdir/fixture" && "${verifier[@]}" "$archive") > "$tmpdir/fixture/checksums.txt"
printf '%064d  unrelated.tar.gz\n' 0 >> "$tmpdir/fixture/checksums.txt"

# An isolated PATH makes the cases independent of the host's installed tools.
for tool in cp grep gzip install mkdir mktemp rm tar; do
    printf '#!%s\nexec %q "$@"\n' "$bash_bin" "$(command -v "$tool")" > "$tmpdir/tools/$tool"
done
{
    printf '#!%s\n' "$bash_bin"
    cat <<'EOF'
case "$1" in
    -s) printf 'Darwin\n' ;;
    -m) printf 'arm64\n' ;;
    *) exit 1 ;;
esac
EOF
} > "$tmpdir/tools/uname"
{
    printf '#!%s\n' "$bash_bin"
    cat <<'EOF'
set -eu
[ "$1" = -fsSL ] && [ "$3" = -o ]
printf '%s\n' "$2" >> "$DOWNLOADS"
cp "$FIXTURE/${2##*/}" "$4"
case "$2:$SCENARIO" in
    *.tar.gz:corrupt) printf 'corruption\n' >> "$4" ;;
    */checksums.txt:missing) printf '%064d  unrelated.tar.gz\n' 0 > "$4" ;;
    */checksums.txt:malformed) printf 'invalid  %s\n' "$ARCHIVE" > "$4" ;;
esac
EOF
} > "$tmpdir/tools/curl"
chmod +x "$tmpdir/tools/"*

run_case() (
    tools="$1"
    scenario="$2"
    case_dir="$tmpdir/${tools// /-}-$scenario"
    mkdir -p "$case_dir/bin" "$case_dir/install" "$case_dir/tmp"
    for tool in $tools; do
        [ "$tool" != none ] || continue
        case "$tool" in
            sha256sum) expected_args="-c --strict -" ;;
            shasum) expected_args="-a 256 -c --strict -" ;;
        esac
        backend=("${verifier[@]}")
        if command -v "$tool" >/dev/null 2>&1; then
            backend=("$(command -v "$tool")")
            if [ "$tool" = shasum ]; then
                backend+=(-a 256)
            fi
        fi
        # Check the selected command and flags, then use a real SHA-256 backend.
        {
            printf '#!%s\n' "$bash_bin"
            printf 'printf "%%s\\n" %q >> "$CHECKSUM_CALLS"\n' "$tool"
            printf '[ "$*" = %q ] || exit 2\n' "$expected_args"
            printf 'exec '
            printf '%q ' "${backend[@]}"
            printf '%s\n' '-c --strict -'
        } > "$case_dir/bin/$tool"
        chmod +x "$case_dir/bin/$tool"
    done

    status=0
    PATH="$case_dir/bin:$tmpdir/tools" \
        TMPDIR="$case_dir/tmp" \
        FIXTURE="$tmpdir/fixture" \
        ARCHIVE="$archive" \
        SCENARIO="$scenario" \
        DOWNLOADS="$case_dir/downloads" \
        CHECKSUM_CALLS="$case_dir/checksum-calls" \
        TGREP_VERSION=v0.0.0 \
        TGREP_INSTALL_DIR="$case_dir/install" \
        "$bash_bin" "$installer" > "$case_dir/output" 2>&1 || status=$?

    if [ "$scenario" = valid ] && [ "$tools" != none ]; then
        if [ "$status" -ne 0 ] || [ ! -x "$case_dir/install/tgrep" ]; then
            cat "$case_dir/output"
            fail "$tools/$scenario: installation did not succeed"
        fi
        cmp "$tmpdir/fixture/tgrep" "$case_dir/install/tgrep"
    else
        [ "$status" -ne 0 ] || fail "$tools/$scenario: installer should fail"
        [ ! -e "$case_dir/install/tgrep" ] || fail "$tools/$scenario: installed an unverified binary"
        if grep -q 'Extracting' "$case_dir/output"; then
            fail "$tools/$scenario: extracted an unverified archive"
        fi
        if [ "$tools" = none ]; then
            grep -q 'requires sha256sum or shasum' "$case_dir/output" || fail "Missing dependency diagnostic"
            [ ! -e "$case_dir/downloads" ] || fail "Downloaded before checking checksum tools"
            if grep -q 'Checksum verification failed' "$case_dir/output"; then
                fail "Reported missing tools as a checksum failure"
            fi
        else
            grep -q 'Checksum verification failed' "$case_dir/output" || fail "Missing checksum failure diagnostic"
        fi
    fi
    if [ "$tools" != none ]; then
        [ "$(cat "$case_dir/checksum-calls")" = "${tools%% *}" ] || fail "Wrong checksum tool selected"
    fi
    for leftover in "$case_dir/tmp/"*; do
        [ ! -e "$leftover" ] || fail "$tools/$scenario: temporary files were not cleaned up"
    done
    printf 'PASS: %s/%s\n' "$tools" "$scenario"
)

for tools in shasum sha256sum "sha256sum shasum"; do
    for scenario in valid corrupt missing malformed; do
        run_case "$tools" "$scenario"
    done
done
run_case none valid
