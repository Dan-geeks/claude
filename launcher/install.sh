#!/usr/bin/env bash
# Installs the `claude` launcher in front of the real Claude Code.
# Needs: Claude Code installed and signed in; the claude-codex proxy (cargo install claude-codex --locked,
# or the prebuilt binary); `codex login` done if you want ChatGPT failover.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="${BIN_DIR:-$HOME/.local/bin}"
cfg="${CLAUDE_CODEX_HOME:-$HOME/.config/claude-codex}"
mkdir -p "$bin" "$cfg"

for f in claude claude-switch; do
  if [ -e "$bin/$f" ] && ! cmp -s "$here/$f" "$bin/$f"; then
    cp -p "$bin/$f" "$bin/$f.bak-before-claude-codex"
    echo "backed up existing $bin/$f"
  fi
  install -m 0755 "$here/$f" "$bin/$f"
done
[ -e "$cfg/instructions.md" ] || install -m 0644 "$here/instructions.md" "$cfg/instructions.md"
[ -e "$cfg/mode" ] || echo auto >"$cfg/mode"

echo "installed to $bin"
case ":$PATH:" in *":$bin:"*) ;; *) echo "note: $bin is not on PATH" ;; esac
echo "Make sure $bin comes BEFORE the directory holding the real claude on PATH, then run: claude"
echo "Edit your extra instructions in $cfg/instructions.md; switch plan with: claude-switch [auto|claude|codex]"
