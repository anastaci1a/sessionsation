#!/bin/sh
# A metadata-only Claude Code fake for integration tests. No network, model or
# credential access. Per-account behaviour is selected by files in the account
# config home ($CLAUDE_CONFIG_DIR):
#   fake-auth-mode: subscription (default) | logged_out | api_key | bedrock
home="${CLAUDE_CONFIG_DIR:?CLAUDE_CONFIG_DIR is required}"
now_ms() { printf '%s000' "$(date +%s)"; }
case "$1" in
  --version)
    printf '%s\n' '2.1.312 (Claude Code fake)'
    exit 0 ;;
  auth)
    case "$2" in
      status)
        mode=subscription
        [ -f "$home/fake-auth-mode" ] && mode=$(cat "$home/fake-auth-mode")
        # An ambient API key would switch real Claude to API billing.
        [ -n "$ANTHROPIC_API_KEY" ] && mode=api_key
        case "$mode" in
          logged_out) printf '%s\n' '{"loggedIn":false}'; exit 1 ;;
          api_key) printf '%s\n' '{"loggedIn":true,"authMethod":"apiKey","apiProvider":"firstParty","email":"fixture@example.invalid"}' ;;
          bedrock) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"bedrock"}' ;;
          *) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"fixture@example.invalid","orgId":"fixture-org","orgName":"Fixture"}' ;;
        esac
        exit 0 ;;
      login)
        printf '%s\n' "$*" > "$home/fake-login-args"
        printf '%s\n' 'subscription' > "$home/fake-auth-mode"
        printf '%s\n' 'Fake login complete.'
        exit 0 ;;
    esac
    exit 2 ;;
esac
session='' name='' mode=''
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session=$2; mode=new; shift 2 ;;
    --resume) session=$2; mode=resume; shift 2 ;;
    --name) name=$2; shift 2 ;;
    --settings) shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$session" ] || exit 2
start=$(sed 's/^.*) //' /proc/$$/stat | cut -d' ' -f20)
mkdir -p "$home/sessions"
slug=$(printf '%s' "$PWD" | tr '/' '-')
mkdir -p "$home/projects/$slug"
transcript="$home/projects/$slug/$session.jsonl"
if [ "$mode" = new ]; then
  printf '{"type":"user","sessionId":"%s","cwd":"%s","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"fixture prompt text"}}\n' "$session" "$PWD" > "$transcript"
fi
{
  printf 'mode=%s\nsession=%s\nname=%s\ncwd=%s\n' "$mode" "$session" "$name" "$PWD"
  printf 'api_key=%s\nauth_token=%s\nbase_url=%s\n' "${ANTHROPIC_API_KEY:+present}" "${ANTHROPIC_AUTH_TOKEN:+present}" "${ANTHROPIC_BASE_URL:+present}"
  printf 'autoupdater=%s\nconfig_dir=%s\n' "$DISABLE_AUTOUPDATER" "$CLAUDE_CONFIG_DIR"
} > "$home/fake-launch-$mode"
printf '{"pid":%s,"sessionId":"%s","cwd":"%s","startedAt":%s,"procStart":"%s","version":"2.1.312","kind":"interactive","name":"%s","nameSource":"flag","status":"busy","updatedAt":%s}\n' \
  "$$" "$session" "$PWD" "$(now_ms)" "$start" "$name" "$(now_ms)" > "$home/sessions/$$.json"
printf 'x' > "$home/sessions/$$.fixture.key"
chmod 000 "$home/sessions/$$.fixture.key"
while :; do sleep 1; done
