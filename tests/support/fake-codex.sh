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
    *'"method":"account/rateLimitResetCredit/consume"'*)
      # Reset-credit fake (contract 1.53.0); behaviour is chosen by files in CODEX_HOME.
      key=$(printf '%s' "$line" | sed -n 's/.*"idempotencyKey":"\([^"]*\)".*/\1/p')
      printf '%s\n' "$key" >> "$CODEX_HOME/fake-reset-keys"
      if [ -f "$CODEX_HOME/fake-reset-unsupported" ]; then
        printf '{"id":%s,"error":{"code":-32600,"message":"Invalid request: unknown variant `account/rateLimitResetCredit/consume`"}}\n' "$id"
        continue
      fi
      # The supervisor runs this worker in its data root: the journal must already exist.
      if grep -q '"phase": "requesting"' "operations/account-reset-credits/$key.json" 2>/dev/null; then
        printf '%s\n' "$key" >> "$CODEX_HOME/fake-reset-journaled"
      fi
      if grep -qx "$key" "$CODEX_HOME/fake-reset-redeemed" 2>/dev/null; then
        outcome=alreadyRedeemed
      elif [ -f "$CODEX_HOME/fake-reset-outcome" ]; then
        outcome=$(cat "$CODEX_HOME/fake-reset-outcome")
      else
        outcome=noCredit
      fi
      if [ "$outcome" = reset ]; then
        printf '%s\n' "$key" >> "$CODEX_HOME/fake-reset-redeemed"
        : > "$CODEX_HOME/fake-reset-done"
        if [ -f "$CODEX_HOME/fake-reset-available" ]; then
          n=$(cat "$CODEX_HOME/fake-reset-available")
          printf '%s\n' $((n - 1)) > "$CODEX_HOME/fake-reset-available"
        fi
      fi
      printf '{"id":%s,"result":{"outcome":"%s"}}\n' "$id" "$outcome" ;;
    *'"method":"account/rateLimits/read"'*)
      reset=$(($(date +%s) + 18000))
      weekly=$(($(date +%s) + 604800))
      short=23; long=42; credits=''
      if [ -f "$CODEX_HOME/fake-reset-done" ]; then short=0; long=0; fi
      if [ -f "$CODEX_HOME/fake-reset-available" ]; then
        credits=$(printf ',"rateLimitResetCredits":{"availableCount":%s,"credits":null}' "$(cat "$CODEX_HOME/fake-reset-available")")
      fi
      printf '{"id":%s,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":%s,"windowDurationMins":300,"resetsAt":%s},"secondary":{"usedPercent":%s,"windowDurationMins":10080,"resetsAt":%s}}%s}}\n' "$id" "$short" "$reset" "$long" "$weekly" "$credits" ;;
  esac
done
