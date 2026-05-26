#!/usr/bin/env python3
"""Extract TUI-relevant constants from the installed Claude Code binary.

Output: a Markdown report grouped by topic, with verbatim values to diff
against `plugins/claude-code/strings.lua`. Run this on every Claude Code
version bump to spot string drifts before they break the classifier in
production.

Usage:
    python3 scripts/extract_claude_strings.py [path-to-claude-binary]

Defaults to the most-recent install under
`~/.local/share/claude/versions/`.
"""
import re
import sys
from pathlib import Path


def default_binary() -> Path | None:
    base = Path.home() / '.local/share/claude/versions'
    if not base.is_dir():
        return None
    files = [p for p in base.iterdir() if p.is_file()]
    if not files:
        return None
    return max(files, key=lambda p: p.stat().st_mtime)


def find_array_containing(data: bytes, needle: bytes,
                          max_back: int = 3000, max_forward: int = 10000) -> str | None:
    idx = data.find(needle)
    if idx == -1:
        return None
    start = data.rfind(b'[', max(0, idx - max_back), idx)
    end = data.find(b']', idx, idx + max_forward)
    if start == -1 or end == -1:
        return None
    raw = data[start:end + 1]
    text = raw.decode('latin-1', errors='replace')
    return re.sub(r'\\x([0-9a-fA-F]{2})',
                  lambda m: bytes([int(m.group(1), 16)]).decode('latin-1', 'replace'),
                  text)


def has(data: bytes, s: str) -> bool:
    return s.encode('utf-8') in data or s.encode('latin-1', errors='ignore') in data


def main() -> int:
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else default_binary()
    if binary is None or not binary.is_file():
        print(f'No Claude Code binary found; pass an explicit path.', file=sys.stderr)
        return 1
    data = binary.read_bytes()

    print(f'# Claude Code TUI string catalog — extracted from `{binary}`\n')

    # Turn completion verbs
    arr = find_array_containing(data, b'"Baked","Brewed"')
    print('## TURN_COMPLETION_VERBS\n')
    print('```json')
    print(arr or '<NOT FOUND>')
    print('```\n')

    # Spinner verbs
    arr = find_array_containing(data, b'"Accomplishing","Actioning"', max_back=200, max_forward=4000)
    print(f'## SPINNER_VERBS ({(arr or "").count(",") + 1 if arr else "?"} entries)\n')
    print('```json')
    print(arr or '<NOT FOUND>')
    print('```\n')

    # Error banners
    print('## ERROR_BANNERS\n')
    error_strings = {
        'rate_limit': [
            "You've used", "You're now using extra usage", "You're close to",
            "You're out of extra usage", "Now using extra usage",
        ],
        'quota': [
            "Credit balance is too low", "Prompt is too long",
            "PDF too large", "Image was too large",
        ],
        'auth': [
            "Not logged in · Please run /login",
            "Invalid API key · Fix external API key",
            "Your ANTHROPIC_API_KEY belongs to a disabled organization",
            "OAuth token revoked", "Authentication error",
        ],
        'connection': [
            "Unable to connect to API: SSL certificate is not yet valid",
            "Unable to connect to API: SSL error",
            "Unable to connect to API. Check your internet connection",
            "Unable to connect to API", "Connection error.", "Request timed out",
        ],
        'api': ["API Error", "API error (status"],
    }
    for category, items in error_strings.items():
        print(f'### {category}\n')
        for s in items:
            print(f'- {"✓" if has(data, s) else "✗"} `{s}`')
        print()

    # Permission / trust / model picker / welcome / usage / chrome
    for title, items in [
        ('PERMISSION_HEADERS', [
            "Do you want to proceed?", "Enter plan mode?", "Ready to code?",
            "Exit plan mode?", "Waiting for permission", "Approve", "Allow",
        ]),
        ('TRUST_DIALOG', [
            "Accessing workspace", "Yes, I trust this folder", "No, exit",
        ]),
        ('MODEL_PICKER', [
            "Select model", "Switch between Claude models", "Auto Mode Active",
            "Opus 4.7 only", "Opus 4.6/4.7, Sonnet 4.6",
        ]),
        ('WELCOME', [
            "Welcome to Claude Code", "Welcome back",
            "Tips for getting started", "What's new",
        ]),
        ('USAGE_PANEL', [
            "Total cost:", "Current week (all models)", "Loading usage data",
            "usage credits", "usage credit limit", "% of your",
            "session limit", "weekly limit", "Opus limit", "Sonnet limit",
        ]),
        ('TOOL_PROGRESS_CHROME', [
            "Running...", "Running…", "Running in the background",
            "(ctrl+o to expand)", "tool use", "tool uses", "Done",
        ]),
    ]:
        print(f'## {title}\n')
        for s in items:
            print(f'- {"✓" if has(data, s) else "✗"} `{s}`')
        print()

    # Tool names
    print('## TOOL_NAMES (verified)\n')
    candidates = [
        'Agent', 'AskUserQuestion', 'Bash', 'Brief', 'Config',
        'CronCreate', 'CronDelete', 'CronList', 'Edit', 'EnterPlanMode',
        'EnterWorktree', 'ExitPlanMode', 'ExitWorktree', 'Glob', 'Grep',
        'LSP', 'MCP', 'NotebookEdit', 'NotebookRead', 'PowerShell',
        'REPL', 'Read', 'RemoteTrigger', 'ScheduleWakeup', 'SendMessage',
        'Skill', 'Task', 'TaskCreate', 'TaskGet', 'TaskList', 'TaskOutput',
        'TaskStop', 'TaskUpdate', 'TeamCreate', 'TeamDelete', 'TodoWrite',
        'ToolSearch', 'WebFetch', 'WebSearch', 'Write', 'LS', 'MultiEdit',
        'NotebookRead', 'TodoRead', 'ScheduleCron', 'ListMcpResources',
        'ReadMcpResource', 'FileEdit', 'FileRead', 'FileWrite', 'McpAuth',
        'Sleep', 'SyntheticOutput',
    ]
    found = [t for t in candidates if f'"{t}"'.encode() in data or f"'{t}'".encode() in data]
    print(f'{len(found)} verified in this binary:\n')
    print('```lua')
    print('TOOL_NAMES = { ' + ', '.join(f'"{t}"' for t in sorted(set(found))) + ' }')
    print('```\n')

    return 0


if __name__ == '__main__':
    sys.exit(main())
