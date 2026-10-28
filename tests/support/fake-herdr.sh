#!/bin/sh
# A minimal HERDR fake for integration tests: one server, one existing Space
# (w1), new tabs get pane w1:p2, w1:p3, ... `pane run` starts the command in the
# background with the pane environment HERDR exports and records its PID as the
# pane's foreground process. State lives in $FAKE_HERDR_DIR. No sockets.
dir="${FAKE_HERDR_DIR:?FAKE_HERDR_DIR is required}"
mkdir -p "$dir"
printf '%s\n' "$*" >> "$dir/calls"
while [ $# -gt 0 ]; do
  case "$1" in
    --session) shift 2 ;;
    *) break ;;
  esac
done
alive() { [ -f "$dir/pid-$1" ] && kill -0 "$(cat "$dir/pid-$1")" 2>/dev/null; }
case "$1 $2" in
  "api snapshot")
    printf '%s\n' '{"result":{"snapshot":{"panes":[],"tabs":[]}}}' ;;
  "workspace list")
    printf '%s\n' '{"result":{"workspaces":[{"workspace_id":"w1","label":"Main"}]}}' ;;
  "workspace create")
    printf '%s\n' '{"result":{"workspace":{"workspace_id":"w9"},"tab":{"tab_id":"w9:t1","workspace_id":"w9"},"root_pane":{"pane_id":"w9:p1"}}}' ;;
  "tab create")
    shift 2
    workspace='' label=''
    while [ $# -gt 0 ]; do
      case "$1" in
        --workspace) workspace=$2; shift 2 ;;
        --label) label=$2; shift 2 ;;
        --cwd|--env) shift 2 ;;
        *) shift ;;
      esac
    done
    [ "$workspace" = w1 ] || { printf '%s\n' '{"error":{"code":"workspace_not_found"}}'; exit 1; }
    n=1; [ -f "$dir/tabs" ] && n=$(cat "$dir/tabs")
    n=$((n + 1)); printf '%s\n' "$n" > "$dir/tabs"
    printf '%s\n' "$label" > "$dir/label-w1:p$n"
    printf '{"result":{"tab":{"tab_id":"w1:t%s","workspace_id":"w1"},"root_pane":{"pane_id":"w1:p%s"}}}\n' "$n" "$n" ;;
  "tab rename")
    printf '%s\n' "$4" > "$dir/renamed-$3"
    printf '%s\n' '{"result":{}}' ;;
  "pane process-info")
    pane=$4
    if alive "$pane"; then
      printf '{"result":{"process_info":{"shell_pid":1,"foreground_processes":[{"pid":%s,"name":"claude"}]}}}\n' "$(cat "$dir/pid-$pane")"
    else
      printf '%s\n' '{"result":{"process_info":{"shell_pid":1,"foreground_processes":[{"pid":1,"name":"sh"}]}}}'
    fi ;;
  "pane run")
    pane=$3
    HERDR_ENV=1 HERDR_PANE_ID=$pane sh -c "$4" </dev/null >/dev/null 2>"$dir/run-$pane.err" &
    printf '%s\n' "$!" > "$dir/pid-$pane" ;;
  *)
    printf '%s\n' '{"error":{"code":"unsupported_fake_command"}}'
    exit 1 ;;
esac
