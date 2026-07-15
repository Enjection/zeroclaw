---
name: openai-quota
description: >-
  Check the current OpenAI / ChatGPT-Codex subscription usage and rate-limit quota: plan, weekly and 5-hour windows, percent used, and reset times. Use whenever the user asks about their OpenAI quota, Codex usage, how much of their limit or messages are left, whether they are rate limited, or when their limit resets.
version: 0.1.0
---

# OpenAI / Codex quota

Use this skill whenever the user asks about their OpenAI / ChatGPT-Codex
subscription usage or limits — for example: "what's my OpenAI quota", "how much
Codex usage is left", "am I rate limited", "when does my limit reset", "check my
subscription usage".

## How to answer

Run this helper with the shell tool (the `python3` command is allow-listed), then
relay its output to the user:

```
python3 /root/.zeroclaw/agents/knot_knitter/workspace/codex-usage.py
```

It prints the plan, each rate-limit window (weekly / 5-hour) with percent used
and reset time, and any per-model limits — the same data Codex CLI shows via
`/status`. Report that summary concisely; do not dump raw JSON.

## Notes

- The script decrypts this instance's own stored OAuth token locally (the same
  ChaCha20-Poly1305 `.secret_key` the daemon uses) and queries the private Codex
  usage endpoint `https://chatgpt.com/backend-api/wham/usage`. It is read-only
  and prints no secrets.
- This endpoint is private and undocumented; if OpenAI changes it the script may
  need updating.
- If the script says the token needs a refresh, tell the user to send another
  message (the token refreshes on use) or to run
  `zeroclaw auth refresh --model-provider openai-codex`.
