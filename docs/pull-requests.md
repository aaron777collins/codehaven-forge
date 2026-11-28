# Issues and pull requests

Tracking work, reviewing changes and merging them. Opening an issue, commenting, reacting and reviewing need read access to the repository and an account. Labels, assignees, milestones, locking and pinning need triage. Merging, releases and applying a suggestion need write. See [Repositories](repositories.md) for permissions.

Issues and pull requests share one number sequence in a repository, so `#12` is one or the other, never both. Type `#12` in any text to link to it and `@name` to mention a person.

## Issues

Open one from a repository's Issues tab, with New issue.

### Templates and issue forms

If the repository has templates, the new issue page lists them first. Templates live in `.codehaven/ISSUE_TEMPLATE/` or `.github/ISSUE_TEMPLATE/` (the first that has files wins), at most 20.

- **Markdown templates** (`*.md`) can start with a front matter block between `---` lines holding `name`, `about`, `title` and `labels`. The body prefills the issue.
- **Issue forms** (`*.yml` or `*.yaml`) draw a form with fields. A form has `name`, `description`, `title`, `labels` and a `body` list of fields of type `markdown`, `input`, `textarea`, `dropdown` and `checkboxes`. A field has `attributes` (`label`, `description`, `placeholder`, `value`, `options`, `multiple`, `render`) and may be required under `validations`.

```yaml
name: Bug report
description: Something is not working
labels: [bug]
body:
  - type: input
    id: version
    attributes:
      label: Version
    validations:
      required: true
  - type: textarea
    id: what
    attributes:
      label: What happened
      render: shell
```

The server draws the form as plain HTML and builds the issue body itself from the values: a `### Label` section per field, in the form's order, `_No response_` for an empty optional answer, and an answer with `render` in a fenced block. A form's labels are applied only when the person can triage. `assignees` in a form is ignored, and `config.yml` is not read. A form that cannot be read (bad YAML, a duplicate id, no field that is not markdown, more than 40 fields, a file over 64 KB) is left out of the list.

### Working with issues

- **Labels, assignees, milestone** are set in the side panel by people with triage. Manage labels at `/{owner}/{repo}/labels` (a name, a description and a colour) and milestones at `/{owner}/{repo}/milestones` (a title, a description and a state; the list shows percent complete).
- **Search and filter.** The list filters by labels, milestone, who opened it and who is assigned, and sorts by newest, oldest, recently updated or most commented. The search box understands:

| Qualifier | Meaning |
| --- | --- |
| `is:open`, `is:closed` | State |
| `is:issue`, `is:pr`, `is:merged`, `is:unmerged`, `is:draft` | Kind and pull request state |
| `is:locked`, `is:unlocked` | Whether the conversation is locked |
| `label:bug`, `-label:bug` | Has, or does not have, a label |
| `author:@me`, `assignee:alice`, `mentions:alice`, `involves:alice`, `review-requested:alice` | People. `@me` is you |
| `milestone:v1` | In a milestone |
| `no:label`, `no:assignee`, `no:milestone` | Nothing set |
| `repo:owner/name` | Which repository |
| `sort:updated-desc` | `created`, `updated` or `comments`, then `asc` or `desc` |

- **Pin** up to three issues to the top of the open list (triage). Reorder pinned issues from the list.
- **Lock** a conversation so only people with triage can add to it.
- **Reactions and subscription.** Everyone who can read can react. Subscribe or unsubscribe to control notifications for one issue.
- **Transfer** an issue to another repository you can write to, if it is the same tier or narrower, never wider. It keeps its author, text, state, comments and reactions, and gets the next number there. Labels, milestone and assignees stay behind.
- **Delete** an issue: repository admins only, and audited. Pull requests cannot be deleted.

## Pull requests

### Open one

From a branch in the repository, or from a fork (see [Repositories](repositories.md)). Choose Pull requests, New pull request, pick the base and compare branches, write a title and description, and choose Create pull request, or tick "Create as draft". A draft cannot be merged until it is marked ready for review, which the REST API and `haven` do.

If the base repository is public and your fork is not, opening the request copies your commits into the base, where everyone can read them. The page says so and asks you to tick a box first.

### Review

Open the Files changed tab. Leave comments on lines of the diff, and reply in the thread. Resolve a thread when it is dealt with. A comment that holds a code block fenced as `suggestion` shows as "Suggested change", and someone with write access sees "Apply suggestion", which commits it to the branch.

At the bottom, submit a review: Comment, Approve or Request changes. Reviewers can be requested in the side panel, people or teams of the owning organization.

- Approvals count only from people with write access. A review from anyone else shows "does not count".
- The author's own approval never counts.
- A request for changes stays until the same reviewer reviews again or a maintainer dismisses it.
- Nothing bypasses the rules, a site admin included.

### Checks and fork approval

CI results show on the pull request's Checks tab. A pull request from a fork runs nothing until someone with write access chooses "Approve and run" for that exact commit, and each new commit needs its own approval. A run started by a connected app waits the same way (see [The Claude connector](connector.md)).

### Merge

The merge box says whether the pull request can be merged and, if not, what blocks it: it is a draft, there are conflicts, checks are pending or failing, approvals are missing, a code owner has not approved, threads are unresolved, or the branch is behind. The rules come from the base branch's protection (see [Repositories](repositories.md)).

Someone with write access chooses a method, and may edit the commit title and message first:

- **Create a merge commit.**
- **Squash and merge.** One commit that lists the commits.
- **Rebase and merge.**

The form carries the head commit the page showed, so a push after the page loaded stops the merge instead of merging something nobody reviewed. A branch of the same repository can be deleted with the merge, or afterwards with "Delete branch".

Through the REST API and `haven pr merge --auto`, a pull request can be set to merge by itself once its rules are met. It merges in the name of whoever turned it on and stops if they lose write access. "Update branch" brings the base into the pull request's branch, also through the API.

### The merge queue

A repository admin can turn the queue on at Settings, General. While it is on, a pull request merges only by being added to the queue: the Merge button, the REST merge and auto-merge all refuse.

The queue works on one pull request per base branch at a time, oldest first. If the branch does not contain the base's current tip, the base is merged into it first, so that checks run on the merged result. Once it is up to date, the ordinary merge gate decides and a clear gate merges it, as the person who queued it, by the method they chose. A pull request leaves the queue, with the reason on its timeline, when a required check failed, it conflicts, someone pushed commits the queue did not make, the person who queued it lost write access, it closed, it waited more than 24 hours, or a person removed it. At most 50 wait per base branch. Turning the queue off empties it. Pull requests from forks still wait for a writer's approval of that commit before checks run.

A pull request's CI run tests the head commit, not a merge into the base. A repository that wants its pull requests tested merged turns on the queue.

## Notifications

The inbox is at `/notifications`, with Unread and All, and mark as read. You are notified when you are mentioned, assigned, asked to review, when something you opened or commented on changes, when an issue or pull request you subscribed to changes, and for the repositories you watch. Only failed CI runs notify the person who started them. Everyone notified is checked for read access first, so a title never reaches someone who could not open the page.

Watch a repository from its header: "Participating and mentions" (the default), "All activity" or "Ignore".

With mail set up on the server, Settings, Notifications offers email for new notifications. It is off until you choose it. An email holds the title and a link, never the text of a comment, and a failed send is tried again up to five times.
