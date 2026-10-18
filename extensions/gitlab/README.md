# gitlab

Tells the rail's Agents tab whether the branch of a finished agent has a merge request on
GitLab, and whether it is open, merged or closed. Merged ones move into the collapsed
Merged group.

## Signing in

Open the Accounts page of the settings (Ctrl+, then Accounts):

1. Create token opens GitLab's page for a personal access token in your browser, with
   the name and the `read_api` scope filled in.
2. Paste the token into the field and press Sign in. scriptschnell-term checks it
   against `/api/v4/user`, keeps it in the system's secret storage and shows your user
   name.

Without a sign-in, a token in `GITLAB_TOKEN` is used for gitlab.com accounts. Public projects
work without a token. The token goes only to the host it belongs to; the extension
itself never sees it.

## Several accounts

Each account on the Accounts page gets its own sign-in. Type an entry into the field under the forge's accounts and press Add account; Remove on an
account's row drops it and signs out of it. An entry is a host, or a host and a group
such as `gitlab.com/acme`, which holds only the repositories under it. A repository takes
the entry on its host with the longest path its own path starts with. With `gitlab.com` and
`gitlab.com/acme` listed, `acme/app` uses the `acme` token and `octo/app` the
`gitlab.com` one. A repository that matches no entry is not checked, so keep the bare host
listed for everything else. `GITLAB_TOKEN` stands in for every entry on gitlab.com without a sign-in. Subgroups work too: `gitlab.com/acme/team` beats `gitlab.com/acme` for the projects under `acme/team`.

## Requests

scriptschnell-term asks only about worktrees whose agent finished and whose branch is not
known to be merged, at most every three minutes. The extension asks for up to 20
projects of a host, with all their branches, in one GraphQL request. A 429 answer pauses
the checks for as long as GitLab's `Retry-After` says, or 15 minutes.

## Discussions

The button beside an agent's row in the Agents tab opens the merge request's status and
comments on the Change request page of the explore panel. Four requests read the merge
request, its discussions, its approvals and its versions, one version per push, and one
more the jobs of its head pipeline. GitLab gives no diff hunk with a comment, so a
comment whose commit is not in the repository shows as gone.

scriptschnell-term places every code comment on the worktree's current head with git, so
comments written before a force push say whether their line is unchanged, moved, changed
or gone. The page is read-only; reply on GitLab.

## Sandboxes

Terminals in a sandbox reach the forge through scriptschnell-term's proxy, which holds
back requests that open, comment on or merge merge requests until you answer:

| Guard | Holds back |
| --- | --- |
| `gitlab.create` | `POST /api/v4/projects/{id}/merge_requests`, GraphQL `mergeRequestCreate` (`glab mr create`) |
| `gitlab.comment` | notes, discussions, draft notes and approvals on a merge request, GraphQL `createNote` and `createDiffNote` (`glab mr note`, `glab mr approve`) |
| `gitlab.merge` | `PUT /api/v4/projects/{id}/merge_requests/{iid}/merge`, merge trains, GraphQL `mergeRequestAccept` (`glab mr merge`) |

Each guard asks by default. Settings, Extensions, scriptschnell-term sets Ask, Block or
Allow per kind for every forge (`sandbox.change_requests` in `settings.json`), and this
extension's page sets a mode for one of its guards, which wins. The dialog offers Block,
Allow (for one minute), Allow in this session and Always allow in this project. The
guards cover the hosts of the accounts and their subdomains. They need the
`guards` permission; without it the extension keeps working without them.

## Accounts

The extension has no settings of its own. Its accounts are gitlab.com, self-managed hosts,
and groups on them such as `gitlab.com/acme`, each with its own sign-in. The Accounts
page edits them and saves them under `forge_hosts` (`gitlab.gitlab`) in `settings.json`.
Without an entry the extension uses gitlab.com.
