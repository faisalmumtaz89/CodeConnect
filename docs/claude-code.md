# Claude Code sessions

A Claude Code session is started from the CodeConnect app: choose this computer, the project and
Claude Code. The daemon runs it through CodeConnect's agent host, which drives Claude Code with
the Agent SDK, through your login shell.

## Accounts

A Claude Code session on Claude Code's **Default** account runs as **you**, with your own Claude
Code sign-in and settings.

Each other Claude Code account you add in CodeConnect has a folder of its own,
`~/.codeconnect/accounts/claude/<id>`. Signing it in runs Claude Code's own `claude auth login`
with `CLAUDE_SECURESTORAGE_CONFIG_DIR` and `CLAUDE_CONFIG_DIR` both set to that folder: on a Mac,
Claude Code keeps the account's sign-in in your login keychain under a name of that folder's, and
its identity file in the folder; your own Claude Code sign-in is not touched. A session on that account starts with `CLAUDE_SECURESTORAGE_CONFIG_DIR` alone set to
the folder: it uses that account's sign-in, and your own settings and projects, where its
conversations are kept.

Choosing another account stops nothing, and new sessions use it at once. An open session moves to
it when you next send it a message, if nothing is running in it: it is stopped, and your message
resumes the same conversation on the new account. If something is running (a turn, a command or
task it started in its last turn, a message of yours it hasn't answered), that message goes on the
old account and the session moves at the next one. A session with a background job still running (a
dev server, a watcher) stays on its account until the job ends; a finished job lets it move with its
next message. A session already running when CodeConnect was updated to this version stays on its
account until its Claude Code ends, since what it started in the background before then was not
recorded. Its menus and settings stay as they are. A session that does not end within 5 seconds of
being stopped for its move is ended as End Run ends one. A process a
session started outside Claude Code's own tracking (a `cmd &`) ends when the session moves accounts.

For each Claude Code account, and the Default, the Accounts page shows the email and plan it is
signed in as (from `claude auth status`) and its plan's five-hour and seven-day usage, or that
it has no plan limits, as Claude Code reports them (asked through the agent host). They are read
when you open the page, when a sign-in finishes, and after a turn of a session, when Claude Code
reports its limits; each account at most once a minute, however many sessions are working, so the
figures shown can be up to a minute old. CodeConnect keeps them in
memory only, never on disk or in its log.

Removing an account runs `claude auth logout` with the same two variables, and the account is
removed only once Claude Code says it is signed out; its folder is then deleted. An account a
running session is on is not removed.
