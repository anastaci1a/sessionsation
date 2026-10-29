#!/bin/sh
# A metadata-only Antigravity CLI (agy 1.4.10) fake for integration tests. No
# network, model or credential access. Install it under the file name `agy`.
# State lives in $HOME/.gemini/antigravity-cli like the real CLI:
#   antigravity-oauth-token                         present after the fake login
#   conversations/<id>.db                           a conversation exists
#   brain/<id>/.system_generated/logs/transcript.jsonl  one JSON step per line
#   presence/<id>.lock                              held open by a live session
# Controls (files in $HOME): fake-print-sleep (seconds), fake-print-error,
# fake-print-session (conversation id to report).
home="${HOME:?HOME is required}"
[ "$AGY_ACCOUNT_HOME" = "$home" ] || { echo "AGY_ACCOUNT_HOME must equal HOME" >&2; exit 3; }
state="$home/.gemini/antigravity-cli"
mkdir -p "$state/conversations" "$state/presence"

conversation='' print='' input='' output='' timeout='' args="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --conversation) conversation=$2; shift 2 ;;
    --conversation=*) conversation=${1#--conversation=}; shift ;;
    -p=*|--print=*) print=1; shift ;;
    -p|--print) print=1; shift 2 ;;
    --input-format) input=$2; shift 2 ;;
    --output-format) output=$2; shift 2 ;;
    --print-timeout) timeout=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf 'args=%s\nupdate=%s\ncache=%s\nruntime=%s\napi_key=%s\npwd=%s\n' "$args" \
  "$AGY_CLI_DISABLE_AUTO_UPDATE" "$XDG_CACHE_HOME" "${XDG_RUNTIME_DIR:+present}" \
  "${GEMINI_API_KEY:+present}" "$PWD" >> "$home/fake-agy-calls"

if [ -n "$print" ]; then
  [ "$input" = stream-json ] && [ "$output" = stream-json ] || exit 42
  case "$timeout" in *s) ;; *) echo "invalid value for flag -print-timeout" >&2; exit 2 ;; esac
  if [ ! -s "$state/antigravity-oauth-token" ]; then
    echo 'Authentication required. Please visit the URL to log in:' >&2
    printf '{"event":"result","result":{"conversation_id":"","status":"ERROR","response":"","error":"authentication failed or timed out","duration_seconds":0,"num_turns":0}}\n'
    exit 1
  fi
  if [ -z "$conversation" ]; then
    conversation=$(cat /proc/sys/kernel/random/uuid)
  fi
  : > "$state/conversations/$conversation.db"
  printf '{"event":"init","conversation_id":"%s","init":{"cwd":"%s"}}\n' "$conversation" "$PWD"
  message=$(cat)
  [ -z "$message" ] && exit 0
  printf '%s\n' "$message" > "$home/fake-last-input"
  [ -f "$home/fake-print-sleep" ] && sleep "$(cat "$home/fake-print-sleep")"
  reported=$conversation
  [ -f "$home/fake-print-session" ] && reported=$(cat "$home/fake-print-session")
  log="$state/brain/$conversation/.system_generated/logs"
  mkdir -p "$log"
  printf '{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-01-01T00:00:00Z","content":"<USER_REQUEST>\\nfake\\n</USER_REQUEST>"}\n' >> "$log/transcript.jsonl"
  if [ -f "$home/fake-print-error" ]; then
    printf '{"event":"result","result":{"conversation_id":"%s","status":"ERROR","response":"","error":"fake failure","duration_seconds":1,"num_turns":1}}\n' "$reported"
    exit 1
  fi
  printf '{"event":"result","result":{"conversation_id":"%s","status":"SUCCESS","response":"fake reply\\n","duration_seconds":1.5,"num_turns":1,"usage":{"input_tokens":3,"output_tokens":2}}}\n' "$reported"
  exit 0
fi

# Interactive login: no conversation. Leaves a synthetic token like a finished
# browser sign-in (the real CLI prints a URL and reads a pasted code).
if [ -z "$conversation" ]; then
  printf 'synthetic-token' > "$state/antigravity-oauth-token"
  chmod 600 "$state/antigravity-oauth-token"
  printf '%s\n' 'Fake login complete.'
  exit 0
fi

# Interactive session: hold the presence lock open like agy, until stdin closes.
exec 9> "$state/presence/$conversation.lock"
read -r _
