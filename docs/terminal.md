# The workspace terminal

The workspace of a repository (`/{owner}/{repo}/workspace`) has a terminal: a bash shell that runs
in your own browser, on a copy of the repository's files. The server starts no shell and no
container for it.

## What it is

A command line beside the editor, for searching, comparing and trying things without cloning the
repository. A fixed line under the terminal, outside its frame, says what it is:

> Runs in this browser. No internet. Not your CI machine. Codehaven never asks for a password
> here.

## What it can and cannot run

It runs bash syntax: pipes, loops and scripts, with grep, sed, awk, find, sort, diff, jq and more.

It cannot run git, make, Python, Node, compilers or any native program, and it has no internet.
Variables you export carry from one line to the next; functions and aliases do not.

Each command line can run up to 30 seconds and 100,000 commands, and print up to 1 MB. There is
no tar, gzip, sqlite3 or yq here.

## Your files

The terminal works on a copy of the repository's files at the commit the editor has open, in
`/work`, with your unsaved drafts on top. Up to 2,000 files or 25 MB; a bigger repository asks you
to pick a folder.

Later saves in the editor reach the terminal. A file changed on both sides keeps the terminal's
copy and says so.

## Moving changes into drafts

Nothing the terminal changes reaches your drafts on its own. "Review terminal changes" lists each
changed file, nothing ticked. You open a file before you can tick it.

Binary files, symbolic links, files inside `.git` and files over 2 MB are listed but cannot be
moved. Ticked files become drafts marked "From terminal".

Committing a terminal file shows its changes and the warning "CI may run this code with secrets",
and needs its own tick. Commit on save never commits a terminal file.

## Stopping a command

Stop or Ctrl+C ends the running command. After two seconds the shell is restarted from the files
as they were after the last finished command.

## Who can use it

Signed-in accounts only, like every workspace page: signed out, the terminal, its assets and its
file snapshot answer a redirect to `/login`. An account that can read the repository can use the
terminal. Committing still needs write access.

## How it is kept apart from your account

The shell runs in a Web Worker inside a sandboxed iframe: an opaque origin that may run scripts
and nothing else, with no network (`connect-src 'none'`). The frame never receives your cookie,
CSRF token, workspace key, user name or a URL with authority, and the page checks the shell's
bytes against this build's hashes before it starts. Any unexpected message closes the frame.

So a command cannot reach the internet, the instance or your session. What it can change is the
copy of the files, and only you can move those changes into drafts.

## For people who run Codehaven

Nothing extra to install or configure. The shell, just-bash 3.6.0 (Apache-2.0), and the drawing,
xterm.js 6.0.0 (MIT), are vendored with the server, so nothing is fetched from another site.

Serve Codehaven over HTTPS at a host name. On plain HTTP away from localhost, browsers do not
send `Sec-Fetch-Dest`, so the server's rule against framed requests is only a second lock, and
drafts cannot be encrypted. An IPv6 literal origin cannot use the workspace at all, because CSP
cannot name one.

Do not add a `Cross-Origin-Embedder-Policy` or a `frame-ancestors` rule of your own for the
workspace in a reverse proxy: the server sets the headers each workspace response needs. A proxy
that strips or rewrites `Content-Security-Policy` breaks the terminal.

The terminal is on by default. `[workspace] terminal` in the config file is the ceiling, and a
site admin with two-factor sign-in can turn it on or off at `/admin/workspace`, separately from
the editor. The change is audited and applies on the next request.
