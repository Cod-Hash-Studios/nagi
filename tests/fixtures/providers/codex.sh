#!/bin/sh

if [ "${1:-}" = "--version" ]; then
  printf '%s\n' 'codex fixture'
  exit 0
fi

case "$0" in
  *disconnect*) scenario=disconnect ;;
  *) scenario=conformance ;;
esac

turn=0
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"session-live"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"session-resumed"}}}'
      ;;
    *'"method":"turn/start"'*)
      turn=$((turn + 1))
      if [ "$turn" -eq 1 ]; then
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
        if [ "$scenario" = "disconnect" ]; then
          printf '%s\n' '{"method":"item/agentMessage/delta","params":{"turnId":"turn-1","delta":"partial"}}'
          exit 0
        fi
        printf '%s\n' '{"id":"attention-1","method":"item/commandExecution/requestApproval","params":{"threadId":"session-live","turnId":"turn-1","itemId":"item-1","command":"cargo test"}}'
      else
        printf '%s\n' '{"id":4,"result":{"turn":{"id":"turn-2"}}}'
      fi
      ;;
    *'"id":"attention-1","result"'*)
      printf '%s\n' '{"method":"serverRequest/resolved","params":{"requestId":"attention-1"}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"turnId":"turn-1","delta":"done"}}'
      printf '%s\n' '{"method":"turn/completed","params":{"turn":{"id":"turn-1","status":"completed"}}}'
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":5,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"turn":{"id":"turn-2","status":"interrupted"}}}'
      ;;
  esac
done
