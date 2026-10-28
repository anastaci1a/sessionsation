#!/bin/sh
# A metadata-only Gemini CLI fake for integration tests. No network, model or
# credential access. Install it under the file name `gemini.js` so that the
# interpreter line of its process looks like the real `node .../gemini.js`.
# State lives in the account home ($GEMINI_CLI_HOME/.gemini), like the real CLI:
#   oauth_creds.json + google_accounts.json   present after the fake login
#   tmp/<slug>/chats/session-<ts>-<id8>.jsonl chat file with a header line
#   projects.json                             cwd -> slug registry
# Controls (files in $GEMINI_CLI_HOME): fake-print-sleep (seconds),
# fake-print-session (session id to report), fake-print-error, fake-print-exit.
home="${GEMINI_CLI_HOME:?GEMINI_CLI_HOME is required}"
state="$home/.gemini"
case "$1" in
  --version)
    printf '%s\n' '0.66.0 (Gemini CLI fake)'
    exit 0 ;;
esac

env_report() {
  printf 'api_key=%s\ngoogle_key=%s\nvertex=%s\nproject=%s\nbase_url=%s\nmodel=%s\n' \
    "${GEMINI_API_KEY:+present}" "${GOOGLE_API_KEY:+present}" "${GOOGLE_GENAI_USE_VERTEXAI:+present}" \
    "${GOOGLE_CLOUD_PROJECT:+present}" "${GOOGLE_GEMINI_BASE_URL:+present}" "${GEMINI_MODEL:+present}"
  printf 'home=%s\nno_relaunch=%s\nfile_storage=%s\ntrust=%s\nno_browser=%s\ncwd=%s\n' \
    "$GEMINI_CLI_HOME" "$GEMINI_CLI_NO_RELAUNCH" "$GEMINI_FORCE_FILE_STORAGE" \
    "$GEMINI_CLI_TRUST_WORKSPACE" "$NO_BROWSER" "$PWD"
}
slug_of() { printf '%s' "$1" | sed 's|^/||; s|/|-|g'; }
register_project() {
  mkdir -p "$state"
  printf '%s\t%s\n' "$PWD" "$(slug_of "$PWD")" >> "$state/fake-projects.tsv"
  {
    printf '{"projects":{'
    sort -u "$state/fake-projects.tsv" | awk -F '\t' 'BEGIN{first=1} {printf "%s\"%s\":\"%s\"", (first?"":","), $1, $2; first=0}'
    printf '}}\n'
  } > "$state/projects.json"
}
chat_file() { # $1 session id
  printf '%s/tmp/%s/chats/session-2026-01-01T00-00-%s.jsonl' "$state" "$(slug_of "$PWD")" "$(printf '%s' "$1" | cut -c1-8)"
}
find_chat() { # $1 session id
  find "$state/tmp" -name "session-*-$(printf '%s' "$1" | cut -c1-8).jsonl" 2>/dev/null | head -n 1
}

# Parse the argument vector once.
session='' mode='' headless='' format='' approval=''
args="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session=$2; mode=new; shift 2 ;;
    -r|--resume) session=$2; mode=resume; shift 2 ;;
    -p|--prompt) headless=1; shift 2 ;;
    -o|--output-format) format=$2; shift 2 ;;
    --approval-mode) approval=$2; shift 2 ;;
    *) shift ;;
  esac
done

# Interactive login (NO_BROWSER): no session flags. Records its environment and
# leaves synthetic credential files; the real CLI would print a URL and read a code.
if [ -z "$session" ] && [ -z "$headless" ]; then
  mkdir -p "$state"
  { env_report; printf 'args=%s\n' "$args"; } > "$home/fake-login-env"
  printf 'synthetic-credential' > "$state/oauth_creds.json"
  chmod 600 "$state/oauth_creds.json"
  printf '{"active":"fixture@example.invalid","old":[]}\n' > "$state/google_accounts.json"
  printf '%s\n' 'Fake login complete.'
  exit 0
fi
[ -n "$session" ] || exit 42

# Headless turn (`-p "" -o json`): the message arrives on stdin.
if [ -n "$headless" ]; then
  [ "$format" = json ] || exit 42
  message=$(cat)
  n=0; [ -f "$home/fake-print-count" ] && n=$(cat "$home/fake-print-count")
  printf '%s\n' "$((n + 1))" > "$home/fake-print-count"
  {
    printf 'mode=%s\nsession=%s\nmessage=%s\napproval=%s\nargs=%s\n' "$mode" "$session" "$message" "$approval" "$args"
    env_report
  } > "$home/fake-print-$mode"
  [ -f "$home/fake-print-sleep" ] && sleep "$(cat "$home/fake-print-sleep")"
  existing=$(find_chat "$session")
  if [ "$mode" = new ]; then
    [ -n "$existing" ] && exit 42
    register_project
    file=$(chat_file "$session")
    mkdir -p "$(dirname "$file")"
  else
    [ -n "$existing" ] || exit 42
    file=$existing
  fi
  if [ ! -s "$file" ]; then
    printf '{"sessionId":"%s","projectHash":"fixture","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z"}\n' "$session" > "$file"
  fi
  printf '{"type":"user","content":"fixture prompt text"}\n' >> "$file"
  reported=$session; [ -f "$home/fake-print-session" ] && reported=$(cat "$home/fake-print-session")
  words=$(printf '%s' "$message" | head -n 1 | tr -cd 'A-Za-z0-9 ')
  if [ -f "$home/fake-print-error" ]; then
    printf '{"session_id":"%s","error":{"type":"FatalError","message":"fixture failure","code":1}}\n' "$reported"
    exit 1
  fi
  printf '{"session_id":"%s","response":"fake reply to: %s","stats":{"models":{"fixture":{"tokens":{"total":7}}}}}\n' "$reported" "$words"
  code=0; [ -f "$home/fake-print-exit" ] && code=$(cat "$home/fake-print-exit")
  exit "$code"
fi

# Interactive session: refuses a taken id (new) or a missing chat (resume), like
# the real CLI, writes the chat header, records its launch and stays alive.
existing=$(find_chat "$session")
if [ "$mode" = new ]; then
  [ -n "$existing" ] && exit 42
  register_project
  file=$(chat_file "$session")
  mkdir -p "$(dirname "$file")"
  printf '{"sessionId":"%s","projectHash":"fixture","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z"}\n' "$session" > "$file"
  printf '{"type":"user","content":"fixture prompt text"}\n' >> "$file"
else
  [ -n "$existing" ] || exit 42
fi
{ printf 'mode=%s\nsession=%s\nargs=%s\n' "$mode" "$session" "$args"; env_report; } > "$home/fake-launch-$mode"
while :; do sleep 1; done
