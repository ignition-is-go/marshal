---
description: Configure Claude Code's statusLine to show this session's marshal nickname.
allowed-tools: Bash, Read, Edit, Write
---

Wire up the user's Claude Code `statusLine` to point at this plugin's helper script so the marshal nickname appears in the footer.

Do this in order:

1. **Resolve the plugin's bin directory.** It is `${CLAUDE_PLUGIN_ROOT}/bin`. That env var is set for you — do not guess paths. If it is empty for any reason, stop and tell the user.

2. **Detect the user's platform** with a single `Bash` call (`uname -s` is enough — `Linux`, `Darwin`, or something Windows-ish like `MINGW*`/`MSYS*`/`CYGWIN*`). On native Windows running PowerShell/cmd, `uname` may not exist; treat command-not-found as "Windows".

3. **Pick the command string** based on platform:
   - Linux / macOS:  `bash "${CLAUDE_PLUGIN_ROOT}/bin/statusline.sh"` — but with `${CLAUDE_PLUGIN_ROOT}` **expanded** to its current absolute value, not left as a variable. settings.json is read once at launch and won't re-expand env vars.
   - Windows: `powershell -NoProfile -ExecutionPolicy Bypass -File "<expanded plugin root>\bin\statusline.ps1"` — same rule: expand the path first, escape backslashes in JSON.

4. **Locate the settings file**:
   - Unix: `$HOME/.claude/settings.json`
   - Windows: `%USERPROFILE%\.claude\settings.json`

   If it does not exist, create it with `{}`. If it exists, read it.

5. **Show the user the JSON block you intend to merge in** — the full `"statusLine": { ... }` object with the resolved command string — and ask if they want you to write it. Use AskUserQuestion with two options: "Write it" and "Just show me — I'll paste it myself". Do not write unprompted.

6. **If they say write it**: merge the `statusLine` key into the existing settings.json (preserve every other key, preserve formatting where reasonable). Use the `Edit` tool when the file exists so unrelated keys aren't disturbed; use `Write` only if the file was missing and you created it with `{}`. After writing, confirm the change took.

7. **If they already have a different `statusLine` configured**, surface that before doing anything destructive. Show the current value and the proposed value side by side, and ask whether to replace, skip, or back the current one up first. Default to *not* overwriting.

8. **Tell them to restart Claude Code** for the change to take effect — settings.json is only read at startup.

Keep the conversation short. This is a one-shot setup command, not a tutorial.
