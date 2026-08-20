# Emulate the deployed AgentCore Runtime service, not harness provisioning

Flint emulates the behavior of an already-deployed AWS Bedrock AgentCore
Runtime on a local Docker host. Its boundary includes local runtime discovery
and configuration, runtime identity and deployment resolution, logical runtime
sessions, replaceable session compute, lifecycle management, and the
runtime-facing API surface for invocation, commands, stopping sessions, agent
cards, and health. It does not emulate AgentCore application harness creation,
project scaffolding, application packaging, or the broader
resource-provisioning APIs used to create and manage AgentCore applications
and infrastructure.

Catalog loading and Docker image discovery are Flint's local control-plane
mechanisms for making a deployed runtime available. They are not intended to
reproduce the complete AWS AgentCore deployment control plane. Documentation
should therefore describe Flint as an emulator of the deployed Runtime service
and its local runtime lifecycle, and must not imply that Flint exposes create,
update, or delete APIs for AgentCore runtimes or harnesses.

## Consequences

- Local catalogs and labeled Docker images are the deployment inputs for
  Flint.
- Runtime sessions and their compute lifecycle are in scope.
- Harness creation, project setup, and general AgentCore resource
  provisioning are out of scope.
- The public API model should distinguish runtime operations from local
  control-plane internals.
- Future runtime deployment-management APIs would require an explicit scope
  decision rather than being assumed by this ADR.
