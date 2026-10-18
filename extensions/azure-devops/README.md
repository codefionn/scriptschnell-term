# azure-devops

Tells the rail's Agents tab whether the branch of a finished agent has a pull request on
Azure DevOps, and whether it is active, completed or abandoned. Completed ones count as
merged and move into the collapsed Merged group.

An account is `dev.azure.com/<organization>`, with its own token for that organization,
or `dev.azure.com`, which covers every organization without an account of its own.
Remotes on `dev.azure.com`, `ssh.dev.azure.com` and `<organization>.visualstudio.com`
take the account of their organization.

## Signing in

1. On the Accounts page of the settings (Ctrl+, then Accounts), type
   `dev.azure.com/<organization>` into the field under Azure DevOps and press Add
   account. Remove on an account's row drops it and signs out of it.
2. Create token opens that organization's personal access token page in your browser.
   Create a token with the Code (Read) scope.
3. Paste the token into the field and press Sign in. scriptschnell-term checks it by
   listing one project of the organization and keeps it in the system's secret storage.
   The `dev.azure.com` account has no organization to check against, so it keeps its
   token without a check.

Without a sign-in, a token in `AZURE_DEVOPS_EXT_PAT` or `AZURE_DEVOPS_PAT` is used for
every account. The token goes only to dev.azure.com; the extension itself never sees it.

## Requests

scriptschnell-term asks only about worktrees whose agent finished and whose branch is not
known to be merged, at most every three minutes. The extension sends one request per
project, which lists up to 200 pull requests of all its repositories, and matches
repository and source branch. A 429 answer pauses the checks for as long as
`Retry-After` says, or 5 minutes.

## Discussions

The button beside an agent's row in the Agents tab opens the pull request's status and
comments on the Change request page of the explore panel. Four requests read the pull
request with its reviewers' votes, its statuses, its threads and its iterations, one per
push, which say whether a push was a force push or a rebase. A thread's commit comes
from the iteration it was written on.

scriptschnell-term places every code comment on the worktree's current head with git, so
comments written before a force push say whether their line is unchanged, moved, changed
or gone. The page is read-only; reply on Azure DevOps.

## Sandboxes

Terminals in a sandbox reach the forge through scriptschnell-term's proxy, which holds
back requests that open, comment on or merge pull requests until you answer:

| Guard | Holds back |
| --- | --- |
| `azure-devops.create` | `POST .../_apis/git/repositories/{repo}/pullrequests` (`az repos pr create`) |
| `azure-devops.comment` | comment threads and replies, and reviewer votes |
| `azure-devops.merge` | a `PATCH` of a pull request that sets `"status": "completed"` or `autoCompleteSetBy` (`az repos pr update --status completed`, `--auto-complete`) |

Completing is a `PATCH` like any other change to a pull request, so the proxy reads the
body to tell them apart. Changing a title or a description goes through.

Each guard asks by default. Settings, Extensions, scriptschnell-term sets Ask, Block or
Allow per kind for every forge (`sandbox.change_requests` in `settings.json`), and this
extension's page sets a mode for one of its guards, which wins. The dialog offers Block,
Allow (for one minute), Allow in this session and Always allow in this project. The
guards cover the hosts of the accounts and their subdomains and `*.visualstudio.com`. They need the
`guards` permission; without it the extension keeps working without them.

## Accounts

The extension has no settings of its own. Its accounts are `dev.azure.com` and
`dev.azure.com/<organization>` entries, one token each. The Accounts page edits them and
saves them under `forge_hosts` (`azure-devops.azure-devops`) in `settings.json`; an
`accounts` list from an earlier version moves there on load. The single Organization
setting of even older versions is gone: add `dev.azure.com/<that organization>` and sign
in to it.
