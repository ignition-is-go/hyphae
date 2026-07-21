# Git commits

- No co-author lines
- No emoji footers

<!-- levi:begin -->
## Task tracking (levi)

This repo tracks tasks with levi, a git-aware issue tracker. State lives in
the repo itself (`refs/levi/events`); status is resolved against git
ancestry, so a task closed at commit X counts as closed only on checkouts
that contain X. Every read command takes `--json` (stable schemas) — prefer
it when parsing.

- **Pick work**: `levi next --claim --json` returns the most important
eligible task, claims it for this dev/machine/worktree (so parallel agents
never grab the same task), and tells you why it ranked first. If you stop
working on a task, release it: `levi drop <id>`.
- **Inspect**: `levi ls --json` (open on this checkout), `levi show <id>
--json` (body, deps, claim, comments, status history).
- **Create**: `levi add "title" [-p p0..p3] [-b body] [-l label]
[--dep <blocker-id>]` — file follow-ups you discover instead of fixing
drive-by; link blockers with `--dep`/`levi dep add`.
- **Complete**: commit the work first, then `levi close <id>` — the close
anchors at HEAD, so it only applies where the fixing commit exists
(feature-branch closes stay open on main until merged; that is correct).
`--no-anchor` is only for tasks unrelated to code state.
- **Reopen** regressions with `levi reopen <id>`; leave context with
`levi comment <id> "text"`.
- Sync is opportunistic after every mutation; `levi sync` forces a full
git-remote + hub exchange.
- **Cross-project**: file upstream bugs with `levi add --project <name>
"title"`; link with `levi dep add <id> --on <project>/lv-xxxx --via
"<how this repo consumes that project>"`. When a foreign blocker
closes, verify the fix is actually reachable through the `via`
mechanism (published release, updated pin, ...) before starting work.
<!-- levi:end -->
