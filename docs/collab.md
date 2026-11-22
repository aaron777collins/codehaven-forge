# Collaboration (`ch-collab`)

Issues, labels, milestones, pull requests, reviews, the merge gate, auto-merge, releases and search.
It reaches git and identity only through the traits in `ch_core::services`.

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
| git | `GitService::compare` (three dot, with commits and the diff), `test_merge`, `merge` with a compare-and-swap on `expected_base_sha`, `create_ref` and `delete_ref` with full ref names, `blob`, `tree`, `grep`, `commit_file_change`. For a fork pull request, `compare` on the fork must see the base's objects (alternates), and `create_ref` on the base repository for `refs/pull/<n>/head` needs the fork's head object, which only exists if git fetches it. Collab ignores a failure there. |
| git | `Repos::protections` readable by `Actor::System`, `Repos::get` and `get_by_name` with read checks. |
| identity | `Authz::repo_permission` (the only permission check collab makes), `Accounts::get_by_username` and `get_user`. |
| notify | the new `DomainEvent::ReviewRequested` for the "review requested" reason. `PrClosed` with `merged: true` is followed by `PrMerged` for a merge. |
| ci | `Checks::required_satisfied`. CI is built after collab, so `workers()` binds the real `Checks` at startup (`deps::bind_checks`). |
| web | `rest::releases::file_response` streams an asset for `/{owner}/{repo}/releases/download/{tag}/{name}` after `Releases::open_asset`. |

## REST

Everything is under `/api/v3` and private to signed-in callers (the gate in `codehaven` enforces that, and
`anonymous_surface.rs` walks these routes too). A repository the caller cannot read is a 404.
Lists take `page` and `per_page` and answer with a `Link` header, offering `next` whenever the page came back full.

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
GET DELETE .../releases/assets/{id}                     (Accept: application/octet-stream downloads the file)

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
- Issue forms in YAML (`.yml` templates are ignored, Markdown templates work).
- Webhook payloads for the new events (the notify stream owns them).
- Transferring or deleting an issue, and reordering pinned issues.
- Repository-wide lists of issue comments and review comments (`/issues/comments`, `/pulls/comments`).
- A merge queue.

## Testing

```sh
make STREAM=collab test P=ch-collab      # 228 tests against a real Postgres, no real git
```

`tests/common/mod.rs` has the fakes: `FakeGit` (refs, compares, merges and file changes in memory),
`TestAuthz`, `FakeRepos`, `FakeAccounts` and `FakeChecks`. `World` sets up alice (owner), bob (write), carol
(read), dave (triage) and eve (stranger) on a private repository. Real git behind these traits is exercised by
the end-to-end run once the git stream is merged.
