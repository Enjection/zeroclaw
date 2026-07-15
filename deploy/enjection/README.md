# Enjection deployment artifacts

Personal-fork deployment assets for the ZeroClaw instance running on the prod
host (Ubuntu, systemd **user** service `zeroclaw.service`, Postgres-backed
memory). These are **not** part of the ZeroClaw build — they're config/runtime
artifacts kept here for reproducibility.

## `skills/openai-quota/`

A ZeroClaw skill that lets the agent report the ChatGPT/Codex **subscription
usage quota** on demand (e.g. over Telegram).

- `codex-usage.py` — decrypts this instance's stored OAuth token (ChaCha20-Poly1305,
  `enc2:` format, key from `~/.zeroclaw/.secret_key`) exactly as the daemon does,
  then GETs the private Codex usage endpoint the Codex CLI polls
  (`https://chatgpt.com/backend-api/wham/usage`) and prints a compact summary
  (plan, weekly/5-hour windows, % used, resets). Read-only; prints no secrets.
  Requires the python `cryptography` package.
- `SKILL.md` — instructs the agent to run the helper and relay its output.

### Install on the box

The helper must live **inside the agent workspace** so the shell tool's path
guard (`PathGuardedTool`) allows it (paths outside `workspace_dir` are blocked):

```sh
# helper -> agent workspace (adjust <agent> to your agent alias)
install -m 0755 codex-usage.py \
  /root/.zeroclaw/agents/<agent>/workspace/codex-usage.py

# skill -> a bundle attached to the agent
zeroclaw skills bundle add local
cp SKILL.md /root/.zeroclaw/shared/skills/local/openai-quota/SKILL.md   # (after `zeroclaw skills add openai-quota --bundle local`)
zeroclaw config set agents.<agent>.skill_bundles '["local"]'
# then restart the daemon
```

Note: the endpoint is private/undocumented and may change. Shell approvals under
the `locked_down` risk profile prompt in-channel; add the command to
`risk_profiles.<profile>.auto_approve` to run it without a prompt.
