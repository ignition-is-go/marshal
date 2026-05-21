---
description: Configure Claude Code's statusLine to show this session's marshal nickname.
allowed-tools: Bash, Read, Edit, Write
---

Wire up the user's Claude Code `statusLine` to invoke `marshal-shim statusline` so the marshal nickname appears in the footer. The renderer is a subcommand of the shim binary, so the same command string works on every platform — no path resolution, no platform branching.

Do this in order:

1. **The block you intend to merge** is always the same:

   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "marshal-shim statusline"
     }
   }
   ```

   This assumes `marshal-shim` is on the user's `PATH` (which it must be already, since the plugin's MCP server entry invokes bare `marshal-shim`). If the user reports it isn't found at runtime, they should `cargo install marshal-shim` and restart Claude Code.

2. **Locate the settings file**:
   - Unix: `$HOME/.claude/settings.json`
   - Windows: `%USERPROFILE%\.claude\settings.json`

   If it does not exist, create it with `{}`. If it exists, read it.

3. **Show the user the block above** and ask if they want you to write it. Use AskUserQuestion with two options: "Write it" and "Just show me — I'll paste it myself". Do not write unprompted.

4. **If they say write it**: merge the `statusLine` key into the existing settings.json (preserve every other key, preserve formatting where reasonable). Use the `Edit` tool when the file exists so unrelated keys aren't disturbed; use `Write` only if the file was missing and you created it with `{}`. After writing, confirm the change took.

5. **If they already have a different `statusLine` configured**, surface that before doing anything destructive. Show the current value and the proposed value side by side, and ask whether to replace, skip, or back the current one up first. Default to *not* overwriting.

6. **Tell them to restart Claude Code** for the change to take effect — settings.json is only read at startup.

Keep the conversation short. This is a one-shot setup command, not a tutorial.
