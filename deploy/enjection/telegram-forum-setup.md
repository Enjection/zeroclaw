# Telegram forum-topics setup (Enjection deployment)

One-time setup to enable the **master topic + parallel per-topic sessions** feature.
Target: < 10 minutes.

## 1. Create a forum supergroup
1. In Telegram, create a new **group** and add your bot to it.
2. Open the group → **Edit** → **Topics** → toggle **ON**. The group becomes a forum
   supergroup with a **General** topic (this is your **master** control topic).

## 2. Promote the bot with topic permissions
1. Group → **Edit** → **Administrators** → **Add Admin** → select the bot.
2. Enable at least **Manage Topics** (and keep default send/message permissions).
   Without **Manage Topics**, `/new_topic` returns a clear preflight error.

## 3. Get the group chat_id
- In **any** topic, send `/chat_id`. The bot replies with the numeric `chat_id`
  (e.g. `-1001234567890`) and the current `thread_id`. `/chat_id` is intentionally
  **not** owner-gated so you can run it during setup before `owners` is configured.
- Alternatively, read it from the daemon trace on the first group message.

## 4. Get your own Telegram user_id (for the owner allowlist)
- Send `/chat_id` **from your own account** in a DM to the bot, or use a userinfo bot;
  note your numeric **user_id** (not the `@username` — usernames are reassignable and
  are rejected by the owner-gate on purpose).

## 5. Configure ZeroClaw
Edit `~/.zeroclaw/config.toml` (adjust the `telegram` alias to yours):

```toml
[channels.telegram.telegram]
# ...existing bot config...
group_ids = ["-1001234567890"]     # the forum supergroup from step 3

[channels.telegram.telegram.topics]
enabled = true
owners  = ["<your_numeric_user_id>"]   # numeric IDs only; who may run topic commands
default_icon_color = 7322096            # optional; Telegram preset color
```

Then restart the daemon:

```sh
ssh root@157.180.99.53 'XDG_RUNTIME_DIR=/run/user/0 systemctl --user restart zeroclaw'
```

## 6. Smoke test
In **General**:
- `/new_topic hello world` → the bot creates a "hello world" topic and confirms in it.
- `/new_topic research | You are my research assistant for project X` → creates the
  topic and the agent's first message there reflects the brief.
- `/topics` → lists your topics with status.
- Inside a topic: `/rename_topic new-name`, `/close_topic`, `/reopen_topic`.

## Notes & limits
- The usage endpoint for topic **names** is only populated from Telegram's
  `forum_topic_created/edited` service events; topics created **outside** the bot appear
  in `/topics` only after the bot observes such an event (Telegram has no "list all topics" API).
- **Master (General) is owner-scoped by default** — each person's General messages are
  their own session, so a stranger can't join your master session. Non-General topics are
  one shared session per topic. (To make General a single shared session for a fully-private
  owner-only group, flip the `master_shared` toggle — see the plan.)
- The scope behavior only changes for groups where `topics.enabled` is set for that alias.
