-- Lets sandboxed terminals use toolchain managers and global tools: rustup
-- installs the toolchain rust-toolchain.toml asks for, nvm installs a Node
-- version, npx and pnpm dlx download and run a package, dotnet tool install
-- adds a global tool. Each tool's home becomes writable and its download
-- hosts are allowed. A variable such as RUSTUP_HOME or NVM_DIR moves a home.
--
-- What a sandbox writes here, the host runs later outside the sandbox. Turn
-- tools off on the extension's page of the settings (Ctrl+,) to keep their
-- homes read-only.

local settings = term.settings()

-- name: the setting that turns the tool off.
-- homes: { variable, default } pairs; the variable wins when it is set.
--   A third field appends a path inside the home.
-- paths: further paths that need no variable.
local TOOLS = {
  {
    name = "rustup",
    homes = { { "RUSTUP_HOME", "~/.rustup" } },
    hosts = { "static.rust-lang.org" },
  },
  {
    -- cargo install and rustup's proxies. The registry cache belongs to the
    -- cargo extension.
    name = "cargo",
    homes = {
      { "CARGO_HOME", "~/.cargo", "/bin" },
      { "CARGO_HOME", "~/.cargo", "/.crates.toml" },
      { "CARGO_HOME", "~/.cargo", "/.crates2.json" },
      { "CARGO_HOME", "~/.cargo", "/.package-cache" },
      { "CARGO_HOME", "~/.cargo", "/.package-cache-mutate" },
      { "CARGO_HOME", "~/.cargo", "/.global-cache" },
    },
    hosts = { "crates.io", "index.crates.io", "static.crates.io" },
  },
  {
    name = "nvm",
    homes = { { "NVM_DIR", "~/.nvm" } },
    hosts = { "nodejs.org" },
  },
  {
    name = "fnm",
    homes = { { "FNM_DIR", "~/.local/share/fnm" } },
    paths = { "~/.fnm", "~/.local/state/fnm_multishells" },
    hosts = { "nodejs.org" },
  },
  {
    name = "volta",
    homes = { { "VOLTA_HOME", "~/.volta" } },
    hosts = { "nodejs.org", "registry.npmjs.org" },
  },
  {
    -- npx keeps its packages in the npm cache.
    name = "npm",
    homes = {
      { "npm_config_cache", "~/.npm" },
      { "NPM_CONFIG_PREFIX", "~/.npm-global" },
    },
    hosts = { "registry.npmjs.org" },
  },
  {
    name = "pnpm",
    homes = { { "PNPM_HOME", "~/.local/share/pnpm" } },
    hosts = { "registry.npmjs.org" },
  },
  {
    name = "yarn",
    paths = { "~/.yarn", "~/.config/yarn" },
    hosts = { "registry.npmjs.org", "registry.yarnpkg.com", "repo.yarnpkg.com" },
  },
  {
    name = "bun",
    homes = { { "BUN_INSTALL", "~/.bun" } },
    hosts = { "registry.npmjs.org", "bun.sh" },
  },
  {
    name = "deno",
    homes = { { "DENO_INSTALL_ROOT", "~/.deno" }, { "DENO_DIR", "~/.cache/deno" } },
    hosts = { "deno.land", "dl.deno.land", "jsr.io", "registry.npmjs.org" },
  },
  {
    name = "pyenv",
    homes = { { "PYENV_ROOT", "~/.pyenv" } },
    hosts = { "www.python.org" },
  },
  {
    -- Python builds come from GitHub releases.
    name = "uv",
    homes = {
      { "UV_PYTHON_INSTALL_DIR", "~/.local/share/uv/python" },
      { "UV_TOOL_DIR", "~/.local/share/uv/tools" },
    },
    hosts = { "pypi.org", "files.pythonhosted.org" },
  },
  {
    name = "pipx",
    homes = { { "PIPX_HOME", "~/.local/share/pipx" } },
    hosts = { "pypi.org", "files.pythonhosted.org" },
  },
  {
    -- Module cache, go install and the toolchains GOTOOLCHAIN downloads.
    name = "go",
    homes = {
      { "GOMODCACHE", "~/go/pkg/mod" },
      { "GOBIN", "~/go/bin" },
    },
    hosts = { "proxy.golang.org", "sum.golang.org", "go.dev", "dl.google.com" },
  },
  {
    -- dotnet tool install, the SDKs dotnet-install.sh puts into ~/.dotnet,
    -- and the CLI's first-run files there.
    name = "dotnet",
    homes = {
      { "DOTNET_CLI_HOME", "~", "/.dotnet" },
      { "NUGET_PACKAGES", "~/.nuget/packages" },
    },
    paths = { "~/.local/share/NuGet" },
    hosts = {
      "api.nuget.org",
      "www.nuget.org",
      "globalcdn.nuget.org",
      "dot.net",
      "builds.dotnet.microsoft.com",
      "dotnetcli.azureedge.net",
      "dotnetcli.blob.core.windows.net",
      "ci.dot.net",
    },
  },
  {
    name = "mise",
    homes = {
      { "MISE_DATA_DIR", "~/.local/share/mise" },
      { "MISE_STATE_DIR", "~/.local/state/mise" },
    },
    hosts = { "mise.jdx.dev", "mise-versions.jdx.dev", "nodejs.org", "static.rust-lang.org" },
  },
  {
    name = "asdf",
    homes = { { "ASDF_DATA_DIR", "~/.asdf" } },
  },
  {
    name = "sdkman",
    homes = { { "SDKMAN_DIR", "~/.sdkman" } },
    hosts = { "api.sdkman.io", "broker.sdkman.io" },
  },
  {
    name = "rbenv",
    homes = { { "RBENV_ROOT", "~/.rbenv" } },
    hosts = { "cache.ruby-lang.org" },
  },
  {
    name = "gem",
    homes = { { "GEM_HOME", "~/.gem" } },
    paths = { "~/.local/share/gem" },
    hosts = { "rubygems.org", "index.rubygems.org" },
  },
}

local function append(list, item)
  for _, have in ipairs(list) do
    if have == item then
      return
    end
  end
  list[#list + 1] = item
end

local function rules()
  local off = {}
  for _, name in ipairs(settings.off) do
    off[name] = true
  end
  local rules = { hosts = {}, writable = {} }
  for _, tool in ipairs(TOOLS) do
    if not off[tool.name] then
      for _, home in ipairs(tool.homes or {}) do
        local value = os.getenv(home[1])
        if not value or value == "" then
          value = home[2]
        end
        append(rules.writable, value .. (home[3] or ""))
      end
      for _, path in ipairs(tool.paths or {}) do
        append(rules.writable, path)
      end
      for _, host in ipairs(tool.hosts or {}) do
        append(rules.hosts, host)
      end
    end
  end
  for _, path in ipairs(settings.writable) do
    append(rules.writable, path)
  end
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("toolchains: " .. tostring(error), "warning")
  end
end

apply()

-- Pick up moved homes when a worktree turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
  end
end)
