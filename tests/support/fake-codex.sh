#!/bin/sh
# A metadata-only fake for daemon integration tests. No network or model calls.
if [ "$1" = '--version' ]; then
  printf '%s\n' 'codex-cli test-fixture'
  exit 0
fi
if [ "$1" = logout ]; then
  rm -f "$CODEX_HOME/auth.json"
  : > "$CODEX_HOME/fake-logged-out"
  printf 'cli\n' > "$CODEX_HOME/fake-logout"
  exit 0
fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"id":%s,"result":{"userAgent":"fake-codex"}}\n' "$id" ;;
    *'"method":"account/logout"'*)
      rm -f "$CODEX_HOME/auth.json"
      : > "$CODEX_HOME/fake-logged-out"
      printf 'rpc\n' > "$CODEX_HOME/fake-logout"
      printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"account/login/start"'*)
      rm -f "$CODEX_HOME/fake-logged-out"
      printf '%s\n' '{"tokens":{"account_id":"test-shared-identity"}}' > "$CODEX_HOME/auth.json"
      printf '{"id":%s,"result":{"type":"chatgptDeviceCode","verificationUrl":"https://example.invalid/device","userCode":"TEST-ONLY"}}\n' "$id" ;;
    *'"method":"account/read"'*)
      if [ -f "$CODEX_HOME/fake-logged-out" ]; then printf '{"id":%s,"result":{"account":null}}\n' "$id"; continue; fi
      printf '{"id":%s,"result":{"account":{"type":"chatgpt","email":"test@example.invalid","planType":"plus"}}}\n' "$id" ;;
    *'"method":"account/rateLimits/read"'*)
      reset=$(($(date +%s) + 18000))
      weekly=$(($(date +%s) + 604800))
      printf '{"id":%s,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":23,"windowDurationMins":300,"resetsAt":%s},"secondary":{"usedPercent":42,"windowDurationMins":10080,"resetsAt":%s}}}}\n' "$id" "$reset" "$weekly" ;;
  esac
done
