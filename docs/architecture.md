# Architecture

## Boot Phases

Components start in four sequential phases. Within each phase, components start
in parallel. If any component in a phase fails, subsequent phases are skipped.
Once all components are running, KuberNix deploys the cluster addons (CoreDNS by
default) in the background.

```mermaid
graph TD
    A["Phase 1: Infrastructure"] --> B["Phase 2: ControlPlane"]
    B --> C["Phase 3: Controller"]
    C --> D["Phase 4: NodeAgent"]

    A --- etcd
    B --- apiserver
    C --- scheduler & controller-manager & cri["cri-o or containerd (x N)"] & proxy
    D --- kubelet["kubelet (x N)"]
```
