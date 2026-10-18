-- knowledge: collections of Markdown documents with titles, tags and
-- metadata. Agents create, change, list, search and read them through the
-- knowledge_* tools, and a view shows one.
--
-- Data lives in term.store, or with the `scope` setting on "project" in
-- term.project_store of each project: `collection/<name>` holds a
-- collection and `doc/<collection>/<id>` a document. Each such place is a
-- space. The extension keeps a copy of a space in memory after the first
-- read, so search does not decode the store again on every call.

local MAX_BODY = 1024 * 1024
local MAX_TITLE = 200
local MAX_TAGS = 32

-- Spaces by key: "app", or a project's id.
local spaces = {}
-- The current space, which every entry point picks first with `use`.
local space_key -- "app" or a project id
local store -- its store
local collections -- name -> collection
local docs -- collection name -> id -> document

local function now()
  return os.date("!%Y-%m-%dT%H:%M:%SZ")
end

-- Lowercase letters, digits and single dashes, at most 60 of them.
local function slug(text)
  local s = tostring(text or ""):lower():gsub("[^%w]+", "-"):gsub("^-+", ""):gsub("-+$", "")
  return (s:sub(1, 60):gsub("-+$", ""))
end

local function per_project()
  return term.settings().scope == "project"
end

-- Reads a space's collections and documents from its store.
local function read(space)
  for _, key in ipairs(space.store.keys("collection/")) do
    local c = space.store.get(key)
    if type(c) == "table" and c.name then
      space.collections[c.name] = c
      space.docs[c.name] = {}
    end
  end
  for _, key in ipairs(space.store.keys("doc/")) do
    local d = space.store.get(key)
    if type(d) == "table" and d.collection and d.id then
      if not space.collections[d.collection] then
        space.collections[d.collection] = { name = d.collection, title = d.collection }
        space.docs[d.collection] = {}
      end
      space.docs[d.collection][d.id] = d
    end
  end
end

-- Makes the space for `project_id` current: the app's, or with the scope
-- setting on "project" that project's. Returns nil and why when it needs
-- a project and has none.
local function use(project_id)
  local key = "app"
  if per_project() then
    if not project_id then
      return nil, "knowledge is kept per project, and no project is active here"
    end
    key = project_id
  end
  local space = spaces[key]
  if not space then
    space = {
      store = key == "app" and term.store or term.project_store(key),
      collections = {},
      docs = {},
    }
    read(space)
    spaces[key] = space
  end
  space_key, store, collections, docs = key, space.store, space.collections, space.docs
  return true
end

-- `use` for the space a view key names. False when that space
-- does not fit the scope setting anymore.
local function use_named(key)
  if (key == "app") == per_project() then return false end
  return use(key ~= "app" and key or nil) ~= nil
end

-- A view key: the space, `|`, then what the view shows.
local function view_key(rest)
  return space_key .. "|" .. rest
end

local function sorted_collections()
  local list = {}
  for _, c in pairs(collections) do list[#list + 1] = c end
  table.sort(list, function(a, b) return a.name < b.name end)
  return list
end

local function sorted_docs(name)
  local list = {}
  for _, d in pairs(docs[name] or {}) do list[#list + 1] = d end
  table.sort(list, function(a, b)
    local x, y = a.title:lower(), b.title:lower()
    if x == y then return a.id < b.id end
    return x < y
  end)
  return list
end

local function count(name)
  local n = 0
  for _ in pairs(docs[name] or {}) do n = n + 1 end
  return n
end

local function save_collection(c)
  store.set("collection/" .. c.name, c)
end

local function save_doc(d)
  store.set("doc/" .. d.collection .. "/" .. d.id, d)
end

-- Rendering ----------------------------------------------------------------

local function cell(text)
  return (tostring(text):gsub("|", "\\|"):gsub("\n", " "))
end

local function scalar(value)
  if type(value) == "table" then return term.json_encode(value) end
  return tostring(value)
end

local function sorted_keys(t)
  local keys = {}
  for k in pairs(t or {}) do keys[#keys + 1] = k end
  table.sort(keys)
  return keys
end

local function render_doc(d)
  local out = { "# " .. d.title, "" }
  local line = {}
  for _, tag in ipairs(d.tags or {}) do line[#line + 1] = "`" .. tag .. "`" end
  line[#line + 1] = "*" .. d.collection .. "/" .. d.id .. "*"
  line[#line + 1] = "updated " .. (d.updated or "?")
  if d.author then line[#line + 1] = "by " .. d.author end
  out[#out + 1] = table.concat(line, " · ")
  local keys = sorted_keys(d.metadata)
  if #keys > 0 then
    out[#out + 1] = ""
    out[#out + 1] = "| Key | Value |"
    out[#out + 1] = "| --- | --- |"
    for _, k in ipairs(keys) do
      out[#out + 1] = "| " .. cell(k) .. " | " .. cell(scalar(d.metadata[k])) .. " |"
    end
  end
  out[#out + 1] = ""
  out[#out + 1] = "---"
  out[#out + 1] = ""
  out[#out + 1] = d.body or ""
  return table.concat(out, "\n")
end

local function render_collection(name)
  local out = {}
  local list = name ~= "" and { collections[name] } or sorted_collections()
  if #list == 0 then
    return "# Knowledge\n\nNo documents yet. Agents in this app's terminals add them with the "
      .. "`knowledge_create` tool; ask one to write down what it learned."
  end
  for _, c in ipairs(list) do
    out[#out + 1] = "# " .. (c.title or c.name)
    out[#out + 1] = ""
    if c.description and c.description ~= "" then
      out[#out + 1] = c.description
      out[#out + 1] = ""
    end
    out[#out + 1] = "| Document | Id | Tags | Updated |"
    out[#out + 1] = "| --- | --- | --- | --- |"
    for _, d in ipairs(sorted_docs(c.name)) do
      out[#out + 1] = "| " .. cell(d.title) .. " | `" .. d.id .. "` | "
        .. cell(table.concat(d.tags or {}, ", ")) .. " | " .. (d.updated or "") .. " |"
    end
    out[#out + 1] = ""
  end
  return table.concat(out, "\n")
end

term.view{
  name = "doc",
  render = function(key)
    local space, rest = key:match("^([^|]+)|(.*)$")
    if not space or not use_named(space) then return nil end
    local name, id = rest:match("^([^/]+)/(.+)$")
    local d = name and docs[name] and docs[name][id]
    if not d then return nil end
    return { title = d.title, markdown = render_doc(d) }
  end,
}

term.view{
  name = "collection",
  render = function(key)
    local space, name = key:match("^([^|]+)|(.*)$")
    if not space or not use_named(space) then return nil end
    if name ~= "" and not collections[name] then return nil end
    local title = name == "" and "Knowledge" or (collections[name].title or name)
    return { title = title, markdown = render_collection(name) }
  end,
}

local function changed(name, id)
  if id then term.refresh_view("doc", view_key(name .. "/" .. id)) end
  term.refresh_view("collection", view_key(name))
  term.refresh_view("collection", view_key(""))
end

-- Arguments ----------------------------------------------------------------

local function check_string(args, key, required, max)
  local v = args[key]
  if v == nil then
    if required then return nil, "'" .. key .. "' is required" end
    return true
  end
  if type(v) ~= "string" then return nil, "'" .. key .. "' must be a string" end
  if required and v == "" then return nil, "'" .. key .. "' must not be empty" end
  if max and #v > max then return nil, "'" .. key .. "' is longer than " .. max .. " bytes" end
  return true
end

local function clean_tags(tags)
  if tags == nil then return {} end
  if type(tags) == "string" then tags = { tags } end
  if type(tags) ~= "table" then return nil, "'tags' must be a list of strings" end
  local out, seen = {}, {}
  for _, tag in ipairs(tags) do
    if type(tag) ~= "string" then return nil, "'tags' must be a list of strings" end
    tag = tag:lower():gsub("^%s+", ""):gsub("%s+$", "")
    if tag ~= "" and not seen[tag] then
      seen[tag] = true
      out[#out + 1] = tag
    end
  end
  if #out > MAX_TAGS then return nil, "at most " .. MAX_TAGS .. " tags" end
  return out
end

local function clean_metadata(meta)
  if meta == nil then return nil end
  if type(meta) ~= "table" then return nil, "'metadata' must be an object" end
  if #meta > 0 then return nil, "'metadata' must be an object, not a list" end
  if next(meta) == nil then return nil end
  return meta
end

-- An existing collection by name, or nil and why.
local function find_collection(args)
  local ok, err = check_string(args, "collection", true)
  if not ok then return nil, err end
  local c = collections[slug(args.collection)]
  if not c then return nil, "no collection named '" .. args.collection .. "'; see knowledge_collections" end
  return c
end

local function find_doc(args)
  local c, err = find_collection(args)
  if not c then return nil, err end
  local ok
  ok, err = check_string(args, "id", true)
  if not ok then return nil, err end
  local d = docs[c.name][args.id]
  if not d then return nil, "no document '" .. args.id .. "' in '" .. c.name .. "'; see knowledge_list" end
  return d
end

local function ensure_collection(name, title, description)
  local key = slug(name)
  if key == "" then return nil, "the collection name needs letters or digits" end
  local c = collections[key]
  if not c then
    c = { name = key, title = title or name, description = description, created = now() }
    c.updated = c.created
    collections[key] = c
    docs[key] = {}
    save_collection(c)
  end
  return c
end

local function summary(d)
  return {
    collection = d.collection,
    id = d.id,
    title = d.title,
    tags = d.tags,
    metadata = d.metadata,
    updated = d.updated,
  }
end

local function full(d)
  local s = summary(d)
  s.body = d.body
  s.created = d.created
  s.author = d.author
  return s
end

local function has_tags(d, tags)
  for _, want in ipairs(tags) do
    local found = false
    for _, tag in ipairs(d.tags or {}) do
      if tag == want then found = true break end
    end
    if not found then return false end
  end
  return true
end

local function limit_of(args, default)
  local n = tonumber(args.limit) or default
  return math.max(1, math.min(100, math.floor(n)))
end

-- Tools --------------------------------------------------------------------

local tag_list = { type = "array", items = { type = "string" } }

-- term.tool, with the caller's space made current before `run`.
local function tool(spec)
  local run = spec.run
  spec.run = function(args, caller)
    local ok, err = use(caller.project_id)
    if not ok then return nil, err end
    return run(args, caller)
  end
  term.tool(spec)
end

tool{
  name = "knowledge_collections",
  description = "Lists the knowledge collections with their titles, descriptions and document counts.",
  run = function()
    local out = {}
    for _, c in ipairs(sorted_collections()) do
      out[#out + 1] = {
        name = c.name, title = c.title, description = c.description, documents = count(c.name),
      }
    end
    return { collections = out }
  end,
}

tool{
  name = "knowledge_create_collection",
  description = "Creates a collection of documents, or changes the title and description of one.",
  input = {
    properties = {
      name = { type = "string", description = "Short name; becomes lowercase letters, digits and dashes" },
      title = { type = "string" },
      description = { type = "string", description = "What belongs in the collection" },
    },
    required = { "name" },
  },
  run = function(args)
    local ok, err = check_string(args, "name", true, 60)
    if not ok then return nil, err end
    for _, key in ipairs({ "title", "description" }) do
      ok, err = check_string(args, key, false, 2000)
      if not ok then return nil, err end
    end
    local c
    c, err = ensure_collection(args.name, args.title, args.description)
    if not c then return nil, err end
    if args.title then c.title = args.title end
    if args.description then c.description = args.description end
    c.updated = now()
    save_collection(c)
    changed(c.name)
    return { name = c.name, title = c.title, description = c.description }
  end,
}

tool{
  name = "knowledge_create",
  description = "Creates a Markdown document in a collection, creating the collection if needed. "
    .. "The body may use Mermaid diagrams (```mermaid) and math ($...$, $$...$$). Returns its id.",
  input = {
    properties = {
      collection = { type = "string" },
      title = { type = "string" },
      body = { type = "string", description = "Markdown" },
      tags = tag_list,
      metadata = { type = "object", description = "Key-value pairs such as status, source or owner" },
      id = { type = "string", description = "Optional id; by default one is made from the title" },
    },
    required = { "collection", "title", "body" },
  },
  run = function(args, caller)
    local ok, err = check_string(args, "collection", true, 60)
    if not ok then return nil, err end
    ok, err = check_string(args, "title", true, MAX_TITLE)
    if not ok then return nil, err end
    ok, err = check_string(args, "body", true, MAX_BODY)
    if not ok then return nil, err end
    ok, err = check_string(args, "id", false, 80)
    if not ok then return nil, err end
    local tags
    tags, err = clean_tags(args.tags)
    if not tags then return nil, err end
    local meta
    meta, err = clean_metadata(args.metadata)
    if err then return nil, err end
    local c
    c, err = ensure_collection(args.collection)
    if not c then return nil, err end
    local base = slug(args.id or args.title)
    if base == "" then base = "doc" end
    local id, n = base, 1
    while docs[c.name][id] do
      if args.id then return nil, "the id '" .. id .. "' is taken in '" .. c.name .. "'" end
      n = n + 1
      id = base .. "-" .. n
    end
    local stamp = now()
    local d = {
      collection = c.name, id = id, title = args.title, tags = tags, metadata = meta,
      body = args.body, created = stamp, updated = stamp, author = caller.agent,
    }
    docs[c.name][id] = d
    save_doc(d)
    changed(c.name, id)
    return summary(d)
  end,
}

tool{
  name = "knowledge_update",
  description = "Changes a document: its title, body (replace, or append to the end), tags or "
    .. "metadata. Metadata merges; a key set to an empty string goes away.",
  input = {
    properties = {
      collection = { type = "string" },
      id = { type = "string" },
      title = { type = "string" },
      body = { type = "string", description = "Replaces the body" },
      append = { type = "string", description = "Markdown added after the body" },
      tags = { type = "array", items = { type = "string" }, description = "Replaces the tags" },
      add_tags = tag_list,
      remove_tags = tag_list,
      metadata = { type = "object" },
    },
    required = { "collection", "id" },
  },
  run = function(args, caller)
    local d, err = find_doc(args)
    if not d then return nil, err end
    local ok
    for key, max in pairs({ title = MAX_TITLE, body = MAX_BODY, append = MAX_BODY }) do
      ok, err = check_string(args, key, false, max)
      if not ok then return nil, err end
    end
    if args.title == "" then return nil, "'title' must not be empty" end
    local tags = d.tags or {}
    if args.tags ~= nil then
      tags, err = clean_tags(args.tags)
      if not tags then return nil, err end
    end
    local add, remove
    add, err = clean_tags(args.add_tags)
    if not add then return nil, err end
    remove, err = clean_tags(args.remove_tags)
    if not remove then return nil, err end
    local meta
    meta, err = clean_metadata(args.metadata)
    if err then return nil, err end

    if args.title then d.title = args.title end
    if args.body then d.body = args.body end
    if args.append then
      local body = d.body or ""
      if body ~= "" and not body:match("\n$") then body = body .. "\n" end
      if #body + #args.append > MAX_BODY then return nil, "the body would be longer than " .. MAX_BODY .. " bytes" end
      d.body = body .. args.append
    end
    local merged, seen = {}, {}
    for _, list in ipairs({ tags, add }) do
      for _, tag in ipairs(list) do
        if not seen[tag] then seen[tag] = true merged[#merged + 1] = tag end
      end
    end
    for _, tag in ipairs(remove) do
      for i = #merged, 1, -1 do
        if merged[i] == tag then table.remove(merged, i) end
      end
    end
    if #merged > MAX_TAGS then return nil, "at most " .. MAX_TAGS .. " tags" end
    d.tags = merged
    if meta then
      d.metadata = d.metadata or {}
      for k, v in pairs(meta) do
        if v == "" then d.metadata[k] = nil else d.metadata[k] = v end
      end
      if next(d.metadata) == nil then d.metadata = nil end
    end
    d.updated = now()
    d.editor = caller.agent
    save_doc(d)
    changed(d.collection, d.id)
    return summary(d)
  end,
}

tool{
  name = "knowledge_list",
  description = "Lists documents without their bodies: all of them, those of one collection, "
    .. "or those with every given tag.",
  input = {
    properties = {
      collection = { type = "string" },
      tags = tag_list,
      limit = { type = "integer", description = "At most this many, 50 by default" },
    },
  },
  run = function(args)
    local names = {}
    if args.collection ~= nil then
      local c, err = find_collection(args)
      if not c then return nil, err end
      names[1] = c.name
    else
      for _, c in ipairs(sorted_collections()) do names[#names + 1] = c.name end
    end
    local tags, err = clean_tags(args.tags)
    if not tags then return nil, err end
    local limit = limit_of(args, 50)
    local out, total = {}, 0
    for _, name in ipairs(names) do
      for _, d in ipairs(sorted_docs(name)) do
        if has_tags(d, tags) then
          total = total + 1
          if #out < limit then out[#out + 1] = summary(d) end
        end
      end
    end
    return { documents = out, total = total }
  end,
}

-- Search -------------------------------------------------------------------

local function occurrences(text, word, cap)
  local n, from = 0, 1
  while n < cap do
    local at = text:find(word, from, true)
    if not at then break end
    n = n + 1
    from = at + #word
  end
  return n
end

local function snippet(body, words)
  local lower = body:lower()
  for _, word in ipairs(words) do
    local at = lower:find(word, 1, true)
    if at then
      local from = math.max(1, at - 60)
      local text = body:sub(from, at + #word + 100):gsub("%s+", " ")
      return (from > 1 and "..." or "") .. text .. "..."
    end
  end
  return (body:sub(1, 160):gsub("%s+", " "))
end

local function score(d, words)
  local title = d.title:lower()
  local body = (d.body or ""):lower()
  local tags = table.concat(d.tags or {}, " ")
  local meta = {}
  for k, v in pairs(d.metadata or {}) do meta[#meta + 1] = k .. " " .. scalar(v) end
  meta = table.concat(meta, " "):lower()
  local total, matched = 0, 0
  for _, word in ipairs(words) do
    local s = occurrences(title, word, 5) * 10
      + occurrences(tags, word, 5) * 6
      + occurrences(meta, word, 5) * 3
      + occurrences(body, word, 20)
    if s > 0 then matched = matched + 1 end
    total = total + s
  end
  return total, matched
end

tool{
  name = "knowledge_search",
  description = "Searches titles, tags, metadata and bodies for words, best matches first, "
    .. "with a snippet of each. Documents that hold every word come before those with some.",
  input = {
    properties = {
      query = { type = "string" },
      collection = { type = "string" },
      tags = tag_list,
      limit = { type = "integer" },
    },
    required = { "query" },
  },
  run = function(args)
    local ok, err = check_string(args, "query", true, 1000)
    if not ok then return nil, err end
    local words = {}
    for word in args.query:lower():gmatch("[^%s]+") do words[#words + 1] = word end
    local names = {}
    if args.collection ~= nil then
      local c
      c, err = find_collection(args)
      if not c then return nil, err end
      names[1] = c.name
    else
      for name in pairs(docs) do names[#names + 1] = name end
    end
    local tags
    tags, err = clean_tags(args.tags)
    if not tags then return nil, err end
    local hits = {}
    for _, name in ipairs(names) do
      for _, d in pairs(docs[name]) do
        if has_tags(d, tags) then
          local s, matched = score(d, words)
          if s > 0 then
            hits[#hits + 1] = { doc = d, score = s, matched = matched }
          end
        end
      end
    end
    table.sort(hits, function(a, b)
      if a.matched ~= b.matched then return a.matched > b.matched end
      if a.score ~= b.score then return a.score > b.score end
      return a.doc.id < b.doc.id
    end)
    local limit = limit_of(args, term.settings().search_limit or 10)
    local out = {}
    for i = 1, math.min(limit, #hits) do
      local hit = hits[i]
      local s = summary(hit.doc)
      s.score = hit.score
      s.snippet = snippet(hit.doc.body or "", words)
      out[#out + 1] = s
    end
    return { results = out, total = #hits }
  end,
}

tool{
  name = "knowledge_view",
  description = "Returns a document with its body, tags and metadata.",
  input = {
    properties = { collection = { type = "string" }, id = { type = "string" } },
    required = { "collection", "id" },
  },
  run = function(args)
    local d, err = find_doc(args)
    if not d then return nil, err end
    return full(d)
  end,
}

tool{
  name = "knowledge_delete",
  description = "Deletes a document, or with only `collection` an empty collection.",
  input = {
    properties = { collection = { type = "string" }, id = { type = "string" } },
    required = { "collection" },
  },
  run = function(args)
    if args.id == nil then
      local c, err = find_collection(args)
      if not c then return nil, err end
      if count(c.name) > 0 then
        return nil, "the collection still holds " .. count(c.name) .. " documents; delete them first"
      end
      collections[c.name], docs[c.name] = nil, nil
      store.delete("collection/" .. c.name)
      changed(c.name)
      return { deleted = c.name }
    end
    local d, err = find_doc(args)
    if not d then return nil, err end
    docs[d.collection][d.id] = nil
    store.delete("doc/" .. d.collection .. "/" .. d.id)
    changed(d.collection, d.id)
    return { deleted = d.collection .. "/" .. d.id }
  end,
}

tool{
  name = "knowledge_open",
  description = "Shows a document, or with only `collection` the collection's overview, to the "
    .. "user in a panel of the app.",
  input = {
    properties = { collection = { type = "string" }, id = { type = "string" } },
    required = { "collection" },
  },
  run = function(args)
    if args.id == nil then
      local c, err = find_collection(args)
      if not c then return nil, err end
      term.open_view("collection", view_key(c.name))
      return { opened = c.name }
    end
    local d, err = find_doc(args)
    if not d then return nil, err end
    term.open_view("doc", view_key(d.collection .. "/" .. d.id))
    return { opened = d.collection .. "/" .. d.id }
  end,
}

term.command{
  name = "knowledge.overview",
  title = "Knowledge: show all collections",
  run = function()
    if use(term.context().project_id) then term.open_view("collection", view_key("")) end
  end,
}
