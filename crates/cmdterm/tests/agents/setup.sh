#!/bin/sh
# Configures the agents in the harness container for one working directory:
# no onboarding, the fake API (fake_api.py) as their model provider, the
# directory trusted, shell commands run without asking (the sandbox tests
# have the model ask for some). Usage: setup.sh WORKDIR
# HARNESS_PORT is the fake API's port (default 8787).
set -eu
workdir=$1
key=$ANTHROPIC_API_KEY
port=${HARNESS_PORT:-8787}
mkdir -p "$HOME/.claude" "$HOME/.codex" "$HOME/.qwen"

# Claude Code: onboarding done, the fake key approved, the folder trusted.
cat > "$HOME/.claude.json" <<EOF
{
  "hasCompletedOnboarding": true,
  "theme": "dark",
  "autoUpdates": false,
  "customApiKeyResponses": { "approved": ["$(printf '%s' "$key" | tail -c 20)"], "rejected": [] },
  "projects": {
    "$workdir": {
      "hasTrustDialogAccepted": true,
      "hasCompletedProjectOnboarding": true,
      "hasClaudeMdExternalIncludesApproved": false,
      "hasClaudeMdExternalIncludesWarningShown": true,
      "allowedTools": []
    }
  }
}
EOF
cat > "$HOME/.claude/settings.json" <<EOF
{ "permissions": { "allow": ["Bash"] } }
EOF

# Codex: a provider on the fake Responses API, the folder trusted, hooks on.
cat > "$HOME/.codex/config.toml" <<EOF
model = "harness-model"
model_provider = "harness"
check_for_update_on_startup = false
approval_policy = "never"
sandbox_mode = "danger-full-access"

[model_providers.harness]
name = "harness"
base_url = "http://127.0.0.1:$port/v1"
wire_api = "responses"
env_key = "OPENAI_API_KEY"
request_max_retries = 0
stream_max_retries = 0

[projects."$workdir"]
trust_level = "trusted"

[features]
codex_hooks = true
EOF

# Qwen Code: OpenAI-compatible auth on the fake chat API.
cat > "$HOME/.qwen/settings.json" <<EOF
{
  "security": { "auth": { "selectedType": "openai" }, "folderTrust": { "enabled": false } },
  "model": { "name": "harness-model" },
  "tools": { "approvalMode": "yolo" },
  "ui": { "hideTips": true },
  "general": { "disableAutoUpdate": true, "disableUpdateNag": true }
}
EOF
