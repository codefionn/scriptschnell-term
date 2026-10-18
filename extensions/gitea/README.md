# gitea

Tells the rail's Agents tab whether the branch of a finished agent has a pull request on
Gitea, and whether it is open, merged or closed. Merged ones move into the collapsed
Merged group.

## Signing in

Public repositories work without signing in. For private ones, open the Accounts page
of the settings (Ctrl+, then Accounts):

1. Create token opens the host's Applications page in your browser. Generate a token
   with read access to repositories.
2. Paste the token into the field and press Sign in. scriptschnell-term checks it
   against `/api/v1/user`, keeps it in the system's secret storage and shows your user
   name.

Without a sign-in, a token in `GITEA_TOKEN` is used for gitea.com accounts. The token
goes only to the host it belongs to; the extension itself never sees it.

## Several accounts

Each account on the Accounts page gets its own sign-in. Type an entry into the field under the forge's accounts and press Add account; Remove on an
account's row drops it and signs out of it. An entry is a host, or a host and an owner
such as `gitea.com/acme`, which holds only the repositories under it. A repository takes
the entry on its host with the longest path its own path starts with. With `gitea.com` and
`gitea.com/acme` listed, `acme/app` uses the `acme` token and `octo/app` the
`gitea.com` one. A repository that matches no entry is not checked, so keep the bare host
listed for everything else. `GITEA_TOKEN` stands in for every entry on gitea.com without a sign-in.

## Requests

Gitea has no GraphQL API. scriptschnell-term asks only about worktrees whose agent
finished and whose branch is not known to be merged, at most every three minutes, and the
extension sends one request per repository: the 50 most recently updated pull requests,
matched by head branch. A branch whose pull request is older than those 50 shows no pull
request. A 429 answer pauses the checks for as long as `Retry-After` says, or 15
minutes.

## Discussions

The button beside an agent's row in the Agents tab opens the pull request's status and
comments on the Change request page of the explore panel. Four requests read the pull
request, its comments, reviews and timeline, whose push events say which pushes were
forced, then one per review with code comments and one for the commit statuses of the
head.

scriptschnell-term places every code comment on the worktree's current head with git, so
comments written before a force push say whether their line is unchanged, moved, changed
or gone. The page is read-only; reply on Gitea.

## Sandboxes

Terminals in a sandbox reach the forge through scriptschnell-term's proxy, which holds
back requests that open, comment on or merge pull requests until you answer:

| Guard | Holds back |
| --- | --- |
| `gitea.create` | `POST /api/v1/repos/{owner}/{repo}/pulls` (`tea pr create`) |
| `gitea.comment` | comments on a pull request or issue, and reviews |
| `gitea.merge` | `POST /api/v1/repos/{owner}/{repo}/pulls/{index}/merge` (`tea pr merge`) |

Each guard asks by default. Settings, Extensions, scriptschnell-term sets Ask, Block or
Allow per kind for every forge (`sandbox.change_requests` in `settings.json`), and this
extension's page sets a mode for one of its guards, which wins. The dialog offers Block,
Allow (for one minute), Allow in this session and Always allow in this project. The
guards cover the hosts of the accounts and their subdomains. They need the
`guards` permission; without it the extension keeps working without them.

## Accounts

The extension has no settings of its own. Its accounts are gitea.com, self-hosted Gitea
hosts, and owners on them such as `gitea.com/acme`, each with its own sign-in. The
Accounts page edits them and saves them under `forge_hosts` (`gitea.gitea`) in
`settings.json`. Without an entry the extension uses gitea.com. A host listed by both the
forgejo and the gitea extension belongs to the one that loads first.
