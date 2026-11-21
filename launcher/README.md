# `claude` launcher with Claude -> ChatGPT failover

Run `claude` as usual. Behind it, a small local proxy sends your requests to your
**Claude plan**, and, if that plan hits its usage limit, to your **ChatGPT (Codex)
plan** until the limit resets. The model name Claude Code shows stays the same.

## Install

1. Install Claude Code and sign in with Claude (`claude`, then `/login`).
2. Install the proxy: `cargo install claude-codex --locked` (or this repo: `cargo install --path .`).
3. For ChatGPT failover, install the Codex CLI and run `codex login`.
4. `bash launcher/install.sh`, then make sure `~/.local/bin` is **before** the real
   `claude` on `PATH`. Run `claude`.

The installer backs up any existing `claude`/`claude-switch` it would replace. To undo
it, delete `~/.local/bin/claude` and `~/.local/bin/claude-switch`.

## Switching plans

```sh
claude-switch            # show the current mode
claude-switch auto       # Claude first, ChatGPT when Claude is rate limited (default)
claude-switch claude     # Claude only, never fails over
claude-switch codex      # ChatGPT only (no Claude login needed)
CLAUDE_BACKEND=codex claude   # one run only
```

A mode change restarts the local proxy the next time you start `claude`.

## What failover does

- Any HTTP 429 from Claude's API counts as a limit. The same request is re-sent to
  Codex, and the limit is remembered for the time the response says (`retry-after`),
  between 30 seconds and 6 hours (5 minutes if none is given). During that time requests
  go straight to Codex without trying Claude. After it, Claude is tried again.
- Other Claude errors (400, 401, 5xx) are returned as they are.
- The reply's model name is rewritten to the one Claude Code asked for, so a session on
  Fable still reports Fable. Turn this off with `CCP_FAILOVER_MASK_MODEL=0`.
- Reasoning and context carry across the switch (existing proxy behaviour).
- Each failover is logged as `claude usage limit reached; failing over to codex` in
  `~/.config/claude-codex/proxy.log`.

## Model mapping

A Claude model sent to Codex maps to the first of these that your Codex account lists:

| Claude model | Codex model (first available wins) |
|---|---|
| `claude-fable-5-2` | `gpt-6.1-sol`, `gpt-6-sol`, `gpt-6-astra`, `gpt-5.6-sol` (newest first) |
| `fable`, `claude-fable-5`, `claude-fable-5-1` | `gpt-6-sol`, `gpt-6.1-sol`, `gpt-6-astra`, `gpt-5.6-sol` |
| `opus`, `claude-opus-*` | same list as older Fable |
| `sonnet`, `claude-sonnet-*` | `gpt-5.6-terra` |
| `haiku`, `claude-haiku-*` | `gpt-6-luna`, `gpt-5.6-luna` |

The list lives in `src/providers/codex/translate/model_allowlist.rs`. You can also pick
a model by hand in Claude Code, e.g. `/model gpt-6-astra[1m]` or `/model claude-opus-5-5`.

## Use it from another machine

Run the proxy on a machine that stays on (a server, your desktop), signed in to Claude
and Codex there, and point `claude` on any other machine at it through an SSH tunnel.
Nothing is exposed to the internet:

```sh
ssh -N -L 18765:127.0.0.1:18765 my-server &
CLAUDE_CODEX_REMOTE=http://127.0.0.1:18765 claude
```

The backend mode (`auto`, `claude`, `codex`) is the one set on the server with
`claude-switch`. Models are still chosen with `/model` inside Claude Code. To reach the
proxy without a tunnel, start it with `CCP_BIND_ADDRESS=0.0.0.0 CCP_AUTH_TOKEN=<secret>`,
put TLS in front of it, and set `CLAUDE_CODEX_KEY=<secret>` on the client.

## Your own instructions

`~/.config/claude-codex/instructions.md` is appended to Claude Code's system prompt on
every start. Edit it, or empty it to turn it off. For project rules, Claude Code's own
`CLAUDE.md` files still work as normal.

## Before you give this to other people

- **Use each person's own plans.** The Claude side forwards the login Claude Code already
  has, and the Codex side uses the `codex login` on that machine. Sharing one person's
  Claude or ChatGPT login with others, or hosting the proxy for them on your accounts,
  goes against Anthropic's and OpenAI's terms and risks the account.
- **Tell users about failover.** Model masking is on by default, so people on another
  backend may not know it. For anyone but yourself, set `CCP_FAILOVER_MASK_MODEL=0`, or at
  least say so. Answers can differ between models.
- The proxy only listens on `127.0.0.1` and has no login of its own.
