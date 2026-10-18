# knowledge

Collections of Markdown documents with titles, tags and metadata. Agents in the app's
terminals create, change, list, search and read them through tools, and a panel shows
them.

## What it does

The palette command "Knowledge: show all collections" (`knowledge.overview`) lists
every collection and its documents in a panel. `knowledge_open` shows one document
there: its title, tags, metadata as a table and the Markdown body, with Mermaid
diagrams and math. Open panels follow changes right away, and F5 or `r` in one
renders it again.

Agents reach the tools through the control socket (`GET /v1/tools`,
`POST /v1/tools/<name>` with the arguments as a JSON object) or the MCP server the
Agents page of the settings installs:

| Tool | Does |
| --- | --- |
| `knowledge_collections` | Lists the collections with titles, descriptions and counts |
| `knowledge_create_collection` | Creates a collection, or changes its title and description |
| `knowledge_create` | Creates a document: `collection`, `title`, `body`, optional `tags`, `metadata`, `id`. Creates the collection if needed |
| `knowledge_update` | Changes `title`, `body`, appends with `append`, replaces `tags`, `add_tags`, `remove_tags`, merges `metadata` (an empty string removes a key) |
| `knowledge_list` | Lists documents without bodies, by `collection` and `tags` |
| `knowledge_search` | Searches titles, tags, metadata and bodies for the words of `query`, best first, with snippets |
| `knowledge_view` | Returns one document with its body |
| `knowledge_delete` | Deletes a document, or an empty collection |
| `knowledge_open` | Shows a document or a collection's overview to the user in a panel |

Collection names and document ids are lowercase letters, digits and dashes, made from
the name or title. Tags are lowercase. A document records when it was created and
changed and which agent created it.

```sh
curl -s --unix-socket "$SCRIPTSCHNELL_TERM_SOCKET" \
  -H "Authorization: Bearer $SCRIPTSCHNELL_TERM_TOKEN" \
  -d '{"collection": "decisions", "title": "Use SQLite", "body": "We use SQLite because...", "tags": ["storage"]}' \
  http://scriptschnell-term/v1/tools/knowledge_create
```

## Data

The `scope` setting says where documents live:

- `app`, the default: in `term.store`, one JSON file per collection and per document
  under `extension-data/knowledge` of the data directory
  (`~/.local/share/scriptschnell-term` on Linux). Every project's agents share them.
- `project`: in `term.project_store` of each project, under
  `project-data/<project id>/knowledge`. The palette command shows the collections of
  the active project, and the tools work on those of the calling terminal's project.
  A terminal outside a project gets an error.

Switching the scope does not move documents; the other place keeps its own.

## Settings

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `search_limit` | integer | 10 | Results `knowledge_search` returns when the agent gives no `limit` |
| `scope` | choice | `app` | `app` for one set of collections, `project` for one per project |

## Permissions

`agents` for the tools, `storage` for the documents of the app and `project_storage`
for those of each project. A shipped copy has them from the start. Revoke on its page
of the settings (Ctrl+,) takes them away, and the extension then waits for a new grant
before it loads.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description, permissions and settings.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
