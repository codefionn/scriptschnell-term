-- Opens projects inside Kubernetes pods. `k8s://pod/path` is a project at
-- `path` in the pod `pod`; the app runs its VCS queries, terminals and
-- extension jobs there through `kubectl exec`.
--
-- scriptschnell-term installs this extension turned off. Turn it on on the
-- Extensions page of the settings (Ctrl+,).

local settings = term.settings()

-- `kubectl [--context C] exec <flags> [-n N] [-c C] {target} -- sh -c {script}`
local function args(flags)
  local list = {}
  if settings.context ~= "" then
    table.insert(list, "--context=" .. settings.context)
  end
  table.insert(list, "exec")
  table.insert(list, flags)
  if settings.namespace ~= "" then
    table.insert(list, "--namespace=" .. settings.namespace)
  end
  if settings.container ~= "" then
    table.insert(list, "--container=" .. settings.container)
  end
  for _, arg in ipairs({ "{target}", "--", "sh", "-c", "{script}" }) do
    table.insert(list, arg)
  end
  return list
end

term.environment{
  name = "k8s",
  title = "Kubernetes pods",
  program = "kubectl",
  -- `-i` passes stdin through, which `term.copy` needs.
  args = args("-i"),
  pty_args = args("-it"),
}
