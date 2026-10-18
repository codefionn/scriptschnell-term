# kubernetes

Opens projects inside Kubernetes pods. Add a project as `k8s://pod/path`, such as
`k8s://dev-0/~/src/app` or `k8s://dev-0/workspace`, and the app runs its VCS queries,
terminals and extension jobs in the pod through `kubectl exec`. The pod needs `sh` and
the VCS you use; `term.copy` also needs `tar`.

It is off until you turn it on (Ctrl+,, Extensions, show all, kubernetes). It needs the
`environments` permission, which lets it name the program, `kubectl`, that reaches the
pods.

## Settings

- Context: the kubeconfig context. Empty uses the current one.
- Namespace: the namespace of the pods. Empty uses the context's namespace.
- Container: the container in each pod. Empty uses the pod's default container.

The settings apply to every `k8s://` project. `kubectl` runs on this machine with your
kubeconfig, so it reaches whatever your credentials reach.

## Limits

Terminals end when the pod restarts, and a new pod under another name needs a new
project. Agents in the pod's terminals cannot reach the app's control socket, since
`kubectl exec` forwards no Unix sockets.
