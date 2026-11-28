# Accounts and sign-in

How people get an account, sign in, protect it with two-factor, and what to do when something is lost. For installing and making the first admin, see [Getting started](getting-started.md).

## How accounts are made

There is no sign-up page, no sign-up route and no sign-in through a proxy header. An identity header from a proxy in front signs nobody in.

- The first account is made on the server, by someone with root on the host: `sudo bash deploy/prod/first-admin.sh <username> <email>`. It prints a link that works once, for one hour. The script refuses to run when any account exists.
- Every other account comes from an invite.

### Invites

A site admin makes an invite at Site admin, Invites, or on the server:

```sh
deploy/compose.sh exec server codehaven admin invite create --email bob@example.com
```

An invite can name an email address (the account must then use it), make the new account a site admin, allow its workflows to run, and put it in an organization and a team. It works once and lasts 7 days by default, from 1 hour to 30 days. `codehaven admin invite list` shows them and `codehaven admin invite revoke <id>` ends one.

The invitee opens the link, picks a username, an optional display name, an email address and a password, and has an account. A used, revoked, expired or made-up link all show the same "This invite link doesn't work any more" page, so a link cannot be probed for. A mistake in the form, such as a name that is taken, does not use the invite up.

A username is up to 39 letters, numbers and single hyphens, starting and ending with a letter or number. It becomes part of the address of everything the person owns, and it cannot be changed later.

A password is 12 to 128 characters, must not be easy to guess (common passwords, names, keyboard runs, dates and repeats are refused), and must not contain the username or the email name. A few random words works well.

## Signing in

Enter the username (or email) and the password. Every refusal says the same thing, "That username and password don't match.", whether the name exists, the password is wrong or the account is suspended.

If the account has two-factor sign-in on, a second step follows. Give one of:

- the 6 digit code from your authenticator app,
- a recovery code, which looks like `abcde-fghjk` and works once,
- or use a passkey, if you added one.

A half-signed-in session lasts 10 minutes and ends after 5 wrong codes. A finished sign-in lasts 7 days without use, and 30 days in all. Settings, Sessions lists every browser the account is signed in on and signs any of them out.

### Forgot your password

Choose "Forgot your password?" on the sign-in page. The answer is the same whether or not an account matched.

- With mail set up (see [Configuration](configuration.md)), the account's primary email gets a link that works once for an hour. At most one is sent per account every ten minutes. Asking for one signs nobody out; choosing the new password signs the account out everywhere.
- Without mail, the page says to ask a site admin. A site admin uses Site admin, Users, Reset password, or on the server `codehaven admin reset-password <username>`. Either prints a link, signs the person out everywhere and revokes their access tokens.

A reset does not turn two-factor off. After choosing the new password the person signs in, through the second step when it is on.

### Lockouts

Ten wrong passwords for one account from one address in 15 minutes lock that address out of that account for 15 minutes, doubling up to 24 hours. A stranger who knows a username cannot lock the owner out from the owner's own address. Forty failures from one address lock that address. Wrong two-factor codes count across all addresses, because only someone who knew the password got that far. The page says "Too many attempts. Try again in N minutes." and nothing about the account.

A site admin clears a lockout at Site admin, Users, Unlock, or with `codehaven admin unlock <username>`.

## Two-factor sign-in

Turn it on at Settings, Two-factor and security. It is optional for everyone except site admins: the admin pages stay closed until the admin has it on, and the Claude connector needs it too.

### Authenticator app

Choose "Set up an authenticator app". The page shows a QR code and the key. Scan or type it into any authenticator app, enter the code it shows, and Codehaven gives you ten recovery codes. They are shown once. Store them somewhere safe, away from the phone that holds the app.

A code can be used once. "Make new recovery codes" replaces the set and needs your password. Turning the app off needs your password and a current code or a recovery code.

### Passkeys

A passkey is a second step you approve with a fingerprint, face, screen lock or security key, instead of an app code. Add one under Passkeys on the same page. You can hold up to 10. You still type your password first.

Passkeys need the site to be opened by a domain name over https: an IP address cannot be a passkey's site, and the page says "Passkeys need this site to be opened by a domain name over https. Ask the administrator to set its address." when `CODEHAVEN_EXTERNAL_URL` is not one.

Removing the last passkey when it is your only second step turns two-factor off, so it asks for your password and a recovery code.

### Recent security events

The same page lists the last 30 things that happened to the account: sign-ins, password changes, new keys and tokens, with the address and the outcome. If an account was locked since you last signed in, a notice says so once, with the addresses.

## Recovery

| What is lost | What to do |
| --- | --- |
| The password | "Forgot your password?" if mail is set up, or ask a site admin for a reset link (above). If you are the only admin, run on the server `deploy/compose.sh exec server codehaven admin reset-password <username>` and open the link it prints |
| The authenticator app, but you have recovery codes | Sign in with a recovery code, then set up the app again and make new codes |
| The app and every recovery code, and no passkey | There is no admin page or command that turns two-factor off for another account, by design. Keep recovery codes safe, and add a passkey as a second way in |
| The first admin's password | `deploy/compose.sh exec server codehaven admin reset-password <username>` prints a new one-hour link, and it works for the only admin. It does not turn two-factor off |
| The first admin's second step | Its recovery codes are the way back in. Nothing resets accounts or empties an instance, and nothing needs to |
| An account that must stop now | A site admin suspends it (Site admin, Users, or `codehaven admin suspend <username>`). That ends its sessions, disables its tokens and stops its SSH keys. `unsuspend` lifts it |

## Access tokens

Git over HTTPS uses a personal access token as the password. Make one at Settings, Access tokens.

- Give it a name, choose its scopes and how long it lasts: 30, 90, 180 days or one year. The default is 90 days and the longest is 365.
- The token is shown once. Codehaven keeps only a hash of it.
- An account can hold 100 active tokens.
- A token never carries site admin powers, whatever its scopes.

| Scope | What it allows |
| --- | --- |
| Repositories (`repo`) | Read and write your repositories |
| Repositories, read only (`repo:read`) | Read your repositories, private ones included |
| Profile (`user`) | Read and update your profile |
| Organizations, read only (`read:org`) | See the organizations and teams you belong to |
| Organizations (`admin:org`) | Manage them |
| Packages (`write:packages`) | Upload and delete packages |
| Packages, read only (`read:packages`) | Download packages |
| Workflows (`workflow`) | Update workflow files and trigger runs. A push that changes `.codehaven/workflows` needs it |
| Repository webhooks (`admin:repo_hook`) | Manage webhooks on your repositories |
| Notifications (`notifications`) | Read your notifications |

A password works for git over HTTPS only on an account without two-factor. On one that has it, git answers "use a personal access token as the password".

## SSH keys

Add a public key at Settings, SSH keys: a title that says which device it lives on, and the one-line public key, usually `~/.ssh/id_ed25519.pub`. Never paste a private key. Ed25519, ECDSA, FIDO security keys and RSA keys of 3072 bits or more are accepted. `ssh-keygen -t ed25519` makes a good one.

The SSH server accepts only `git-upload-pack`, `git-receive-pack` and `git-upload-archive`. A shell, a terminal or any other command gets a short message and the session ends. A removed key stops working at once, and so does every key of a suspended account.

## Other settings

- **Password.** Settings, Password changes it. A change ends your other sessions and gives this one a new cookie.
- **Profile.** The username cannot be changed. The display name is shown next to your work. The email is optional.
- **Notifications.** With mail set up on the server, you can choose to be emailed when something new needs your attention: mentions, assignments, review requests and repositories you watch. The email holds the title and a link, never the text of a comment. Without mail the inbox is all there is.
- **Connected apps.** Lists every app, such as Claude, that you connected, with what it may reach, and lets you revoke it. See [The Claude connector](connector.md).

## Organizations and teams

Repositories can belong to an organization instead of a person. Make one at New organization (`/new/org`). The name follows the same rules as a username.

- People need an account first. An organization owner adds existing accounts as Member or Owner. Only a site admin can invite someone new, and can add them to the organization in the same step.
- An organization has a default repository permission for its members (none, read, write or admin) and teams that give more or less on chosen repositories. A team has a permission and a list of members and repositories.
- An organization starts with the permission to run CI that its creator had at that moment.
- Deleting an organization needs its repositories to be deleted or transferred first.

The permissions on a repository are listed in [Repositories](repositories.md).
