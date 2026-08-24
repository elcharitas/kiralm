#!/usr/bin/env bash
# Bench one (engine, model, config) combo against a fixed prompt set.
# Usage: bench/run.sh <label> <mem_limit_gb|none> -- <llama-server extra args...>
# Model path and port are fixed; edit MODEL/PORT below or export overrides.
set -euo pipefail

LABEL="${1:?label required}"; shift
MEM_GB="${1:?mem limit in GB, or 'none' for native}"; shift
[[ "${1:-}" == "--" ]] && shift
EXTRA_ARGS=("$@")

MODEL="${MODEL:-$HOME/.ollama/models/blobs/sha256-7121486771cbfe218851513210c40b35dbdee93ab1ef43fe36283c883980f0df}"
PORT="${PORT:-8090}"
RESULTS="$(dirname "$0")/results.jsonl"
IMAGE="ghcr.io/ggml-org/llama.cpp:server"

# short / medium / long prompts, plain text files so curl -d @file works
PROMPT_DIR="$(dirname "$0")/prompts"
mkdir -p "$PROMPT_DIR"
[[ -f "$PROMPT_DIR/short.txt" ]] || echo "Explain what a hash map is in two sentences." > "$PROMPT_DIR/short.txt"
[[ -f "$PROMPT_DIR/medium.txt" ]] || python3 -c "print('Summarize the following. ' + 'The quick brown fox jumps over the lazy dog. '*180)" > "$PROMPT_DIR/medium.txt"
[[ -f "$PROMPT_DIR/long.txt" ]]   || python3 -c "print('Summarize the following. ' + 'The quick brown fox jumps over the lazy dog. '*120)" > "$PROMPT_DIR/long.txt"

cleanup() {
  if [[ -n "${CID:-}" ]]; then command docker rm -f "$CID" >/dev/null 2>&1 || true; fi
  if [[ -n "${SPID:-}" ]]; then kill "$SPID" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

start_ts=$(python3 -c 'import time; print(time.time())')

if [[ "$MEM_GB" == "none" ]]; then
  llama-server -m "$MODEL" --port "$PORT" -ngl 99 "${EXTRA_ARGS[@]}" \
    > "$(dirname "$0")/last_server.log" 2>&1 &
  SPID=$!
else
  CID=$(command docker run -d --rm \
    --memory="${MEM_GB}g" --memory-swap="${SWAP_GB:-$MEM_GB}g" \
    -p "${PORT}:8080" \
    -v "$(dirname "$MODEL")":/models \
    "$IMAGE" -m "/models/$(basename "$MODEL")" \
    --port 8080 -ngl 0 "${EXTRA_ARGS[@]}")
fi

# wait for server, cap 180s
for i in $(seq 1 180); do
  curl -s "http://localhost:${PORT}/health" | grep -q '"status":"ok"' && break
  sleep 1
done
load_s=$(python3 -c "import time; print(round(time.time() - $start_ts, 2))")

run_prompt() {
  local name="$1" file="$2"
  local body t0 t1
  body=$(python3 -c "import json,sys; print(json.dumps({'prompt': open('$file').read(), 'n_predict': 128, 'temperature': 0}))")
  t0=$(python3 -c 'import time; print(time.time())')
  resp=$(curl -s "http://localhost:${PORT}/completion" -d "$body")
  t1=$(python3 -c 'import time; print(time.time())')
  echo "$resp" | jq -c --arg label "$LABEL" --arg prompt "$name" --arg mem "$MEM_GB" \
    --argjson wall "$(python3 -c "print(round($t1-$t0,3))")" --argjson load "$load_s" \
    '{label: $label, prompt: $prompt, mem_gb: $mem, load_s: $load,
      wall_s: $wall, tokens_predicted: .tokens_predicted,
      tok_per_s: (if .timings then .timings.predicted_per_second else null end),
      prompt_tok_per_s: (if .timings then .timings.prompt_per_second else null end),
      ttft_ms: (if .timings then .timings.prompt_ms else null end)}' >> "$RESULTS"
}

run_prompt short "$PROMPT_DIR/short.txt"
run_prompt medium "$PROMPT_DIR/medium.txt"
run_prompt long "$PROMPT_DIR/long.txt"

echo "done: $LABEL (mem=$MEM_GB) -> $RESULTS"
tail -3 "$RESULTS" | jq -c '{label, prompt, mem_gb, load_s, tok_per_s, ttft_ms}'
