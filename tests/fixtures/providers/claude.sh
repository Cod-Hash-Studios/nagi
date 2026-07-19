#!/bin/sh

if [ "${1:-}" = "--version" ]; then
  printf '%s\n' 'claude fixture'
  exit 0
fi

case "$0" in
  *disconnect*) scenario=disconnect ;;
  *) scenario=conformance ;;
esac

resume_session=''
previous=''
for argument in "$@"; do
  if [ "$previous" = '--resume' ]; then
    resume_session="$argument"
    break
  fi
  previous="$argument"
done

IFS= read -r initialize
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"managed-1","response":{}}}'

if [ -n "$resume_session" ]; then
  while IFS= read -r line; do :; done
  exit 0
fi

IFS= read -r first_turn
printf '%s\n' '{"type":"system","subtype":"init","session_id":"session-live"}'

if [ "$scenario" = 'disconnect' ]; then
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]},"session_id":"session-live"}'
  exit 0
fi

printf '%s\n' '{"type":"control_request","request_id":"attention-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-1","input":{"command":"cargo test"},"title":"Run cargo test?"}}'
IFS= read -r attention_response
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]},"session_id":"session-live"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"session_id":"session-live"}'

IFS= read -r second_turn
IFS= read -r interrupt
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"managed-2","response":{}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"session_id":"session-live"}'

while IFS= read -r line; do :; done
