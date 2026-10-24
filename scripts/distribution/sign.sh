#!/bin/sh
set +x
set -eu

fail() {
  printf '%s\n' "Signing failed: $*" >&2
  exit 1
}

usage() {
  printf '%s\n' 'Usage: sign.sh --binary-dir DIR --target TARGET --output-dir DIR' \
    'CI-only Developer ID signing and notarization. Credentials come from environment secrets.'
}

binary_dir=
target=
output_dir=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary-dir|--target|--output-dir)
      [ "$#" -ge 2 ] || fail "missing value for $1"
      case "$1" in
        --binary-dir) binary_dir=$2 ;;
        --target) target=$2 ;;
        --output-dir) output_dir=$2 ;;
      esac
      shift 2
      ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument" ;;
  esac
done

[ "${GITHUB_ACTIONS:-}" = true ] || fail 'this script only runs in GitHub Actions'
[ -n "${RUNNER_TEMP:-}" ] && [ -d "$RUNNER_TEMP" ] || fail 'RUNNER_TEMP must be an existing directory'
[ -n "$binary_dir" ] && [ -d "$binary_dir" ] || fail 'missing binary directory'
[ -f "$binary_dir/diskr" ] && [ ! -L "$binary_dir/diskr" ] || fail 'diskr must be a regular executable, not a symlink'
[ -x "$binary_dir/diskr" ] || fail 'diskr is not executable'
[ -n "$output_dir" ] || fail 'missing output directory'
case "$target" in
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) fail 'unsupported release target' ;;
esac

[ -n "${MACOS_DEVELOPER_ID_P12:-}" ] || fail 'MACOS_DEVELOPER_ID_P12 is required'
[ -n "${MACOS_DEVELOPER_ID_PASSWORD:-}" ] || fail 'MACOS_DEVELOPER_ID_PASSWORD is required'
[ -n "${MACOS_DEVELOPER_ID_IDENTITY:-}" ] || fail 'MACOS_DEVELOPER_ID_IDENTITY is required'
[ -n "${APPLE_NOTARY_KEY_P8:-}" ] || fail 'APPLE_NOTARY_KEY_P8 is required'
[ -n "${APPLE_NOTARY_KEY_ID:-}" ] || fail 'APPLE_NOTARY_KEY_ID is required'
[ -n "${APPLE_NOTARY_ISSUER_ID:-}" ] || fail 'APPLE_NOTARY_ISSUER_ID is required'
case "$MACOS_DEVELOPER_ID_IDENTITY" in
  'Developer ID Application: '*) ;;
  *) fail 'the signing identity must be a Developer ID Application certificate' ;;
esac

umask 077
work=$(mktemp -d "${RUNNER_TEMP%/}/diskr-sign.XXXXXX")
keychain="$work/signing.keychain-db"
keychain_attempted=false
cleanup() {
  status=$?
  trap - 0 HUP INT TERM
  if [ "$keychain_attempted" = true ]; then
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

export DISKR_SIGN_WORK="$work"
python3 - <<'PY'
import base64
import binascii
import os
from pathlib import Path

work = Path(os.environ["DISKR_SIGN_WORK"])
for name, filename in [("MACOS_DEVELOPER_ID_P12", "certificate.p12"), ("APPLE_NOTARY_KEY_P8", "notary.p8")]:
    try:
        data = base64.b64decode("".join(os.environ[name].split()), validate=True)
    except (ValueError, binascii.Error):
        raise SystemExit(f"Signing failed: {name} is not valid base64") from None
    if not data or (filename.endswith(".p8") and b"-----BEGIN PRIVATE KEY-----" not in data):
        raise SystemExit(f"Signing failed: {name} has invalid contents")
    (work / filename).write_bytes(data)
PY

keychain_password=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
keychain_attempted=true
security create-keychain -p "$keychain_password" "$keychain" >/dev/null
security set-keychain-settings -lut 3600 "$keychain" >/dev/null
security unlock-keychain -p "$keychain_password" "$keychain" >/dev/null
security import "$work/certificate.p12" -k "$keychain" -P "$MACOS_DEVELOPER_ID_PASSWORD" \
  -T /usr/bin/codesign -T /usr/bin/security >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null

codesign --force --options runtime --timestamp --sign "$MACOS_DEVELOPER_ID_IDENTITY" \
  --keychain "$keychain" "$binary_dir/diskr"
codesign --verify --strict --verbose=2 "$binary_dir/diskr"

mkdir -p "$output_dir"
dmg="$output_dir/diskr-$target.dmg"
[ ! -e "$dmg" ] || fail 'disk image output already exists'
hdiutil create -volname diskr -srcfolder "$binary_dir" -format UDZO "$dmg" >/dev/null
hdiutil verify "$dmg" >/dev/null
codesign --force --timestamp --sign "$MACOS_DEVELOPER_ID_IDENTITY" --keychain "$keychain" "$dmg"
codesign --verify --strict --verbose=2 "$dmg"

submit_status=0
xcrun notarytool submit "$dmg" --key "$work/notary.p8" --key-id "$APPLE_NOTARY_KEY_ID" \
  --issuer "$APPLE_NOTARY_ISSUER_ID" --wait --timeout 20m --output-format json \
  > "$work/submission.json" || submit_status=$?

submission_id=$(python3 - <<'PY'
import json
import os
import uuid
from pathlib import Path

try:
    result = json.loads((Path(os.environ["DISKR_SIGN_WORK"]) / "submission.json").read_text())
    print(uuid.UUID(result.get("id", "")))
except (ValueError, TypeError, AttributeError):
    pass
PY
)
if [ -n "$submission_id" ]; then
  diagnostic="$RUNNER_TEMP/notarization-$target.json"
  if xcrun notarytool log "$submission_id" "$diagnostic" --key "$work/notary.p8" \
    --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID" >/dev/null; then
    printf '%s\n' "Notarization diagnostic saved to $diagnostic"
  fi
fi
[ "$submit_status" -eq 0 ] || fail 'notarization submission failed or timed out'
python3 - <<'PY'
import json
import os
from pathlib import Path

try:
    result = json.loads((Path(os.environ["DISKR_SIGN_WORK"]) / "submission.json").read_text())
except (ValueError, OSError):
    raise SystemExit("Signing failed: invalid notarization response") from None
if not isinstance(result, dict) or result.get("status") != "Accepted":
    raise SystemExit("Signing failed: notarization was not accepted")
PY

# Standalone executables cannot carry a stapled ticket; the DMG can.
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
printf '%s\n' "Signed and notarized $target; disk image staple verified."
