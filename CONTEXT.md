# AgentCore Runtime Emulation

Flint emulates the local lifecycle of AWS Bedrock AgentCore Runtime services.
The context covers how runtimes are discovered, selected, authorized, invoked,
and kept available through replaceable compute.

## Runtime catalog

**Runtime**:
A named AgentCore service that accepts requests under a runtime identity,
protocol, and account and region context.
_Avoid_: Agent, container, deployment

**Deployment**:
A concrete serving configuration for a Runtime, selected by a qualifier and
used as the pinned basis for a Runtime Session.
_Avoid_: Runtime version, container

**Qualifier**:
A name that selects a Deployment of a Runtime. The current model supports the
`DEFAULT` qualifier.
_Avoid_: Environment, stage

**Runtime catalog**:
The set of Runtimes Flint can resolve for incoming requests.
_Avoid_: Registry, image list

**Runtime descriptor**:
The declaration supplied by a runtime source that names a Runtime and
describes its protocol, lifecycle requirements, and requested environment
values.
_Avoid_: Runtime configuration, container metadata

## Identity and access

**Local identity**:
The region and account context Flint derives from a request, using local
defaults for unsigned requests.
_Avoid_: User, tenant, AWS account

**Principal**:
The authenticated identity used when a Deployment evaluates authorization
policy.
_Avoid_: Local identity, runtime user

**Runtime user ID**:
An optional caller-supplied user context associated with an invocation and its
authorization decision.
_Avoid_: Principal, session ID

## Runtime sessions

**Runtime session**:
A durable logical context identified by a Runtime, qualifier, and session ID.
It can remain valid when no compute is currently running.
_Avoid_: Container, request, invocation

**Session compute**:
The replaceable active compute serving a Runtime Session. Replacing it does
not change the logical session.
_Avoid_: Runtime session, deployment

**Session lease**:
An operation's temporary hold on Session compute. Leases track activity and
provide cancellation when the operation or session ends.
_Avoid_: Session, lock

**Deployment drift**:
A mismatch between the Deployment pinned by an existing Runtime Session and
the Deployment currently available from the Runtime catalog.
_Avoid_: Catalog refresh, session expiration

## Operations

**Invocation**:
A request that sends application input to a Runtime Session for processing.
_Avoid_: Session, command

**Command**:
An administrative shell operation executed inside the Session compute, subject
to the Deployment's command policy.
_Avoid_: Invocation, container action

**Agent card**:
The A2A metadata document exposed by a Runtime that supports the agent-card
capability.
_Avoid_: Runtime descriptor, health document
