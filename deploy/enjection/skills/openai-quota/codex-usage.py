#!/usr/bin/env python3
"""Report the ChatGPT/Codex subscription usage for this ZeroClaw instance.

Mirrors the Codex CLI `/status`: decrypts the OAuth token that ZeroClaw stores
(ChaCha20-Poly1305, `enc2:` format, key from ~/.zeroclaw/.secret_key) exactly as
the daemon does, then GETs the private usage endpoint the Codex CLI polls
(https://chatgpt.com/backend-api/wham/usage) and prints a compact summary.

Read-only. Never prints the token. Runs as the daemon user (root) so it can read
.secret_key. This endpoint is private/undocumented and may change.
"""
import json, time, sys, urllib.request, urllib.error

ZC = "/root/.zeroclaw"
SEC = ZC + "/.secret_key"
AUTH = ZC + "/auth-profiles.json"
PROFILE = "openai-codex:default"
URL = "https://chatgpt.com/backend-api/wham/usage"


def decrypt_token():
    from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
    key = bytes.fromhex(open(SEC).read().strip())
    prof = json.load(open(AUTH))["profiles"][PROFILE]
    v = prof["access_token"]
    if isinstance(v, str) and v.startswith("enc2:"):
        blob = bytes.fromhex(v[5:])
        v = ChaCha20Poly1305(key).decrypt(blob[:12], blob[12:], None).decode()
    return v, prof.get("account_id", "")


def human_dur(secs):
    secs = max(0, int(secs))
    d, secs = divmod(secs, 86400)
    h, secs = divmod(secs, 3600)
    m, _ = divmod(secs, 60)
    parts = []
    if d:
        parts.append(f"{d}d")
    if h:
        parts.append(f"{h}h")
    if m and not d:
        parts.append(f"{m}m")
    return " ".join(parts) or "0m"


def window_label(secs):
    return {18000: "5-hour", 604800: "weekly", 86400: "daily"}.get(
        int(secs or 0), f"{human_dur(secs)} window")


def fmt_window(w):
    if not w:
        return None
    used = w.get("used_percent", 0) or 0
    label = window_label(w.get("limit_window_seconds", 0))
    ra = w.get("reset_after_seconds")
    reset = f"resets in {human_dur(ra)}" if ra is not None else ""
    if w.get("reset_at"):
        reset += time.strftime(" (%Y-%m-%d %H:%M UTC)", time.gmtime(w["reset_at"]))
    return f"{label}: {used:.0f}% used" + (f" · {reset}" if reset else "")


def main():
    try:
        tok, acct = decrypt_token()
    except Exception as e:
        print(f"Couldn't read/decrypt the Codex auth token: {e}")
        return
    headers = {
        "Authorization": f"Bearer {tok}",
        "ChatGPT-Account-Id": acct,
        "Accept": "application/json",
        "User-Agent": "codex_cli_rs/0.20.0",
        "originator": "codex_cli_rs",
    }
    req = urllib.request.Request(URL, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            d = json.loads(r.read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as e:
        if e.code == 401:
            print("Couldn't read Codex usage: the OAuth token needs a refresh. "
                  "Send another message (the agent refreshes it on use) or run "
                  "`zeroclaw auth refresh --model-provider openai-codex`.")
        else:
            print(f"Couldn't read Codex usage (HTTP {e.code}).")
        return
    except Exception as e:
        print(f"Couldn't reach the Codex usage endpoint: {e}")
        return

    plan = d.get("plan_type", "unknown")
    email = d.get("email", "")
    out = [f"\U0001f9e0 OpenAI Codex usage — plan: {plan}" + (f" ({email})" if email else "")]
    rl = d.get("rate_limit") or {}
    if rl.get("limit_reached"):
        out.append("  ⚠️ rate limit currently REACHED")
    for key in ("primary_window", "secondary_window"):
        s = fmt_window(rl.get(key))
        if s:
            out.append("  " + s)
    for extra in d.get("additional_rate_limits") or []:
        name = extra.get("limit_name", "?")
        s = fmt_window((extra.get("rate_limit") or {}).get("primary_window"))
        if s:
            out.append(f"  {name} — {s}")
    cr = d.get("credits") or {}
    if cr.get("unlimited"):
        out.append("  credits: unlimited")
    elif cr.get("has_credits"):
        out.append(f"  credits: {cr.get('balance', '?')}")
    print("\n".join(out))


if __name__ == "__main__":
    main()
