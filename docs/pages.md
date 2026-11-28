# Pages: static sites from a branch

A repository can publish a static site from a branch, from its root or its `docs` folder, like GitHub Pages. Every push to that branch publishes a new copy. A copy that fails to publish says why, and the last good copy keeps serving. An older copy can be put back. The design is in [Pages design](design/pages.md).

## Turn Pages on for the instance (operator)

Pages is off until the operator names a host for it. Until then, a repository's Settings, Pages tab shows only "Pages is not set up here. The server's administrator has not given Codehaven a pages host, so sites cannot be published on this instance."

1. **Pick the host name.** It must be neither the app's host nor a parent of it, or the server refuses to start (a parent domain would share cookies with the app). If the app is `code.example.com`, use `pages.example.com`. Do not use `example.com`.
2. **Add a DNS record** for it that points at the same machine as the app: an `A` record (and `AAAA` if the app has one) for `pages.example.com` with the same address as `code.example.com`, or a `CNAME` to it.
3. **Set it in the env file** and deploy again:

   ```sh
   CODEHAVEN_PAGES_HOST=pages.example.com
   ```

   ```sh
   sudo bash deploy/prod/up.sh
   ```

4. **Give the proxy a site for the name.** With the bundled Caddy (`CODEHAVEN_PROXY=caddy`), edit `deploy/Caddyfile.example`: at the bottom is a commented block for the pages host. Remove the leading `#` from its lines, and replace `{$CODEHAVEN_PAGES_HOST}` with the literal name, because the Caddy container is not given that variable:

   ```
   pages.example.com {
   	request_header -Cookie
   	request_header -Authorization
   	request_header -Proxy-Authorization
   	request_header -Remote-User
   	request_header -Remote-Groups
   	request_header -Remote-Email
   	request_header -Remote-Name
   	header -Set-Cookie
   	reverse_proxy server:3200
   }
   ```

   Then recreate Caddy: `deploy/compose.sh up -d caddy`. (Instead of writing the literal name, you can pass the variable on with an extra compose file: `services: { caddy: { environment: { CODEHAVEN_PAGES_HOST: pages.example.com } } }`, and point `CODEHAVEN_COMPOSE_EXTRA` at it.) Caddy gets a certificate for the new name on its own once DNS points at the machine. The block drops `Cookie`, `Authorization` and identity headers on the way in and `Set-Cookie` on the way out. The server does the same and answers every request for that host itself, before any app route. With your own proxy, make an equivalent site for the name that sends to `<alias>:3200`.

Full mode, which serves each owner's sites at `https://<owner>.pages.example.com/` with no sandbox, is a switch at Site admin, Pages. It needs a wildcard certificate for `*.pages.example.com`, which needs a DNS challenge and a Caddy build with your DNS provider's module. The default mode works on one host with no wildcard.

## Your first site

`<project>` below means the repository's name: a repository `demo` owned by `alice` is published at `https://pages.example.com/alice/demo/`.

1. In a repository you admin, make a file `index.html` at the top and push it to `main`:

   ```sh
   echo '<h1>Hello from Pages</h1>' > index.html
   git add index.html && git commit -m "Add a home page" && git push
   ```

2. Open the repository's Settings, then Pages.
3. Choose the branch `main` and the folder `/ (the whole repository)`.
4. Leave "Who can see it" as it is, or narrow it. Choose Turn on Pages (after that the button reads Save and publish).
5. Open "Open the site", or go to `https://pages.example.com/alice/demo/`. You see the heading.

Every later push to `main` publishes a new copy. For a site in the `docs` folder, choose `/docs` and put `index.html` there.

## Publish a site

You need Admin on the repository to change Pages, and Maintain to look at it. Go to Settings, Pages.

1. Choose the **branch** and the **folder**: `/` (the whole repository) or `/docs`.
2. Choose **who can see it**. This is never more people than can read the repository, and a copy made while the repository was more private stays that private.
3. Save. Push to the branch to publish a new copy, or choose "Publish again" to publish the branch's head.

The site is at `https://pages.example.com/<owner>/<project>/`, and the Settings page has an "Open the site" link. A published copy holds up to 2,000 files and 50 MB. Symbolic links and submodules are left out. In a site that is not public, image, style and script addresses must be relative.

**Copies.** The page lists published copies with their commit. "Serve this copy" puts an older one back. **Turn off Pages** stops the site at once and deletes every copy. The repository is not changed.

## Who can see a site

Who can see a site follows the repository, and the admin may narrow it, never widen it:

| Repository tier | The site is visible to |
| --- | --- |
| Public | Anyone |
| Internal | Signed-in accounts |
| Private | People with access to the repository |

For a site that is not public, the viewer is sent through a short-lived token made for one site and one person. It never carries the app session.

## How a site is kept apart from the app

Sites live on a host of their own, never the app's. Each page runs sandboxed with no cookies: no cookie goes in or out, and the page has an opaque origin, so it cannot read the app or a neighbour's site. Each project gets its own `localStorage`, kept by a small bridge page. Responses carry `X-Robots-Tag: noindex, nofollow` and `Referrer-Policy: no-referrer`. Only `GET` and `HEAD` are answered.

A request for a path with no file tries `<path>.html`, then `<path>/index.html`, then the site's own `404.html`.

## Limits

The file and size limits, and how long a reply may stall, are server settings (`pages.max_files`, `pages.max_bytes` and the others in [Configuration](configuration.md)). Pages is for static files. There is no build step: to publish generated output, have a workflow commit it to the branch, or push the built files.
