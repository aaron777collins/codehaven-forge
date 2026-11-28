# The browser workspace

Every repository has a workspace at `/{owner}/{repo}/workspace`, opened from the repository page. It is a code editor, a commit box, a run panel and a terminal in one page, served entirely by your instance.

## What is in it

- **Editor.** Monaco, the editor inside VS Code, with a file tree, tabs, search and a diff. Unsaved edits are kept as drafts in the browser, encrypted, and locked when you sign out.
- **Commit.** Many files in one commit, to the current branch or a new one. When you commit to a new branch there is a link to open a pull request.
- **Runs.** The workflows of each commit with their live logs beside the code, with run, re-run and cancel.
- **Terminal.** A bash shell with pipes, loops, grep, sed, awk, find, sort, diff and jq, on a copy of the repository's files with your drafts on top. It runs in your browser, with no internet. Nothing it changes reaches your drafts until you tick it in review. Details are in [The workspace terminal](terminal.md).

## Who can use it

It is for signed-in accounts only. Nobody who is signed out can open any workspace page, file or stream, even for a public repository. A signed-in account can open the workspace of any repository it can read, and needs write access to commit or run a workflow. Streams close as soon as access ends.

The server starts no shell and no container for it, and every file the editor needs is served by the instance itself, with nothing loaded from another site.

## Turning it off

A site admin with two-factor sign-in can turn the editor off, and the terminal separately, at Site admin, Workspace (`/admin/workspace`). The terminal is on by default. Turning the editor off hides the workspace links, and the workspace pages answer "not found". Turning the terminal off applies to the next page load: a terminal that is already open runs until its page closes.

The server's `workspace.enabled` and `workspace.terminal` settings are the ceiling. The admin page can turn a part off, never on past them. See [Configuration](configuration.md).

## What it needs

Serve Codehaven over `https` at a host name. On plain `http` away from `localhost`, browsers do not send the header the server uses to refuse framed requests, and drafts cannot be encrypted. An IPv6 literal address cannot use the workspace at all, because a content security policy cannot name one. Do not add your own `Cross-Origin-Embedder-Policy` or `frame-ancestors` rule for the workspace in a proxy, and do not let a proxy rewrite `Content-Security-Policy`: it breaks the terminal.
