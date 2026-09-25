# Notifications

When a chat you spawned settles, Zeron delivers a system message into your
chat — you do not poll.

## What arrives

Each child transition lands once, as a `[Zeron system]` prompt:

- `@chat:<id> completed:` followed by the child's last reply (long replies
  are trimmed; read the rest with `zeron chat output <id>`).
- `@chat:<id> failed.` Review the chat before deciding next steps.
- `@chat:<id> was interrupted.` Do not resume, restart, retry, replace or
  continue the work unless the user explicitly asks.
- `@chat:<id> needs help.` followed by what it is blocked on. Answer with
  `zeron chat tell` or resolve the pending question with `zeron chat answer`.

Several children settling close together arrive as one combined
`Child chat updates:` message. If you are mid-turn, delivery waits until
your turn ends — nothing interrupts your work.

## Acks

Running `zeron chat wait` or `zeron chat output` on a child from inside your
chat acknowledges the updates you have already seen; acknowledged updates are
never delivered again.

## Rules

Do not poll with sleeps or repeated `show`/`log` reads. Let children work and
react to the notification. Use `zeron chat wait` only when you genuinely need
a result before continuing.
