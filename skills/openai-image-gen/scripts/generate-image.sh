#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF' >&2
Usage:
  generate-image.sh --prompt "<prompt>" --output "<path>" [options]

Options:
  -p, --prompt <text>      Image description prompt (Required)
  -o, --output <file>      Local file path to save image (e.g. assets/hero.png) (Required)
  -m, --model <name>       Model name (default: "gpt-image-2.5-sunburst")
  -s, --size <dims>        Image dimensions (e.g. "1024x1024", "1536x1024", "1024x1536"; default: "1024x1024")
  -q, --quality <level>    Quality setting (default: "high"; GPT Image: auto/low/medium/high/xhigh/max)
  -b, --base-url <url>     Override OpenAI API base URL
  -k, --api-key <key>      Override API Key
  -h, --help               Show this help message

Authentication discovery order:
  1. CLI flags (--api-key / --base-url)
  2. Environment variables (OPENAI_API_KEY / OPENAI_BASE_URL)
  3. ~/.codex/auth.json (OPENAI_API_KEY, with the official API URL)
  4. ~/.codex/config.toml (selected provider's base_url and Bearer header together)
EOF
  exit 2
}

PROMPT=""
OUTPUT=""
MODEL="gpt-image-2.5-sunburst"
SIZE="1024x1024"
QUALITY="high"
CLI_BASE_URL=""
CLI_API_KEY=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -p|--prompt)
      [[ $# -ge 2 && -n "$2" ]] || usage
      PROMPT="$2"
      shift 2
      ;;
    -o|--output)
      [[ $# -ge 2 && -n "$2" ]] || usage
      OUTPUT="$2"
      shift 2
      ;;
    -m|--model)
      [[ $# -ge 2 && -n "$2" ]] || usage
      MODEL="$2"
      shift 2
      ;;
    -s|--size)
      [[ $# -ge 2 && -n "$2" ]] || usage
      SIZE="$2"
      shift 2
      ;;
    -q|--quality)
      [[ $# -ge 2 && -n "$2" ]] || usage
      QUALITY="$2"
      shift 2
      ;;
    -b|--base-url)
      [[ $# -ge 2 && -n "$2" ]] || usage
      CLI_BASE_URL="$2"
      shift 2
      ;;
    -k|--api-key)
      [[ $# -ge 2 && -n "$2" ]] || usage
      CLI_API_KEY="$2"
      shift 2
      ;;
    -h|--help)
      usage
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage
      ;;
  esac
done

if [[ -z "$PROMPT" ]]; then
  echo "Error: --prompt is required." >&2
  exit 1
fi

if [[ -z "$OUTPUT" ]]; then
  echo "Error: --output is required." >&2
  exit 1
fi

command -v curl >/dev/null || { echo "Error: 'curl' is required but not installed." >&2; exit 2; }
command -v jq >/dev/null || { echo "Error: 'jq' is required but not installed." >&2; exit 2; }
command -v python3 >/dev/null || { echo "Error: 'python3' is required but not installed." >&2; exit 2; }

CODEX_DIR="${CODEX_HOME:-$HOME/.codex}"
CONFIG_TOML="$CODEX_DIR/config.toml"
AUTH_JSON="$CODEX_DIR/auth.json"

BASE_URL="${CLI_BASE_URL:-${OPENAI_BASE_URL:-}}"
API_KEY="${CLI_API_KEY:-${OPENAI_API_KEY:-}}"

if [[ -z "$API_KEY" ]] && [[ -f "$AUTH_JSON" ]]; then
  API_KEY=$(jq -r '.OPENAI_API_KEY // .openai_api_key // empty' "$AUTH_JSON" 2>/dev/null || true)
fi

if [[ -z "$API_KEY" ]] && [[ -f "$CONFIG_TOML" ]]; then
  # Only use credentials from the selected provider, paired with its own URL.
  provider=$(python3 - "$CONFIG_TOML" <<'PY'
import json
import sys
import tomllib

try:
    with open(sys.argv[1], "rb") as source:
        config = tomllib.load(source)
    name = config.get("model_provider", "openai")
    provider = config.get("model_providers", {}).get(name, {})
    headers = provider.get("http_headers", {})
    authorization = next((value for key, value in headers.items() if key.lower() == "authorization"), "")
    token = authorization.split(" ", 1)[1] if authorization.lower().startswith("bearer ") else ""
    print(json.dumps({"url": provider.get("base_url", ""), "key": token}))
except (OSError, ValueError, TypeError, AttributeError) as exc:
    sys.exit(f"Error: Cannot read selected Codex provider: {exc}")
PY
  )
  provider_url=$(jq -r '.url' <<<"$provider")
  provider_key=$(jq -r '.key' <<<"$provider")
  if [[ -n "$provider_url" && -n "$provider_key" ]]; then
    if [[ -z "$BASE_URL" || "${BASE_URL%/}" = "${provider_url%/}" ]]; then
      API_KEY="$provider_key"
      BASE_URL="${BASE_URL:-$provider_url}"
    fi
  fi
fi

if [[ -z "$API_KEY" ]]; then
  echo "Error: OpenAI API Key not found." >&2
  echo "Please set OPENAI_API_KEY or configure ~/.codex/config.toml / ~/.codex/auth.json." >&2
  exit 1
fi

BASE_URL="${BASE_URL:-https://api.openai.com}"
BASE_URL="${BASE_URL%/}"
BASE_URL="${BASE_URL%/v1}"
ENDPOINT="$BASE_URL/v1/images/generations"
OUT_DIR="$(dirname "$OUTPUT")"
mkdir -p "$OUT_DIR"

PAYLOAD=$(jq -n \
  --arg prompt "$PROMPT" \
  --arg model "$MODEL" \
  --arg size "$SIZE" \
  --arg quality "$QUALITY" \
  '{
    model: $model,
    prompt: $prompt,
    size: $size,
    quality: $quality,
    n: 1
  }')

TMP_RESPONSE="$(mktemp /tmp/openai_img_XXXXXX.json)"
TMP_IMAGE=""
trap 'rm -f "$TMP_RESPONSE" "$TMP_IMAGE"' EXIT
TMP_IMAGE="$(mktemp "$OUT_DIR/.image.XXXXXXXX")"

if ! HTTP_CODE=$(curl -sS --connect-timeout 10 --max-time 600 -w "%{http_code}" -o "$TMP_RESPONSE" \
  -X POST "$ENDPOINT" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d "$PAYLOAD"); then
  echo "Error: Image generation request failed." >&2
  exit 1
fi

if [[ ! "$HTTP_CODE" =~ ^[0-9]{3}$ ]] || (( 10#$HTTP_CODE < 200 || 10#$HTTP_CODE >= 300 )); then
  error_msg=$(jq -r '.error.message // .message // empty' "$TMP_RESPONSE" 2>/dev/null || true)
  error_msg="${error_msg:-HTTP status $HTTP_CODE}"
  echo "Error: Image generation failed (HTTP $HTTP_CODE): $error_msg" >&2
  exit 1
fi

# Check if response returned b64_json or url
B64_DATA=$(jq -r '.data[0].b64_json // empty' "$TMP_RESPONSE" 2>/dev/null || true)
IMG_URL=$(jq -r '.data[0].url // empty' "$TMP_RESPONSE" 2>/dev/null || true)

if [[ -n "$B64_DATA" ]]; then
  if ! printf '%s' "$B64_DATA" | base64 -d > "$TMP_IMAGE"; then
    echo "Error: Invalid base64 image data." >&2
    exit 1
  fi
elif [[ -n "$IMG_URL" ]]; then
  if [[ "$IMG_URL" != https://* ]] || ! curl -fLsS --connect-timeout 10 --max-time 120 --proto '=https' --proto-redir '=https' "$IMG_URL" -o "$TMP_IMAGE"; then
    echo "Error: Image download failed." >&2
    exit 1
  fi
else
  echo "Error: No valid image data or URL found in API response." >&2
  exit 1
fi

if [[ ! -s "$TMP_IMAGE" ]]; then
  echo "Error: Image response was empty." >&2
  exit 1
fi
mv -f -- "$TMP_IMAGE" "$OUTPUT"

# Output a clean JSON status line for high signal-to-noise ratio in agent context
jq -n \
  --arg status "ok" \
  --arg file "$OUTPUT" \
  --arg model "$MODEL" \
  --arg size "$SIZE" \
  '{status: $status, file: $file, model: $model, size: $size}'
