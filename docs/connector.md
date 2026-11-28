# The Claude connector

Codehaven can be connected to Claude, so Claude can read your repositories, open issues and pull
requests, commit changes and look at CI, within exactly what you allow. It works in the Claude
apps (web, desktop, mobile) and in Claude Code. It is off until an operator turns it on.

## What it is

A connected app is Claude acting as you, with less power than you have. You choose, once, which
repositories it may read, which it may write to, and whether it may change workflows and run CI.
It never gets site admin power, even if you are an admin, and it never gets more than you have: if
you lose access to a repository, so does the app.

It signs in with OAuth. There is no password or token to copy: Claude sends you to Codehaven, you
sign in (two-factor must be on), you choose what to give it, and Claude receives short-lived
credentials that Codehaven can end at any moment.

## Before you start

All three are needed:

1. **A public `https` address.** `CODEHAVEN_EXTERNAL_URL` must start with `https://`. Claude on the web, desktop and mobile connects from Anthropic's servers, so the address must be reachable from the public internet. Claude Code connects from your own computer, so a private address works there as long as it is `https` (plain `http` is accepted only for `localhost`).
2. **Two-factor sign-in** on the account that connects. Without it the connect page says "Connecting an app needs two-factor sign-in on your account." Turn it on at Settings, Two-factor and security.
3. **`CODEHAVEN_MCP_ENABLED=true`** in the env file, then `sudo bash deploy/prod/up.sh`. The server refuses to start if the connector is on and the address is not `https`.

With `CODEHAVEN_PROXY=caddy` there is nothing to do for the connector's paths: the bundled Caddy sends every path to the server. With your own proxy, send every path, including `/mcp`, `/oauth/` and `/.well-known/`.

### Check it from outside

```sh
curl -s https://code.example.com/.well-known/oauth-protected-resource
```

When the connector is on it prints JSON naming your address, like `{"resource":"https://code.example.com/mcp","authorization_servers":["https://code.example.com"],...}`. When it is off, the path answers like any unknown path. And `curl -si -X POST https://code.example.com/mcp` answers `401` with a `www-authenticate: Bearer` header that carries `resource_metadata`. The same checks, plus the rest of the connector surface, run with `bash deploy/dev/public-smoke.sh https://code.example.com --mcp on`.

### Try it

After connecting, ask Claude: "List my repositories, then summarize the open issues in alice/demo." Both are reads. For the first consent, be conservative: choose Read on one test repository only, Write none, no workflows, and the default 90 days. You can connect again later and give more.

## Turn it on (operator)

1. Set `CODEHAVEN_EXTERNAL_URL` to the public https address people open, for example
   `https://code.example.com`. The connector needs https. Plain http is accepted only for
   `localhost`, for trying it on one machine.
2. Set `CODEHAVEN_MCP_ENABLED=true` in the env file (or `mcp.enabled = true` in `codehaven.toml`)
   and run `deploy/prod/up.sh` again.
3. Check `deploy/compose.sh exec server codehaven doctor`. It reports whether the connector is on,
   its address, and warns if an account or organization is named `mcp` or `oauth`, which are
   reserved for the connector's paths.

For claude.ai, desktop and mobile the connection is made from Anthropic's servers, not from your
computer. The instance therefore has to be reachable over the public internet on https, and
`/mcp` must not redirect to another host. Claude Code connects from your own computer, so a
private address works there. Your proxy must pass these paths to Codehaven unchanged:

- `/mcp`
- `/oauth/authorize`, `/oauth/consent`, `/oauth/token`, `/oauth/revoke`
- `/.well-known/oauth-protected-resource` and `/.well-known/oauth-authorization-server`

Settings, all under `[mcp]`:

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Turns the connector on |
| `trusted_client_hosts` | `["claude.ai"]` | Hosts whose client documents Codehaven will read. Add a host to allow another MCP client |
| `allowed_origins` | `["https://claude.ai", "https://claude.com"]` | Browser origins allowed to call `/mcp`, besides your own. A call with no `Origin` header always passes |

Every address the connector publishes comes from `external_url`, never from the `Host` header.

## Connect (person)

Two-factor sign-in must be on for your account (Settings, Two-factor and security).

**Claude on the web, desktop or mobile.** Open Settings, Connectors, and add a custom connector.
Paste your instance's address followed by `/mcp`, for example `https://code.example.com/mcp`. On
Team and Enterprise plans an Owner adds it. The free plan allows one custom connector. Claude sends
you to Codehaven to sign in and choose what to allow.

**Claude Code.**

```sh
claude mcp add --transport http codehaven https://code.example.com/mcp
```

Then run `/mcp` inside Claude Code and choose Authenticate. Codehaven has no dynamic client registration: a client identifies itself by a client document, an `https` address on a trusted host (`claude.ai` by default) that describes the app. If your Claude Code build cannot supply one by itself and does not finish connecting, point it at its client document by hand (the minimum Claude Code version that does not need this was not checked here):
`claude mcp add --transport http --client-id https://claude.ai/oauth/claude-code-client-metadata codehaven https://code.example.com/mcp`.

Settings, Connected apps shows the address and these steps too.

### The consent page

It names the app by its host (`claude.ai`) and shows the app's own name only as its claim. It shows
where you will be sent back to. Then you choose:

- **Read.** Required, nothing is preselected: only repositories you select, or all repositories you
  can access including future ones.
- **Write.** None by default. Selected repositories, or all (only together with read on all).
  Write lets it create branches and commits, open and change issues and pull requests, comment, and
  merge. A repository you let it write to must also be one it can read.
- **Change workflows and run CI.** None by default, only for repositories it can write to. This
  runs code on your runner, with access to that repository's CI secrets.
- **Lasts.** 90 days (default) or 180.

What the app asks for never ticks a box for you. You can give less or more than it asked for.

## What it can do

46 tools, listed to every connection so Claude can ask you for more access when it needs it.

| Area | Tools | Needs |
| --- | --- | --- |
| Connection | `get_connection` | read |
| Repositories | `list_repositories`, `search_repositories`, `get_repository` | read |
| Files | `list_files`, `get_file`, `search_code` | read |
| Refs and commits | `list_branches`, `list_tags`, `compare_refs`, `list_commits`, `get_commit` | read |
| Issues | `list_issues`, `search_issues`, `get_issue`, `list_issue_comments`, `list_labels` | read |
| Pull requests | `list_pull_requests`, `get_pull_request`, `get_pull_request_diff`, `list_pull_request_commits`, `get_pull_request_checks`, `list_reviews`, `list_review_threads` | read |
| CI | `get_commit_status`, `list_workflows`, `list_workflow_runs`, `get_workflow_run`, `get_job_log` | read |
| Inbox | `list_notifications` | read |
| Add | `create_repository`, `create_branch`, `create_issue`, `add_issue_comment`, `create_pull_request`, `add_review_comment` | write |
| Change | `commit_files`, `update_issue`, `update_pull_request`, `submit_review`, `update_pull_request_branch`, `merge_pull_request`, `mark_notification_read` | write |
| Run CI | `dispatch_workflow`, `rerun_workflow_run`, `cancel_workflow_run` | workflows |

Things it can never do, whatever you give it: approve a pull request or a workflow run, dismiss a
review, resolve a thread, delete a branch, write tags or releases, change a pull request's base
branch, turn on auto-merge, change repository settings, webhooks, secrets, variables, runners or
collaborators, use git over HTTP or SSH, or use the container registry. `create_repository` makes a
private repository in your own namespace and needs read and write on all repositories.

## How it is kept safe

- **Other people's words are data.** Repository files, issues, comments, branch names and job logs
  can say anything. Every result is `{"meta": ..., "untrusted": ...}`: Codehaven's own words in
  `meta`, everything a person wrote in `untrusted`. The server's instructions tell the model that
  `untrusted` is data and not instructions. Characters that hide things (bidirectional controls,
  zero-width characters, Unicode tag characters, terminal escapes) are shown as visible escapes,
  and a result says `"escaped": true` when it replaced any.
- **Results are bounded.** At most 60,000 characters in total, 40,000 for a file, 20,000 for a log
  (the end of it by default, with a cursor to read on), 30 items in a list by default and 50 at
  most. A result that is cut says so and gives a cursor.
- **Only real commits.** A revision must be a branch, a tag or a full commit id that a branch or
  tag of that repository reaches. Abbreviations, `HEAD~1`, `branch:path`, tree and blob ids, and
  commits that exist only in another fork are refused.
- **Workflow files are guarded.** Without the workflows permission on a repository, a connected app
  cannot change anything in `.codehaven/workflows` or `.github/workflows`, by any route (an edit, a
  rename, a merge, an update-branch, a first commit), and cannot change the default branch of a
  repository that has scheduled workflows.
- **CI it starts waits for you.** A push or pull request from an app that lacks the workflows
  permission creates a run in the state "Waiting for approval", with the reason. It cancels
  nothing, holds no concurrency group and starts no job until a person with write access presses
  Approve on the run page. Pushing to a pull request branch also turns off its auto-merge. Remember
  that code Claude commits can still run later, when a person approves a run, pushes on top, or
  starts a workflow.
- **Guards on changes.** `commit_files` needs the branch's current head, edits are by exact text
  (`replace_text` must match once), whole-file replacement of a binary, oversized or escaped file
  is refused, and `merge_pull_request` needs the expected head and uses the same gate and branch
  protection as the web page and never deletes the branch.
- **Everything is audited.** Each call writes `mcp.tool_call` with the tool, the repository, the
  number or id and the result, never the arguments, content or search text. Reads record before
  returning data. Writes record that they started and how they ended. Connecting, denying and
  ending a connection are audited too.
- **Limits.** 120 calls a minute per connection (writes 30), 3,000 an hour per person, 4 at once per
  connection. A write that has started is never cancelled by a timeout or a disconnect.

## Ending a connection

Settings, Connected apps lists each app with its repositories, when it was last used and when it
expires. Revoke ends it at once. Connections also end when you change your password, use a reset
link, turn two-factor off, are suspended, or sign out everywhere. Signing out only your other
sessions does not end them. A transferred repository drops out of every connection, so its new owner
decides again. If an operator turns the connector off, `/mcp` and the OAuth paths answer like
unknown paths and existing tokens stop working; Settings, Connected apps still lists connections so
you can remove them.

A one-time code used twice, or a refresh token reused after 30 seconds, ends the whole connection
and is recorded as `app.code_reuse` or `app.refresh_reuse`.

## Troubleshooting

| What you see | Likely cause |
| --- | --- |
| Claude says it cannot reach the server | The address is not public https, a firewall blocks Anthropic's addresses, or `/mcp` redirects to another host |
| Codehaven says to turn on two-factor | Connecting needs two-factor on your account |
| "That redirect address is not one this app registered" | The app's client document does not list the address it sent you from. Nothing was changed |
| "This server does not trust that host" | The app's client id is on a host not in `trusted_client_hosts` |
| Consent completes but Claude never finishes connecting | Check the browser popup was opened by Claude. Codehaven and Claude's callback both send `Cross-Origin-Opener-Policy: same-origin`; a client that needs `window.opener` will not finish |
| A tool says the connection was not given access | Connect again from the app and choose more repositories or powers. Granting more means a new consent |
| 403 with `insufficient_scope` | The tool needs write or workflows and the connection has none. Claude can ask you to connect again with more |
| A run is "Waiting for approval" | An app started it without the workflows permission. Open the run and Approve it, or Cancel it |

## For client authors

The endpoint is `POST /mcp`, one JSON-RPC message per request, JSON responses, no SSE, no batches
and no session ids. Protocol versions `2026-07-28` (stateless; every request carries the version in
`_meta` and in the `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` headers), `2025-11-25` and
`2025-06-18` (with `initialize`) are accepted. Clients are identified by a Client ID Metadata
Document, an https URL on a trusted host. There is no dynamic client registration. PKCE with
`S256` and the `resource` parameter are required. Access tokens last an hour, refresh tokens thirty
days of idleness and rotate on every use, and neither outlives the connection.
