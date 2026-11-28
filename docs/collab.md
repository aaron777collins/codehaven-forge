# Collaboration (`ch-collab`)

Issues, labels, milestones, pull requests, reviews, the merge gate, auto-merge, releases and search.
It reaches git and identity only through the traits in `ch_core::services`.

> Contributor notes. This page records how the part was built, for people working on Codehaven. To install or use it, start at [Getting started](getting-started.md).

## Shape

```
Core (core.rs)              shared lookups, hydration of rows into views, timeline, cross references
  issues.rs, comments.rs    issues, comments, reactions, timeline, subscriptions, templates
  labels.rs                 labels, milestones, the starting labels
  filter.rs, query.rs       the search language ("is:open label:bug author:@me sort:updated-desc")
  pulls.rs                  open, update, close, reopen, draft, files, commits, mergeability
  merge.rs, gate.rs         the gate (pure), merging, update branch, auto-merge
  protect.rs                branch protection patterns and how several protections combine
  reviews.rs                reviews, pending reviews, inline comments, threads, requests, CODEOWNERS
  codeowners.rs, suggest.rs CODEOWNERS parsing and matching, suggestion blocks (both pure)
  releases.rs               releases, assets, generated notes
  search.rs                 repositories, issues and pull requests, users, code
  sync.rs                   the `pr-sync` and `collab-defaults` consumers
  services.rs               the five `ch_core::services` traits over Core
  rest/                     `/api/v3` endpoints and the DTO mapping
```

`Core` holds a `Deps` (database, audit, events, blobs, and the `Authz`, `GitService`, `Repos` and `Checks`
services). Every public method takes the `Actor`, checks permission first, changes rows in one transaction
together with the audit record and the domain event, and returns a view with names, labels and counts filled
in (`ch_core::collab`).

The traits in `ch_core::services` were extended (all new methods default to "not built", so a fake written
before this stream keeps compiling). The web stream should call the `*view*` methods, which take one round trip
each, rather than the older ones that return bare ids.

## Rules worth knowing

- Opening an issue or pull request, commenting, reacting and reviewing need read access. Labels, assignees,
  milestones, locking and pinning need triage. Merging, releases and applying a suggestion need write.
  A token without the `repo` scope is read-only even for the things a reader may do with a password.
- Nothing bypasses the gate, not even a site admin. Approvals count only from people with write access, the
  author's own approval never counts, and a change request stays until the reviewer reviews again or a
  maintainer dismisses it.
- If CI cannot be asked, required checks count as not passed. If protections cannot be read, the merge fails.
- Auto-merge merges in the name of whoever turned it on, and stops if they lose write access.
- A private repository's issue titles never appear in another repository's timeline for someone who cannot
  read the first one.
- Search never copies the permission model into SQL. It over-fetches candidates and keeps what `Authz` allows.
- Issue numbers and pull request numbers share one sequence per repository. Milestones have their own.

## What it expects from other streams

| From | What |
| --- | --- |
| git | `GitService::compare` (three dot, with commits and the diff), `test_merge`, `merge` with a compare-and-swap on `expected_base_sha`, `create_ref` and `delete_ref` with full ref names, `blob`, `tree`, `grep`, `commit_file_change`. For a fork pull request, `compare` on the fork must see the base's objects (a fork copies its source's objects, so it holds them), and `create_ref` on the base repository for `refs/pull/<n>/head` needs the fork's head object, which only exists if git fetches it. Collab ignores a failure there. |
| git | `Repos::protections` readable by `Actor::System`, `Repos::get` and `get_by_name` with read checks. |
| identity | `Authz::repo_permission` (the only permission check collab makes), `Accounts::get_by_username` and `get_user`. |
| notify | the new `DomainEvent::ReviewRequested` for the "review requested" reason. `PrClosed` with `merged: true` is followed by `PrMerged` for a merge. |
| ci | `Checks::required_satisfied`. CI is built after collab, so `workers()` binds the real `Checks` at startup (`deps::bind_checks`). |
| web | `rest::releases::file_response` streams an asset for `/{owner}/{repo}/releases/download/{tag}/{name}` after `Releases::open_asset`. |

## REST

Everything is under `/api/v3` and private to signed-in callers (the gate in `codehaven` enforces that, and
`anonymous_surface.rs` walks these routes too). A repository the caller cannot read is a 404.
Lists take `page` and `per_page` and answer with a `Link` header, offering `next` whenever the page came back full.
The release and release-asset lists are paged from memory, so their `Link` says exactly whether there is a next page and which is the last.

```
GET  /issues, /user/issues                              ?filter=assigned|created|mentioned|review_requested
GET POST   /repos/{owner}/{repo}/issues                 ?state labels assignee creator mentioned milestone sort direction q
GET PATCH  /repos/{owner}/{repo}/issues/{n}
PUT DELETE .../issues/{n}/lock, .../issues/{n}/pin
GET POST   .../issues/{n}/comments    GET PATCH DELETE .../issues/comments/{id}
GET POST PUT DELETE .../issues/{n}/labels      DELETE .../issues/{n}/labels/{name}
POST DELETE .../issues/{n}/assignees
GET .../issues/{n}/events, .../issues/{n}/timeline
GET POST DELETE .../issues/{n}/reactions, .../issues/comments/{id}/reactions, .../pulls/comments/{id}/reactions
GET PUT DELETE .../issues/{n}/subscription
GET POST .../labels     GET PATCH DELETE .../labels/{name}
GET POST .../milestones GET PATCH DELETE .../milestones/{n}
GET .../assignees       GET .../assignees/{user}        GET .../issue-templates

GET POST   .../pulls                                    ?state head base sort direction q
GET PATCH  .../pulls/{n}
GET .../pulls/{n}/files, .../pulls/{n}/commits
GET PUT    .../pulls/{n}/merge                          PUT .../pulls/{n}/update-branch
GET .../pulls/{n}/merge-gate                            (extension: every reason a merge is blocked)
PUT DELETE .../pulls/{n}/auto-merge                     POST .../pulls/{n}/ready_for_review, .../convert_to_draft
GET POST   .../pulls/{n}/reviews    GET DELETE .../reviews/{id}    POST .../reviews/{id}/events
PUT .../reviews/{id}/dismissals     GET .../reviews/{id}/comments
GET POST   .../pulls/{n}/comments   GET PATCH DELETE .../pulls/comments/{id}
POST .../pulls/comments/{id}/apply-suggestion           (extension)
GET .../pulls/{n}/threads           PUT DELETE .../pulls/{n}/threads/{id}/resolve   (extension)
GET POST DELETE .../pulls/{n}/requested_reviewers

GET POST .../releases   GET .../releases/latest   GET .../releases/tags/{tag...}   POST .../releases/generate-notes
GET PATCH DELETE .../releases/{id}                      GET POST .../releases/{id}/assets   (raw body, ?name= &label=)
GET PATCH DELETE .../releases/assets/{id}               (Accept: application/octet-stream downloads the file; PATCH renames and labels)

GET /search/repositories, /search/issues, /search/users, /search/code   (code needs `repo:owner/name`)
```

## Consumers

- `pr-sync`: on `Push` it moves open pull requests with that head branch (head sha, counts, mergeability,
  the pull ref, review comment positions: lines that moved are renumbered, lines that were rewritten become
  outdated and keep their original hunk, and CODEOWNERS are asked again), recognises a branch that landed on
  its base by a plain push as merged, and applies `Fixes #n` from pushed commits on the default branch. On
  `CheckRunUpdated`, `StatusCreated` and `ReviewSubmitted` it tries auto-merge. A handler that keeps failing is
  retried five times and then skipped, so one bad event cannot hold up the rest.
- `collab-defaults`: `RepoCreated` gives the repository nine starting labels.

## Differences from the plan

- Queries use `sqlx::query_as` with typed rows, not `sqlx::query!`. About two hundred of them, all covered by the
  integration tests against Postgres, and none touches `.sqlx/`.
- There is no `search-index` consumer. The search columns are generated by Postgres.
- One migration, `0110_collab_pull_stats.sql`, adds the counts the pull request list shows and who turned on
  auto-merge. The high number keeps it clear of the other streams' migrations.
- Audit actions added: `pull.merged`, `pull.auto_merge_changed`, `review.dismissed`, `comment.deleted`,
  `issue.lock_changed`, `label.deleted`, `release.published`, `release.deleted`.

## Not done

- Cross-repository code search (the plan keeps it for `tantivy` later). Code search is per repository.
- Webhook payloads for the new events (the notify stream owns them).
- Nothing from the earlier list of transferring or deleting an issue, or reordering pinned issues: all three are built (see
  "Deleting, moving and ordering issues" below).
- Repository-wide lists of issue comments and review comments (`/issues/comments`, `/pulls/comments`).

## Issue forms and the merge queue (ch-dxk)

**Issue forms.** `.github/ISSUE_TEMPLATE/*.yml` (or `.codehaven/ISSUE_TEMPLATE`) is read as a YAML issue
form beside the Markdown templates: `name`, `description`, `title`, `labels` and a `body` of `markdown`,
`input`, `textarea`, `dropdown` and `checkboxes` fields with `attributes` (`label`, `description`,
`placeholder`, `value`, `options`, `multiple`, `render`) and `validations.required`. The new issue page
lists the templates and draws a form as plain HTML on the server, so the content security policy is
unchanged. What is submitted is values only: the server reads the form again from the repository, checks
each value against its field (an option that is not listed, a required answer that is empty, a second
line in a one-line input) and builds the issue body from them: a `### Label` section per field in the
form's order, `_No response_` for an empty optional answer, and an answer with `render:` in a fenced
block that its own text cannot close. A form that cannot be read (bad YAML, a duplicate id, no field
that is not markdown, more than 40 fields, a file over 64 KB) is left out of the list. A form's
labels are applied only when the person can triage, as on the plain new issue page. `assignees` in a
form is ignored. `config.yml` is not read.

**Merge queue.** A repository admin turns it on in Settings, General. While it is on, a pull request
merges only by being added to the queue: the Merge button, the REST merge and auto-merge all refuse. The queue
works on one pull request per base branch at a time, oldest first. If the branch does not contain the base's
current tip (the base moved after the branch was last checked), the base is merged into the pull request's branch (the "update branch" a person can do, written as the person who
queued it), so that checks run on the merged result; those checks start as they do for any push, and for
a pull request from a fork they still wait for a writer's approval of that exact commit, which the queue never
gives. Once it is up to date, the ordinary merge gate decides (required checks, reviews, resolved threads,
code owners) and a clear gate merges it as the person who queued it, by the method they chose. A
pull request leaves the queue, with the reason on its timeline (`merge_queue_removed`), when a required
check failed, it conflicts, someone pushed commits the queue did not make, the person who queued it lost
write access, it closed, it waited more than 24 hours, or a person removed it. Turning the queue off
empties it. At most 50 wait per base branch. The queue advances on events and once a minute.

## Testing

```sh
make STREAM=collab test P=ch-collab      # 228 tests against a real Postgres, no real git
```

`tests/common/mod.rs` has the fakes: `FakeGit` (refs, compares, merges and file changes in memory),
`TestAuthz`, `FakeRepos`, `FakeAccounts` and `FakeChecks`. `World` sets up alice (owner), bob (write), carol
(read), dave (triage) and eve (stranger) on a private repository. Real git behind these traits is exercised by
the end-to-end run once the git stream is merged.

## Copy consent for a pull request from a less visible fork (ch-4pn.14)

Opening a pull request from a fork copies the head's commits into the base, where everyone who
can read the base sees them. When the base is public and the head repository is not, the person
opening it must say so on purpose, or the request is refused with a `publish_head` field error
that says what it would publish. Web: a checkbox that appears with that error. REST: the
Codehaven field `publish_head: true` on `POST /repos/{owner}/{repo}/pulls`. CLI:
`haven pr create --publish-head`. The tier the head was published to (the base's, as strangers
see it) is kept in `pull_requests.head_publish_tier`. A later head of that pull request is
copied into the base only while the base is no wider than that tier: a base that becomes public
after the pull request was opened keeps the head it had, and the author opens a new pull
request with the confirm to publish more.

When a base is made public, the pull requests that came from a fork that is not public and
were never confirmed are withdrawn: no pull ref, no copied commits, no test merge, and the merge
gate says the author has not agreed to publish. Their "Files changed" and "Commits" lists, in
the API and on the pages, are empty for anyone who cannot read the fork (signed out included),
and such a person cannot comment on their lines. Line comments made before the base was widened
stay in the database, but their diff hunk and file path (code from the fork) go out blank, in the
thread and comment lists, a single comment and the repository-wide list, for anyone who cannot
read the fork. The words of the comment stay. Whoever can read the fork still sees the
changes and the full comments. The git layer repeats the rule under the write ticket, so a refresh or merge that was
checked just before the base went public still copies nothing: a repository that is not public
gives no commits to one that is, unless an open pull request from it was confirmed.

## Deleting, moving and ordering issues (ch-u6j)

- **Delete** an issue: repository admins only, audited (`issue.deleted`). Removes the issue,
  its comments, reactions and timeline; the number is not used again. Pull requests cannot be
  deleted. REST: `DELETE /repos/{owner}/{repo}/codehaven/issues/{number}`. Web: Manage, Delete.
- **Transfer** an issue: write access on both repositories, and the target must be the same
  tier or narrower, never wider (a private issue never lands in a public repository). The issue
  keeps its author, text, state, comments and reactions and gets the next number of the target;
  labels, milestone, assignees and other people's subscriptions stay behind. Audited
  (`issue.transferred`). REST: `POST .../codehaven/issues/{number}/transfer` with
  `{"repository": "owner/name"}`. Web: Manage, Transfer. A repository the person cannot see is
  "no such repository".
- **Pinned order**: pinned issues carry a `pin_position` (at most three pinned). REST:
  `GET .../codehaven/issues/pinned` and `PUT .../codehaven/issues/pinned/order` with
  `{"numbers": [3, 1, 2]}` naming every pinned issue once (triage access). Web: pinned issues
  sit first on the open issue list, with Reorder pinned for people with triage access.
- The repository-wide comment lists are `GET /repos/{owner}/{repo}/issues/comments` and
  `/pulls/comments`, as GitHub's.

All of these need a signed-in person; signed out they answer like a repository that is not there.

## Releases for release actions (ch-ctk.10, ch-ctk.11)

What `softprops/action-gh-release`, `ncipollo/release-action` and the `gh` CLI's REST calls need, over
`/api/v3/repos/{owner}/{repo}/releases`:

- **A job's own token** creates, uploads, replaces and publishes when its workflow says `contents: write`
  (the release is the `codehaven-actions` bot's, and starts no workflow). A read-only job is refused.
  A job's token may also fetch the `browser_download_url` of a file of its own repository.
- **Uploads** use the release's `upload_url` (the same server) and are not cut off by the request clock: the sender
  may be silent for `http.upload_idle_secs` (60) between two chunks and no longer. A file is at most 2 GiB. A second
  file of the same name is a 422 `already_exists`; delete the old one (or rename it with `PATCH .../assets/{id}`,
  `{name, label}`) first, which is what the actions do to replace a file.
- **`digest`** on every asset (`sha256:` and 64 hex characters, in the API, the page and the `release` event);
  `HEAD` and a single `Range` on downloads (206, 416 past the end, an unusable header is ignored, only a read of
  bytes counts as a download); an `ETag` that is the digest.
- **`make_latest`** (`"true"`, `"false"`, `"legacy"`, as a string): the latest release is the newest mark among
  published releases that are not prereleases and did not ask to be left out (`false`); publishing marks a release,
  `legacy` marks only a release whose version is the highest so far. `discussion_category_name` is a 422 that names
  the field. Migration 0146 backfills the marks from the publish dates.
- **Generated notes** list the pull requests whose merge commit is between the previous tag and the target (not those
  merged "since a date"), grouped by label. A `.github/release.yml` (or `.codehaven/release.yml`) on the default branch
  names the sections (`changelog.categories`, labels, `*`) and what to leave out (`exclude.labels`,
  `exclude.authors`); a file that cannot be read means the built-in grouping, never an error.
- **The web** can edit (`/{owner}/{repo}/releases/{id}/edit`), publish a draft, upload files (a multipart form that
  streams into storage, the CSRF token checked from the query before the body is read) and delete a file; every
  file shows its SHA-256. `/{owner}/{repo}/releases/latest` and `.../releases/latest/download/{name}` redirect to the
  latest release and its file, for a README to link to; signed out they answer only for a public repository, and
  look like a page that is not there for any other.
