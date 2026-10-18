-- Blocks ad, tracker and malware hosts in sandboxed terminals. scriptschnell-term
-- downloads HaGeZi's DNS blocklist from GitHub, or from its Codeberg mirror
-- when GitHub fails, keeps the copy for a week, and its proxy refuses every
-- listed host and their subdomains.
--
-- scriptschnell-term installs this extension and keeps it current. The
-- settings come from manifest.toml; change them on the extension's page of
-- the settings (Ctrl+,).

local settings = term.settings()

local GITHUB = "https://raw.githubusercontent.com/hagezi/dns-blocklists/main/wildcard/"
local CODEBERG = "https://codeberg.org/hagezi/mirror2/raw/branch/main/dns-blocklists/wildcard/"

local max_age = math.max(1, settings.max_age_days) * 24 * 60 * 60

local lists = {}
if settings.list ~= "off" then
  local file = settings.list .. "-onlydomains.txt"
  lists[#lists + 1] = {
    name = "hagezi-" .. settings.list:gsub("%.", "-"),
    urls = { GITHUB .. file, CODEBERG .. file },
    max_age = max_age,
  }
end
for index, url in ipairs(settings.urls) do
  lists[#lists + 1] = { name = "list-" .. index, urls = { url }, max_age = max_age }
end

local ok, error = pcall(term.sandbox, {
  blocklists = lists,
  blocked = settings.blocked,
  unblocked = settings.unblocked,
})
if not ok then
  term.notify("blocklist: " .. tostring(error), "warning")
end
