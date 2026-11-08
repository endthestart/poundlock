# Let an outside agent delegate coding to Poundlock

`poundlock mcp` lets an agent such as Claude Code use Poundlock durable jobs as
its hands. The agent plans, researches, reviews and tests. Every line of code is
written by a job on your own models, inside Poundlock's isolated runner, behind
checks the agent names. You do not need Poundlock Dispatch for this.

It is a [Model Context Protocol](https://modelcontextprotocol.io) server that
runs on the machine where the agent works and talks to your controller with one
application token.

## What it can do

| Tool | What it does |
|---|---|
| `list_models` | Lists the models that can run durable jobs, where each sends data, and whether it is busy. |
| `delegate_task` | Uploads a folder and starts a job for one bounded task: what to do, which files to change, the commands that prove it works (`acceptance_checks`), and paths it must not change (`frozen_paths`). |
| `job_status`, `wait_for_job` | Follow the job. A wait lasts at most 10 minutes; the agent waits again while the job runs. |
| `job_result` | Whether the checks passed, with their output; frozen paths the job touched; the files each patch changes; the end of the worker's report. |
| `get_patch` | The patch, for review. Large patches are saved to a file. |
| `apply_patch` | Applies the patch to the folder after checking it applies cleanly. |
| `cancel_job` | Cancels a job. |
| `provenance` | Lists the patches applied to a folder and every file changed some other way since the first. |

The runner runs the acceptance checks itself after the worker finishes, and
sends failures back to the worker for up to `max_repairs` rounds. A test listed
in `frozen_paths` cannot be edited to make it pass: the job fails verification
instead.

## Set it up

1. In the controller, open **Access** and create an application token with
   **List available models**, **Read shared jobs and results**, **Submit jobs
   with supplied files** and **Cancel shared jobs**. Add **Allow prompts and
   files to leave the controlled network** only if the agent may use External
   models.
2. Save the token in a file only you can read, such as
   `~/.config/poundlock/agent-token` with mode 600. Prefer a file to
   `POUNDLOCK_TOKEN`: environment variables and arguments are easier to leak.

3. Register the server with your agent. For Claude Code:

   ```bash
   claude mcp add poundlock -- poundlock mcp --controller https://poundlock.example.net --token-file ~/.config/poundlock/agent-token --root ~/Development
   ```

`--root` names the folders the agent may upload and apply patches to, with
everything beneath them. Repeat it for more than one. The agent cannot reach any
other folder through this server.

To share the setup with a project, put `config/examples/claude-code.mcp.json`
in the project as `.mcp.json` and edit the controller address and token path.
`--root .` then means the project folder Claude Code starts the server in.

`poundlock` is the same binary the node archives in each release carry
(`poundlock-macos-aarch64.tar.gz` and the others); put it on your `PATH`.

During a long `wait_for_job`, the server sends MCP progress notifications when
the client asks for them, so the agent sees the job's status while it waits.

## Tell the agent how to work

An agent follows the server's built-in instructions, but a line in your project's
`CLAUDE.md` or `AGENTS.md` makes the arrangement explicit:

```markdown
Do not edit source files yourself. Write code only through the Poundlock MCP
server: plan the change, then use delegate_task for one bounded task at a time,
naming the files to change and acceptance_checks that prove it works. Write the
tests first in their own task and list them in frozen_paths for the tasks that
implement them. Review job_result and get_patch before apply_patch, then run the
project's checks yourself.
```

`delegate_task` also takes `budget_minutes` and `budget_usd`: the job's own
limit on wall-clock time, repairs included, and on what it may spend on an
External model. `job_status` shows what it has spent.

Good tasks are small: one or two files of new logic and their tests. Name the
files, say what to reuse, and give checks that test behaviour a user would see,
not merely that the code builds.

To hold Claude Code to it, put `config/examples/claude-code.settings.json` in
the project as `.claude/settings.json`. It denies Claude Code's own file
editing tools and lets the read-only Poundlock tools run without asking;
`delegate_task`, `apply_patch` and `cancel_job` still ask each time unless you
allow them too. The agent keeps its shell, which it needs to run tests, so a
shell command could still write a file. The provenance check below finds that.

## Check that the code came from jobs

Each `apply_patch` into a Git folder records the folder's files, by content
hash, just before and just after the patch, in `.git/poundlock/`. The record
holds paths and hashes, never contents. Any file that changed between one
patch and the next, or since the last, changed some other way: by hand, by a
formatter, or by the agent's shell.

```bash
poundlock provenance ~/Development/my-app
```

lists the patches applied there and every such file, and whether each
patch's job had passed its checks when it was applied. It exits 1 if any file
changed outside jobs or any patch was applied without passing checks. The agent can run the same check with the `provenance` tool. Changes
before the first patch, such as a plan the agent wrote, are not counted. The
record lives where the agent can reach it, so it is evidence for review, not
proof against an agent that sets out to hide an edit.

## What leaves your machine

`delegate_task` uploads the folder you name:

- in a Git folder, the tracked files plus untracked files your ignore rules
  allow, including uncommitted edits;
- in any other folder, every file except dependency and build directories such
  as `node_modules` and `target`.

Credential files never leave: `.env` files (but not `.env.example`), private
keys, `.npmrc`, cloud credentials, Terraform state and the other names in
`src/mcp/snapshot.rs`. Symbolic links are never followed. The limits are 10,000
files and 50 MiB. Use `dry_run` to see exactly what would be sent.

The upload is encrypted in the controller's database until the job ends. The
prompt, results and events are kept. If the chosen model is External, the task
and the files the worker reads go to that provider.

## Large repositories: send the commit instead

A repository larger than 50 MiB, or one whose exact commit matters, can be
sent as a Git source: `delegate_task` with `"source": "git"`. The runner then
fetches the commit itself, so the folder must be the repository's top folder,
with everything committed and pushed, and its remote must be on a host the
controller approves (`POUNDLOCK_GIT_HOSTS`). An SSH remote such as
`git@github.com:owner/repo.git` is fetched over HTTPS. For a private
repository, start the server with `--git-token-file` (and `--git-username` if
the host needs one): the token goes to the controller with the job, encrypted
there until the job ends, and never to the agent.

## Applying patches

A patch is applied with `git apply`, after `git apply --check` succeeds, so a
patch that does not fit the folder changes nothing. Patches that touch Git
metadata or credential files are refused. Hooks never run. A patch is the
worker's code: the agent should read it before applying it, and the project's
own checks should run afterwards. A green check inside the job is evidence, not
acceptance.

To keep the job's work apart from yours, apply it as a branch instead:
`apply_patch` with `"branch": "job/greeting"` commits the patch as one new
branch on top of the folder's current commit, using a scratch index, so your
working tree, index and checked-out branch do not change. The commit's author
is `Poundlock job`, and its message carries `Poundlock-Job` and
`Poundlock-Verification` trailers, so history shows which code came from a job.
The commit is not signed. Review it with `git show`, then merge it or delete
it.
