# github

Tells the rail's Agents tab whether the branch of a finished agent has a pull request on
GitHub, and whether it is open, merged or closed. Merged ones move into the collapsed
Merged group.

## Signing in

Open the Accounts page of the settings (Ctrl+, then Accounts):

1. Create token opens GitHub's page for a fine-grained token in your browser, with read
   access to pull requests and contents filled in. A classic token with the `repo`
   scope works too.
2. Paste the token into the field and press Sign in. scriptschnell-term checks it
   against `/user`, keeps it in the system's secret storage (`secret-tool` or the
   macOS keychain) and shows who you are signed in as.

Without a sign-in, a token in `GH_TOKEN` or `GITHUB_TOKEN` is used for github.com accounts.
The token goes only to the host it belongs to and its subdomains; the extension itself
never sees it.

## Several accounts

Each account on the Accounts page gets its own sign-in. Type an entry into the field under the forge's accounts and press Add account; Remove on an
account's row drops it and signs out of it. An entry is a host, or a host and an owner
such as `github.com/acme`, which holds only the repositories under it. A repository takes
the entry on its host with the longest path its own path starts with. With `github.com` and
`github.com/acme` listed, `acme/app` uses the `acme` token and `octo/app` the
`github.com` one. A repository that matches no entry is not checked, so keep the bare host
listed for everything else. `GH_TOKEN` and `GITHUB_TOKEN` stand in for every entry on github.com without a sign-in.

## Requests

scriptschnell-term asks only about worktrees whose agent finished and whose branch is not
known to be merged, at most every three minutes. The extension puts up to 50 branches
across all repositories of a host into one GraphQL request, which costs one point of the
5000 points an hour a token gets. A 403 or 429 answer pauses the checks for as long as
GitHub's `Retry-After` says, or 15 minutes.

## Discussions

The button beside an agent's row in the Agents tab opens the pull request's status and
comments on the Change request page of the explore panel. One GraphQL request reads the
pull request, its draft and conflict state, its comments, reviews, review threads with
the commit and diff hunk each was written on, its force pushes, each reviewer's latest
verdict and the check runs and commit statuses of its last commit. It needs a sign-in,
like the checks. A fine-grained token without read access to commit statuses shows no
checks.

scriptschnell-term places every code comment on the worktree's current head with git, so
comments written before a force push say whether their line is unchanged, moved, changed
or gone. The page is read-only; reply on GitHub.

## Sandboxes

Terminals in a sandbox reach the forge through scriptschnell-term's proxy, which holds
back requests that open, comment on or merge pull requests until you answer:

| Guard | Holds back |
| --- | --- |
| `github.create` | `POST /repos/{owner}/{repo}/pulls`, GraphQL `createPullRequest` (`gh pr create`) |
| `github.comment` | issue and review comments, reviews, GraphQL `addComment`, `addPullRequestReview*` and `submitPullRequestReview` (`gh pr comment`, `gh pr review`) |
| `github.merge` | `PUT /repos/{owner}/{repo}/pulls/{number}/merge`, GraphQL `mergePullRequest`, `enablePullRequestAutoMerge` and `enqueuePullRequest` (`gh pr merge`) |

`gh` sends GraphQL, so the proxy reads the body of `POST /graphql` before it lets it
pass. Queries go through untouched.

Each guard asks by default. Settings, Extensions, scriptschnell-term sets Ask, Block or
Allow per kind for every forge (`sandbox.change_requests` in `settings.json`), and this
extension's page sets a mode for one of its guards, which wins. The dialog offers Block,
Allow (for one minute), Allow in this session and Always allow in this project. The
guards cover the hosts of the accounts and their subdomains, such as api.github.com. They need the
`guards` permission; without it the extension keeps working without them.

## Accounts

The extension has no settings of its own. Its accounts are github.com, GitHub Enterprise
Server hosts, and owners on them such as `github.com/acme`, each with its own sign-in.
The Accounts page edits them and saves them under `forge_hosts` (`github.github`) in
`settings.json`. Without an entry the extension uses github.com.
