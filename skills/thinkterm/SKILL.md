---
name: thinkterm
description: Use when running inside ThinkTerm (THINKTERM=1 is set) and you need to look at other terminals, check on or coordinate other coding agents, send them text, or open new panes.
---

You are running inside ThinkTerm. Its command line, `thinkterm cli`, lets you see every
pane, read what other panes show, type into them, open new panes, and check on or
coordinate the coding agents running in them.

Run `thinkterm cli --help` for all commands and `thinkterm cli <command> --help` for
details. If `thinkterm` is not on PATH, it is in the folder named by the
WEZTERM_EXECUTABLE_DIR environment variable (thinkterm.exe on Windows).

Some commands affect what the user or other agents are using, such as typing into a
pane you did not open, closing panes or stopping the server; you may want to ask the
user first. Whether to ask is up to you.
