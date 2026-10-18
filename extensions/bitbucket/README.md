# bitbucket

Tells the rail's Agents tab whether the branch of a finished agent has a pull request on
Bitbucket Cloud, and whether it is open, merged or declined. Merged ones move into the
collapsed Merged group. Bitbucket Server and Data Center have a different API and are not
covered.

## Signing in

Public repositories work without signing in. For private ones, open the Accounts page
of the settings (Ctrl+, then Accounts):

1. Create token opens Atlassian's API token page in your browser. Create an API token
   with scopes for Bitbucket: `read:pullrequest:bitbucket`, `read:repository:bitbucket`
   and `read:user:bitbucket`.
2. Paste your Atlassian email and the token as `email:token` into the field and press
   Sign in. Bitbucket's API tokens log in with Basic auth as that pair. scriptschnell-term
   checks it against `/2.0/user`, keeps it in the system's secret storage and shows your
   name.

To use another login for one workspace, type `bitbucket.org/<workspace>` into the field
under Bitbucket's accounts and press Add account. Its repositories then use that login, and every
other repository the `bitbucket.org` one. Remove on an account's row drops it and signs
out of it.

The login goes only to bitbucket.org and its subdomains, such as api.bitbucket.org; the
extension itself never sees it. App passwords no longer work on Bitbucket; use an API
token.

## Requests

Bitbucket has no GraphQL API. scriptschnell-term asks only about worktrees whose agent
finished and whose branch is not known to be merged, at most every three minutes. The
extension sends one request per repository and up to 20 branches, filtered with
`source.branch.name = "..." OR ...`, newest first. Bitbucket allows 1000 such requests an
hour with a login, and far fewer without one. A 429 answer pauses the checks for as long
as `Retry-After` says, or 15 minutes.

## Discussions

The button beside an agent's row in the Agents tab opens the pull request's status and
comments on the Change request page of the explore panel. Four requests read the pull
request with its reviewers, its comments with their replies, its commit statuses, and
its activity, whose updates name the head of every push. Bitbucket names no commit for a
code comment, so its line is taken as it is on the head.

scriptschnell-term places every code comment on the worktree's current head with git, so
comments written before a force push say whether their line is unchanged, moved, changed
or gone. The page is read-only; reply on Bitbucket.

## Sandboxes

Terminals in a sandbox reach the forge through scriptschnell-term's proxy, which holds
back requests that open, comment on or merge pull requests until you answer:

| Guard | Holds back |
| --- | --- |
| `bitbucket.create` | `POST /2.0/repositories/{workspace}/{repo}/pullrequests` |
| `bitbucket.comment` | comments, approvals and change requests on a pull request |
| `bitbucket.merge` | `POST /2.0/repositories/{workspace}/{repo}/pullrequests/{id}/merge` |

Each guard asks by default. Settings, Extensions, scriptschnell-term sets Ask, Block or
Allow per kind for every forge (`sandbox.change_requests` in `settings.json`), and this
extension's page sets a mode for one of its guards, which wins. The dialog offers Block,
Allow (for one minute), Allow in this session and Always allow in this project. The
guards cover the hosts of the accounts and their subdomains, such as api.bitbucket.org. They need the
`guards` permission; without it the extension keeps working without them.

## Accounts

The extension has no settings of its own. Its accounts are bitbucket.org and workspaces
on it such as `bitbucket.org/acme`, each with its own sign-in. A repository takes the
account with the longest path it starts with; one that matches no account is not
checked. The Accounts page edits them and saves them under `forge_hosts`
(`bitbucket.bitbucket`) in `settings.json`. Without an entry the extension uses
bitbucket.org.
