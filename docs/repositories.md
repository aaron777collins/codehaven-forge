# Repositories

Making, importing, sharing and looking after repositories: the three visibility tiers, access, forks, branches and branch protection, releases and webhooks. Issues, pull requests and review are in [Issues and pull requests](pull-requests.md).

## Make or import a repository

**New repository** (`/new`). Choose the owner (you, or an organization you own), a name, an optional description and a visibility, and whether to add a README. A name uses letters, numbers, dots, dashes and underscores. It becomes part of the address, `https://code.example.com/<owner>/<name>`. A repository starts private. Choosing Public creates it private first, and you confirm on the next page.

**Import a repository** (`/new/import`). Copies a repository from another server. The address must be a git repository anyone can clone over `https`. Branches and tags come across; issues and pull requests do not. The limit is 2 GB. The imported copy is private or internal, never public: make it public afterwards if you want that. The server fetches the address itself, so it refuses private and loopback addresses and plain `http` unless the operator set `imports.allow_private` (see [Configuration](configuration.md)).

Every new repository gets nine starting labels: bug, enhancement, documentation, question, good first issue, help wanted, breaking, duplicate and wontfix.

## Who can see a repository

A repository is in one of three tiers. The tier is set when it is made and changed under Settings, General, Visibility.

| Tier | Who can read it |
| --- | --- |
| Private | Only people you give access to |
| Internal | Also every signed-in account on this server |
| Public | Also anyone on the internet, without an account, while the site admin allows public repositories |

A repository is private by default. Its owner can widen it to every signed-in account, or to anyone on the internet while the site admin allows public repositories. Contributing needs an account: nobody who is signed out can open an issue, comment, review or send a pull request, and there is no sign-up page, so an account means an invite.

**Public is off until the site admin turns it on.** A site admin does it at Site admin, Settings, or with `codehaven admin public-repos on`. Turning it on makes nothing public by itself. Turning it off lowers every public repository to internal at once.

**Who can make a repository public.** The account that owns it, an owner of the organization that owns it, or a site admin who has two-factor on and is in a browser session. The page checks the repository first, and you must type its name and tick that you checked the code and its history for secrets and personal information. Making a repository public publishes its whole history: every commit on every branch and tag, files deleted in later commits, the names and email addresses inside commits, and its issues, pull requests, comments, releases and listed packages. Earlier Actions runs, logs, artifacts, checks and statuses stay hidden. Making it private later cannot take back copies. If a secret was ever committed, change that secret first.

Before a repository is widened, Codehaven rebuilds its store from its refs alone, so deleted history that git would otherwise still serve is not published by accident. The old store is kept unreachable for 7 days and then removed.

**Who can contribute.** For an internal or public repository, the owner chooses under Settings, General: "Everyone signed in who can read it" (the default) may open issues, comment and send pull requests, or "Only people with access". It also covers reactions and reviews. Pushing, merging and triage always need access.

**What a signed-out visitor sees.** Of a public repository: code and history, issues, pull requests and comments, releases and tags, the file tree, blame and compare, and Actions runs and logs recorded while it was public. Never settings, the workspace, the editor or the terminal, and never a write of any kind. A private repository, an internal repository and one that does not exist all look the same to someone who is signed out.

Pushing over git, SSH or the API always needs a credential. A signed-out clone of a public repository is allowed and rate limited.

## Access

Add people at Settings, Collaborators: a username and a permission.

| Permission | What it allows |
| --- | --- |
| Read | Clone, open issues and pull requests, comment |
| Triage | Also manage labels, assignees and milestones, and close issues |
| Write | Also push and merge |
| Maintain | Also change settings: General, Branches and Webhooks |
| Admin | Also manage collaborators, secrets and variables, environments, Pages and the danger zone |

Teams in an organization, and the organization's default repository permission, give access too. A site admin gets no extra power over a repository unless they have two-factor on and are in a browser session. An archived repository is read only for everyone, including its owner, until it is unarchived (Settings, General, danger zone).

## Rename, transfer, archive and delete

All are under Settings, General, danger zone, and need Admin.

- **Rename.** The web address changes with it. Update your git remotes and links.
- **Transfer.** The new owner gets full control. You type the repository's name to confirm. A transferred repository drops out of every connected app, so its new owner decides again.
- **Archive.** Read only for everyone until unarchived.
- **Delete.** Removes the code, issues and pull requests for good. There is no undo. You type the full name to confirm.

## Forks

A fork is your own copy of a repository. Choose Fork on the repository page, pick your account or an organization you own, and a visibility no wider than the source. A fork can never be more visible than the repository it came from. A public fork starts as internal and is confirmed on the visibility page.

A fork copies every branch and tag, so it borrows nothing from its source. Send changes back with a pull request from the fork. `/{owner}/{repo}/forks` lists the forks you can read.

If a repository with forks is deleted, its forks are kept. Its oldest surviving fork becomes the new root of the network, and no fork becomes more visible than it was.

## Branches and tags

`/{owner}/{repo}/branches` and `/tags` list them, and a branch can be deleted there. The first branch pushed to an empty repository becomes the default branch (`main`, then `master`, then the first by name). Change it under Settings, General. The default branch can never be deleted.

Only `refs/heads/*` and `refs/tags/*` can be pushed.

### Protected branches

Settings, Branches. A rule has a branch name or a pattern, and these options:

| Option | What it does |
| --- | --- |
| Required approvals, 0 to 6 | How many approving reviews a pull request needs before it can merge |
| Dismiss stale reviews | A new push removes earlier approvals |
| Require code owner review | The people in `CODEOWNERS` for the changed files must approve |
| Require resolved review threads | Every review conversation must be resolved |
| Require the branch to be up to date | The pull request must contain the base branch's current tip |
| Block force push | Refuses pushes that rewrite history |
| Block deletion | Refuses deleting the branch |
| Allow direct push without a pull request | Lets people with write access push straight to the branch |
| Required checks | One check name per line, exactly as it shows on a pull request, such as `ci / test`. Every one must pass |

A pattern may use `*` for anything except `/`, `**` for anything including `/`, and `?` for one character. `release/*` protects every branch one level under `release`. When several rules match a branch, the strictest value of each option wins. A branch that changes only through pull requests cannot be deleted either: allow direct push, or remove the rule, to delete it.

A protected branch refuses direct pushes, force pushes and deletion, enforced in a git hook on the server, so it holds for the web editor, the API and every client. A pull request merge may land, and creating a branch under a pattern is allowed so that a pull request can be opened. No one bypasses a rule, a site admin included.

The list under Required checks offers the names of checks that ran on recent commits. A name that never ran on a pull request's head commit blocks the merge, and the page names the nearest one that did. If CI cannot be asked, required checks count as not passed.

`CODEOWNERS` is read from the base branch, from `.codehaven/CODEOWNERS`, `.github/CODEOWNERS`, `CODEOWNERS` or `docs/CODEOWNERS`, in that order. Owners are `@username` entries; a team of the organization that owns the repository counts as `@org/team`.

## Releases

Releases and tags are listed at `/{owner}/{repo}/releases`. Write access makes one: choose a tag, write notes in Markdown (type `#` to mention an issue and `@` to mention a person), and mark it a pre-release or a draft. A draft is seen only by people with write access. A release can carry files, each with a size and a SHA-256 digest. `/{owner}/{repo}/releases/latest` and `/releases/latest/download/<name>` give stable links to the newest. Publishing a release can start workflows that listen for `release`, and notifies watchers.

## Webhooks

Settings, Webhooks. Add a payload address that starts with `http://` or `https://`, an optional secret and the events to send: push, pull request, issues, issue comment, pull request review, release, workflow run, create, delete, check run, status and member. A repository or organization may have 20.

- Each delivery is signed with HMAC-SHA256 of the exact body in `X-Hub-Signature-256` when you set a secret. `X-Codehaven-Event` and `X-GitHub-Event` name the event, and a delivery id is included.
- A failed delivery is retried at 1 minute, 5 minutes, 30 minutes, 2 hours and 6 hours, six attempts in all. Redirects are never followed.
- Private, loopback, link-local and cloud-metadata addresses are refused, when the hook is saved and again on every delivery, unless the operator allowed them. The host's own public addresses are always refused.
- Payloads use GitHub's field names for the fields common tools read: `repository`, `sender`, `issue`, `pull_request`, `comment`, `review`, `release`, `ref`, `before` and `after`. They are not byte for byte GitHub's. The `commits` list in a push payload is empty.

Deliveries, and organization-wide hooks, are managed through the REST API under `/api/v3/repos/{owner}/{repo}/hooks` and `/api/v3/orgs/{org}/hooks`.

## Git access

Clone with the HTTPS address the repository page shows, or the SSH one. Both need credentials to push, and for a private or internal repository to clone.

- **HTTPS.** Username and a personal access token as the password. See [Accounts and sign-in](accounts.md). Pushes over HTTPS are limited by free space: a push that would leave the data disk below `storage.min_free_gib` (10 GiB by default) is refused.
- **SSH.** Add a public key at Settings, SSH keys. The address is `git@host:owner/repo.git`, or `ssh://git@host:port/owner/repo.git` when the port is not 22.
- A CI job reads its own repository with a short-lived job token and nothing else.

The repository page also has stars and watch: Watch has the levels "Participating and mentions", "All activity" and "Ignore". Archives of any ref download from `/{owner}/{repo}/archive/<ref>.zip` or `.tar.gz`.

## Search

The search box finds repositories, issues and pull requests, people and code. Code search looks at the default branch of the 30 most recently pushed repositories you can read, `repo:owner/name` narrows it to one, and each repository has a Search code page. Code search needs an account.

## The command line and the API

`haven` is a command line client for the REST API (`/api/v3`, shaped like GitHub's): `haven auth login`, `repo create|clone|view|fork|list`, `issue`, `pr create|list|view|checkout|diff|review|merge`, `run list|view|watch|logs|rerun|cancel`, `workflow list|run`, `release create|list|upload` and `haven api` for anything else. No prebuilt binary is shipped. Build it from `crates/haven-cli` with `cargo build --release -p haven-cli`; the binary is named `haven`. The token goes in the OS keyring, or `~/.config/haven/tokens.toml`, and is sent only to the host it was issued for.
