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

Choosing another account moves each running session to it between turns: once a session's turn
has ended it is stopped, and your next message to it resumes the same conversation on the new
account.

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
