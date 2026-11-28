# Identity: what it provides and how to use it

Written by the identity stream (`ch-auth`, `ch-notify`). `ARCHITECTURE.md` sections 7 and 8.4 to
8.5 have the design. This file is the working guide for the web, git, collab and ci streams, and
for whoever integrates.

> Contributor notes. This page records how the part was built, for people working on Codehaven. To install or use it, start at [Getting started](getting-started.md).

## The shape

The instance is invite-only. There is no sign-up route, no setup page, no proxy-header login and
no anonymous read outside public repositories. The first admin is made on the server with
`codehaven admin create-user`, and every other account comes from an invite.

| Crate | Owns |
| --- | --- |
| `ch-auth` | accounts, invites, sessions, two-factor, lockout, tokens, SSH keys, authorization, orgs and teams, the request guard, the user, org and site admin REST endpoints |
| `ch-notify` | the notification inbox and its fan-out, webhooks and their delivery, the SSRF guard |

Both build against `ch-core` traits only. `ch-core::identity` is new in this stream.

## For the web stream: driving the sign-in pages

Pages call traits on `AppState`. Nothing in `ch-web` needs to depend on `ch-auth`.

| Page | Call |
| --- | --- |
| `POST /login` | `state.credentials.sign_in(login, password, &client)` returns `SignIn { user, grant, step, notice }` |
| `POST /login/two-factor` | `state.credentials.complete_two_factor(pending_cookie, code, &client)`. The code is a 6 digit TOTP code or a recovery code like `abcde-fghjk` |
| `POST /login/two-factor/passkey/begin`, `POST /login/two-factor/passkey` | `state.credentials.pending_factors`, `begin_passkey_sign_in` (options as JSON) and `complete_passkey_sign_in` (the authenticator's answer as JSON). Same attempt limits as a code |
| sign out | `state.credentials.sign_out(cookie, &client)` |
| `GET/POST /invite/{token}` | `state.invites.peek(token)` to render, `state.invites.accept(token, NewAccount, &client)` to finish. Then call `sign_in` with the password the person just chose |
| `GET/POST /password/{token}` | `state.credentials.peek_link(token)` (shows the username) and `redeem_link(token, new_password, &client)`. Then `sign_in` |
| Settings > Sessions | `sessions`, `revoke_session`, `sign_out_others` |
| Settings > Security | `change_password`, `sign_in_history`, and the `state.twofactor` calls: `status`, `begin_setup`, `confirm_setup`, `disable` (a current code too when no passkey is left), `regenerate_recovery_codes`, and for passkeys `passkeys_available`, `passkeys`, `begin_passkey`, `finish_passkey`, `remove_passkey` |
| Settings > Emails, profile | `state.directory` |
| Site admin > Users | `state.directory.list_users`, `set_admin`, `unsuspend`; `state.accounts.suspend`, `unlock`, `set_can_run_ci` |
| Site admin > Invites | `state.invites.create`, `list`, `revoke` |
| Site admin > Audit log | `state.audit.query` |
| Orgs and teams | `state.orgs` |
| Inbox, watch levels | `state.notifications` |
| Hooks | `state.webhooks` |

### Cookies

`SessionGrant { cookie, expires_at, max_age_secs }` is what to put in the cookie. Build the header
with `ch_core::access::session_set_cookie(&grant.cookie, grant.max_age_secs, https)` and clear it
with `session_clear_cookie(https)`. The name is `access::session_cookie_name(https)`, which is
`__Host-ch_session` on https and `ch_session` on plain http. Read a cookie with
`access::cookie_value(headers, name)`.

A sign-in with `step == TwoFactorRequired` returns a cookie that only works for
`complete_two_factor`. It expires in 10 minutes, and `from_session` treats it as anonymous, so it
opens no page. After `complete_two_factor` the cookie has a **new value**. Always replace it.

Every call that changes privilege returns a new cookie too: `change_password`,
`TwoFactor::confirm_setup` (inside `TwoFactorEnabled`) and `TwoFactor::disable`. The old cookie is
dead, so set the new one in the same response or the person is signed out.

### CSRF

The guard layer (`ch_auth::Guard`, wired in `crates/codehaven/src/app.rs`) checks every unsafe
request that has no `Authorization` header: `Origin` (or `Referer`) must equal
`server.external_url`, and a token must be present.

Handlers read the token from the request extensions:

```rust
async fn login_page(Extension(csrf): Extension<ch_core::identity::CsrfToken>) -> Html<String> { ... }
```

Put it in every form as `<input type="hidden" name="_csrf" value="...">`, and on the body as
`<body hx-headers='{"X-CSRF-Token": "..."}'>` so htmx requests carry it. The server also reads
`_csrf` from a urlencoded body, and from the query string for multipart uploads. The guard sets the
`__Host-ch_csrf` cookie on the first page a visitor loads, so the sign-in and invite forms work
before a session exists. A page rendered for a signed-in session gets a token bound to that
session.

Sign-in on a browser tunnel (`http://127.0.0.1:3200`) needs
`CODEHAVEN_EXTERNAL_URL=http://127.0.0.1:3200`, or the Origin check refuses the form.

### What to say

Every refusal of a sign-in is `Error::Unauthorized("That username and password don't match.")`,
whether the name exists, the password is wrong, the account is suspended or has no password. Show
that text. Do not try to say which. A locked account (or a locked address) is `Error::RateLimited
{ retry_after_secs }`: say "Too many attempts. Try again in N minutes." and nothing about the
account.

Invites: `peek` and `accept` return `Error::NotFound` for expired, used, revoked and made-up links.
Show "This invite link doesn't work any more. Ask the person who sent it for a new one." for all of
them. A form mistake (name taken, weak password, wrong email) is `Error::Validation` and does **not**
use the invite up.

After a sign-in, `SignIn.notice` is `Some` when someone locked the account since the last time the
owner signed in. Show it once, with `source_ips`.

An admin without two-factor has `Actor::is_admin_without_2fa()`. Send them to two-factor setup
before any admin page. `require_site_admin` errors read "Turn on two-factor sign-in before using
the admin pages."

### The password rules

`state.credentials.check_password(username, email, password)` returns the same `Validation`
errors the real calls do, so a form can check before submitting. The rules: 12 to 128 characters,
a zxcvbn score of 3 or more (this covers common passwords, names, keyboard runs, dates and repeats),
and no username or email name inside it.

## For git, collab and ci: using Authn and Authz

- `Authz::repo_permission(actor, repo)` is the permission model of section 7.2. `Anonymous` gets
  `View` on a public repository while public repositories are allowed, and `None` everywhere
  else. A site admin only counts with a session and two-factor.
  A token is capped by its scopes. A job token reads its own repo only.
- `Authz::require(actor, repo, needed)` is `NotFound` when the actor cannot read the repo and
  `Forbidden` when they can but lack `needed`. On an archived repo, `Triage`, `Write` and
  `Maintain` fail with "This repository is archived". `Admin` stays possible so a repo can be
  unarchived.
- Git over HTTP: Basic with a personal access token as the password works for everyone. Basic
  with a password works only for accounts without two-factor (they get a message telling them to
  use a token).
- `Authn::from_ssh_key(fingerprint)` returns `SshIdentity::User` or `SshIdentity::Deploy`. A
  suspended account's keys stop working at once.

### What ch-ci must store

`Authn::from_bearer` and Basic recognise CI credentials by their hash, with no prefix rule:

- a job token: `workflow_jobs.token_hash = ch_db::tokens::hash_token(secret)`. Valid while the
  job's `status <> 'completed'`. It is a job token with `write_checks: false` when the run has
  `event IN ('pull_request', 'pull_request_target')` and `workflow_runs.event_payload.fork` is
  `true`. Set that flag when the pull request comes from a fork.
- a runner credential: `runners.credential_hash = hash_token(secret)`, valid while `revoked_at IS
  NULL`.

Use `ch_db::tokens::new_token("chj_", 40)` and `new_token("chr_", 40)` so secret scanners can
match them.

### Events the notification fan-out reads

`IssueOpened`, `IssueAssigned`, `CommentCreated`, `IssueClosed`, `IssueReopened`, `PrOpened`,
`PrClosed`, `PrMerged`, `ReviewSubmitted`, `ReviewCommentCreated`, `ReleasePublished` and
`WorkflowRunUpdated` (when `completed`). It reads the thread from the tables: `issues`,
`issue_assignees`, `issue_subscriptions`, `issue_comments`, `review_requests`, `reviews`,
`review_comments`, `watches`.

Collab: `ReviewRequested` is an event now, and `fanout.rs` gives the requested reviewer the reason
`review_requested`. Delete the `review_requests` row when the review comes in, or the stale request keeps
outranking the reviewer's other reasons.

Everyone notified is checked with `Authz::repo_permission >= Read` first, so a title never reaches a
person who could not open the page. The inbox checks again when it is read.

## Webhooks

Payloads carry GitHub's field names for the fields common tools read: `repository`, `sender`,
`issue`, `pull_request` (with `head`, `base`, `merged`), `comment`, `review`, `release`, `status`,
`ref`, `before`, `after`. They are not byte for byte GitHub's: the `push` payload has an empty
`commits` list until the git stream gives `ch-notify` a way to list them, and the workflow and
check payloads are short.

Delivery: HMAC-SHA256 in `X-Hub-Signature-256` over the exact body, `X-Codehaven-Event` and
`X-GitHub-Event`, a delivery GUID, retries at 1 min, 5 min, 30 min, 2 h and 6 h (six attempts in
all), redirects never followed, 64 KiB of response kept at most. Targets are checked twice, when
the hook is saved and again on every delivery, and the connection goes to the address that was
checked. Loopback, private, link-local (including the cloud metadata address), carrier-grade NAT
and the IPv6 equivalents (also when wrapped as `::ffff:a.b.c.d`, NAT64 or 6to4) are refused unless
`webhooks.allow_private` is on.

## Admin commands

```
codehaven admin create-user --username alice --email alice@example.com --admin --can-run-ci   # prints a 1 hour set-password link
codehaven admin reset-password <user>                                 # signs them out everywhere, revokes their tokens, prints a link
codehaven admin unlock <user>                                         # clears a lockout
codehaven admin suspend <user> | unsuspend <user>
codehaven admin users                                                 # list accounts and their flags
codehaven admin invite create [--email e] [--admin] [--can-run-ci] [--org o [--team t]] [--expires 7d] [--note n]
codehaven admin invite list | revoke <id>
```

## Limits and numbers

| What | Value |
| --- | --- |
| Password hash | Argon2id, 64 MiB, 3 passes, 1 lane, 4 at a time |
| Lockout | 10 failed passwords for one account from one address (an IPv6 /64 counts as one) in 15 minutes: 15 minutes, doubling to 24 hours, for that address only, so a stranger who knows a username cannot lock the owner out from the owner's own address. Wrong two-factor codes also count across all addresses, since only someone who knows the password gets that far. 40 failures from one address lock that address the same way. An attempt is counted before the password is checked, so simultaneous guesses share the same 10. With two-factor on, a right password does not clear the count: only a right code does. A half-signed-in session ends after 5 wrong codes |
| Sign-in, two-factor, invite and link pages | 10 a minute, burst 5, per address |
| Failed Basic or token auth | 30 a minute per address |
| Everything else signed out | 120 a minute per address |
| Signed-in API | 5,000 an hour per token or account, with `X-RateLimit-*` |
| Sessions | 7 days idle, 30 days absolute. Pending two-factor: 10 minutes |
| Tokens | `chp_` + 40 base62, 90 days by default, 365 at most, 100 active per account |
| Invites | `chi_` + 32 base62, 1 hour to 30 days, 7 days by default |
| One-time links | `cho_` + 32 base62, 1 hour by default |
| Hooks | 20 per repository or org |

## Database

Migration `0009_identity.sql` adds `users.totp_last_step`, so a TOTP code cannot be used twice. The
identity crates use `sqlx::query` and `query_as` with bound parameters, not the `query!` macros,
so they add nothing to `.sqlx/`. Every query is exercised by a test. Citext columns are always
compared as `$1::citext` so lookups are case insensitive, and selected as `::text`.
