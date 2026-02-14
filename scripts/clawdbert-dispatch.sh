#!/usr/bin/env bash
set -euo pipefail

# ClawdBert Dispatch Script
# Bridge between VK executor process model and OpenClaw gateway sessions.
#
# VK sets these env vars:
#   VK_PROJECT_NAME    - project name
#   VK_PROJECT_ID      - project UUID
#   VK_TASK_ID         - task UUID
#   VK_WORKSPACE_ID    - workspace UUID
#   VK_WORKSPACE_BRANCH - git branch (e.g. vk/abc123-fix-cors)
#   VK_SESSION_ID      - VK execution session UUID
#
# The task prompt (card description) arrives on stdin.

GATEWAY_URL="${CLAWDBERT_GATEWAY_URL:-http://127.0.0.1:18789}"
GATEWAY_TOKEN="${CLAWDBERT_GATEWAY_TOKEN:?CLAWDBERT_GATEWAY_TOKEN must be set}"

BRANCH="${VK_WORKSPACE_BRANCH:-${VK_BRANCH_NAME:-default}}"
SESSION_LABEL="clawdbert-${VK_PROJECT_NAME:-unknown}:${BRANCH}"

# Read prompt from stdin
PROMPT=$(cat)

if [ -z "$PROMPT" ]; then
    echo "Error: No prompt provided on stdin" >&2
    exit 1
fi

# Trap SIGTERM for graceful shutdown
cleanup() {
    echo "Received SIGTERM, cleaning up..." >&2
    # TODO: send cancel to gateway session
    exit 143
}
trap cleanup TERM

echo "Dispatching to OpenClaw: ${SESSION_LABEL}" >&2
echo "  Project: ${VK_PROJECT_NAME:-?}  Task: ${VK_TASK_ID:-?}" >&2
echo "  Branch:  ${BRANCH}  Worktree: ${PWD}" >&2

# Build the task message with VK context
TASK_MESSAGE=$(cat <<EOF
# VK Task Dispatch

**Project:** ${VK_PROJECT_NAME:-unknown}
**Branch:** ${BRANCH}
**Worktree:** ${PWD}
**Task ID:** ${VK_TASK_ID:-unknown}

## Assignment

${PROMPT}
EOF
)

# Spawn isolated session on the gateway
SPAWN_RESPONSE=$(curl -sf -X POST "${GATEWAY_URL}/api/sessions/spawn" \
    -H "Authorization: Bearer ${GATEWAY_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "{
        \"label\": \"${SESSION_LABEL}\",
        \"task\": $(printf '%s' "$TASK_MESSAGE" | jq -Rs .),
        \"model\": \"anthropic/claude-sonnet-4-5\"
    }") || {
    echo "Error: Failed to spawn session" >&2
    echo "Response: ${SPAWN_RESPONSE:-<empty>}" >&2
    exit 1
}

RUN_ID=$(echo "$SPAWN_RESPONSE" | jq -r '.runId // empty')
SESSION_KEY=$(echo "$SPAWN_RESPONSE" | jq -r '.sessionKey // empty')

if [ -z "$RUN_ID" ]; then
    echo "Error: No runId in spawn response" >&2
    echo "Response: $SPAWN_RESPONSE" >&2
    exit 1
fi

echo "Session spawned: key=${SESSION_KEY} run=${RUN_ID}" >&2

# Poll for completion
POLL_INTERVAL=10
while true; do
    sleep "$POLL_INTERVAL"

    # Check if the run is still active by listing sessions
    STATUS=$(curl -sf "${GATEWAY_URL}/api/sessions?label=${SESSION_LABEL}" \
        -H "Authorization: Bearer ${GATEWAY_TOKEN}" || echo '{}')

    # Check if session's last run is still going
    ABORTED=$(echo "$STATUS" | jq -r '.sessions[0].abortedLastRun // false')

    if [ "$ABORTED" = "true" ]; then
        echo "Session aborted/failed" >&2
        exit 1
    fi

    # TODO: better completion detection via gateway API
    # For now, rely on the spawn timeout
done
