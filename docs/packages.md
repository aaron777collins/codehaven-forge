# Container registry and packages

Codehaven keeps container images in a built-in registry and NuGet packages in a feed per account. Both are on the same address as the rest of the site, with no extra service to run. Both use personal access tokens from Settings, Access tokens (see [Accounts and sign-in](accounts.md)).

A token needs the Packages scope (`write:packages`) to push and Packages, read only (`read:packages`) to pull or restore.

## Container images

The registry speaks the OCI Distribution Spec 1.1 at `/v2/`, on the same host as the site, so Docker clients need no insecure-registry setting once the proxy terminates TLS. An image is named `<owner>/<name>`, where the owner is an account or an organization.

```sh
echo $TOKEN | docker login code.example.com -u alice --password-stdin
docker tag myapp code.example.com/alice/myapp:1.0
docker push code.example.com/alice/myapp:1.0
docker pull code.example.com/alice/myapp:1.0
```

Use the host name only, with no `https://`. The registry supports chunked and monolithic uploads, cross-repository mounts, tag listing, manifest and tag deletes, referrers, range requests on layers, the catalog, and garbage collection. Layers are stored once and shared across images. Docker schema 1 is not supported. The conformance suite and a real Docker client are run against it by `deploy/tests/registry-conformance.sh`.

### Who can pull and push

- An image with no repository belongs to its owner. For an organization, members pull and owners push.
- An image **linked to a repository** follows that repository: read access pulls, write access pushes. The first push links the image when a repository of the same name exists and the pusher can write to it.
- Signed out, only an image linked to a **public** repository can be pulled, and only while public repositories are allowed. Every other pull and every push needs a token. Anything the caller may not see looks exactly like an image that does not exist.

### Package pages

`/packages` lists every image and package you could pull. `/packages/<owner>` lists one account's, and a repository's Packages tab lists the ones linked to it. A package page shows the `docker pull` or `dotnet` commands, and the tags or versions with their digest, platforms, size and push time. These pages need you to be signed in.

The owner of a package can link it to one of their repositories that they can write to (the page first shows who would gain access, and a public repository lets anyone pull it), unlink it, delete one tag or version, or delete the whole package by typing its name. Deleting a tag keeps its manifest while another tag or an index still points at it, so a multi-architecture image keeps working. Each change is audited.

A workflow job pushes with its `GITHUB_TOKEN` when its `permissions:` allow `packages: write`, to a package of its own repository. It never creates one for a public repository, because the first push would publish it. Docker itself is not available inside a job (see [Using CI](using-ci.md)), so build images where Docker runs and push from there.

## NuGet packages

Every account has a NuGet v3 feed at `/nuget/<owner>/index.json`, for push, restore, search and unlist. It is for signed-in people and tokens only. A signed-out request gets a Basic challenge, the same whether the feed exists or not, and there is no public feed.

```sh
dotnet nuget add source https://code.example.com/nuget/alice/index.json -n codehaven -u alice -p TOKEN --store-password-in-clear-text
dotnet nuget push Pkg.1.0.0.nupkg -s codehaven -k TOKEN
dotnet add package Pkg
dotnet nuget delete Pkg 1.0.0 -s codehaven -k TOKEN      # unlists, never removes
```

The package page in the browser shows the add-source command with `/v3/index.json` at the end. The address the server answers is `/nuget/<owner>/index.json`, so use that one.

- A package is `owner/id`, in the same table as images, and follows the same rules: no repository means it belongs to its owner, a linked package follows its repository.
- A version is written once. `1.0` and `1.0.0` are the same version, and pushing it again is refused with a 409.
- The server reads the zip's directory and its one top-level `.nuspec` (512 KiB at most, no DOCTYPE or entity) and unpacks nothing. A package is at most 250 MiB, and a package holds at most 5000 versions.
- Unlisting hides a version from search. Asking for it by version still restores it.
- A job pushes with `GITHUB_TOKEN` as the key when its `packages: write` allows it. A job with `packages: read` restores the packages of its own repository.
- With a plain `http` source, the NuGet client needs `allowInsecureConnections` in `NuGet.Config`, and the token in the source's credentials as well as in `--api-key`.

## Limits that apply to uploads

Uploads, pushes and release files are refused when they would leave the data disk with less than `storage.min_free_gib` free, 10 GiB by default. See [Configuration](configuration.md).
