use std::collections::HashMap;

use aws_sdk_bedrockagentcore::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::Blob,
    types::{InvokeAgentRuntimeCommandRequestBody, InvokeAgentRuntimeCommandStreamOutput},
};
use bollard::{Docker, query_parameters::ListContainersOptionsBuilder};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:35469";
const DEFAULT_RUNTIME_ARN: &str =
    "arn:aws:bedrock-agentcore:us-west-2:000000000000:runtime/flint_local";
const CONTAINER_QUALIFIER: &str = "CONTAINER";
const SESSION_ID: &str = "20000000-0000-0000-0000-000000000091";

fn client() -> Client {
    let endpoint = std::env::var("AGENTCORE_RUNTIME_ENDPOINT_URL")
        .unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned());
    let config = aws_sdk_bedrockagentcore::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-west-2"))
        .credentials_provider(Credentials::new(
            "local-access-key",
            "local-secret-key",
            None,
            None,
            "flint-containerized-conformance",
        ))
        .endpoint_url(endpoint)
        .build();
    Client::from_conf(config)
}

fn runtime_arn() -> String {
    std::env::var("AGENTCORE_RUNTIME_ARN").unwrap_or_else(|_| DEFAULT_RUNTIME_ARN.to_owned())
}

#[tokio::test]
#[ignore = "requires a running Flint Compose service and local Docker"]
async fn containerized_runtime_operations_use_the_shared_network() {
    let client = client();
    let runtime_arn = runtime_arn();

    let unqualified = client
        .invoke_agent_runtime()
        .agent_runtime_arn(&runtime_arn)
        .runtime_session_id(SESSION_ID)
        .content_type("application/json")
        .accept("application/json")
        .payload(Blob::new(br#"{"fixture":"unqualified"}"#))
        .send()
        .await
        .expect("unqualified containerized invocation succeeds");
    assert_eq!(unqualified.status_code(), Some(200));
    assert_eq!(unqualified.runtime_session_id(), Some(SESSION_ID));
    assert_eq!(
        unqualified
            .response
            .collect()
            .await
            .expect("unqualified invocation body")
            .into_bytes()
            .as_ref(),
        br#"{"result":{"kind":"fixture"},"status":"completed"}"#
    );

    let qualified = client
        .invoke_agent_runtime()
        .agent_runtime_arn(&runtime_arn)
        .qualifier(CONTAINER_QUALIFIER)
        .runtime_session_id(SESSION_ID)
        .content_type("application/json")
        .accept("application/json")
        .payload(Blob::new(br#"{"fixture":"qualified"}"#))
        .send()
        .await
        .expect("explicitly qualified containerized invocation succeeds");
    assert_eq!(qualified.status_code(), Some(200));
    assert_eq!(qualified.runtime_session_id(), Some(SESSION_ID));
    assert_eq!(
        qualified
            .response
            .collect()
            .await
            .expect("qualified invocation body")
            .into_bytes()
            .as_ref(),
        br#"{"result":{"kind":"fixture"},"status":"completed"}"#
    );

    let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
    let runtime_owner = std::env::var("AGENTCORE_RUNTIME_OWNER")
        .unwrap_or_else(|_| "flint-container-test".to_owned());
    let filters = HashMap::from([(
        "label".to_owned(),
        vec!["agentcore.emulator.managed=true".to_owned()],
    )]);
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .expect("list Flint-managed runtime containers")
        .into_iter()
        .filter(|container| {
            container.labels.as_ref().is_some_and(|labels| {
                labels.get("agentcore.emulator.owner").map(String::as_str)
                    == Some(runtime_owner.as_str())
                    && labels
                        .get("agentcore.emulator.runtime-session-id")
                        .map(String::as_str)
                        == Some(SESSION_ID)
                    && labels
                        .get("agentcore.emulator.runtime-arn")
                        .map(String::as_str)
                        == Some(runtime_arn.as_str())
                    && labels
                        .get("agentcore.emulator.qualifier")
                        .map(String::as_str)
                        == Some(CONTAINER_QUALIFIER)
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        containers.len(),
        1,
        "one container is reused for the session"
    );
    let container_id = containers[0].id.as_deref().expect("container ID");
    let inspection = docker
        .inspect_container(container_id, None)
        .await
        .expect("inspect runtime container");
    let published_ports = inspection
        .network_settings
        .and_then(|settings| settings.ports)
        .map(|ports| {
            ports
                .values()
                .filter_map(|bindings| bindings.as_ref())
                .flatten()
                .count()
        })
        .unwrap_or_default();
    assert_eq!(
        published_ports, 0,
        "container mode must not publish runtime ports"
    );

    let mut command = client
        .invoke_agent_runtime_command()
        .agent_runtime_arn(&runtime_arn)
        .qualifier(CONTAINER_QUALIFIER)
        .runtime_session_id(SESSION_ID)
        .body(
            InvokeAgentRuntimeCommandRequestBody::builder()
                .command("printf containerized")
                .build()
                .expect("command body"),
        )
        .send()
        .await
        .expect("containerized command succeeds");
    let mut stdout = String::new();
    let mut exit = None;
    while let Some(event) = command.stream.recv().await.expect("command event") {
        if let InvokeAgentRuntimeCommandStreamOutput::Chunk(chunk) = event {
            if let Some(delta) = chunk.content_delta()
                && let Some(value) = delta.stdout()
            {
                stdout.push_str(value);
            }
            if let Some(stop) = chunk.content_stop() {
                exit = Some((stop.exit_code(), stop.status().as_str().to_owned()));
            }
        }
    }
    assert_eq!(stdout, "containerized");
    assert_eq!(exit, Some((0, "COMPLETED".to_owned())));

    let card = client
        .get_agent_card()
        .agent_runtime_arn(&runtime_arn)
        .qualifier(CONTAINER_QUALIFIER)
        .runtime_session_id(SESSION_ID)
        .send()
        .await
        .expect("containerized agent card succeeds");
    assert_eq!(card.status_code(), Some(200));

    let stopped = client
        .stop_runtime_session()
        .agent_runtime_arn(&runtime_arn)
        .qualifier(CONTAINER_QUALIFIER)
        .runtime_session_id(SESSION_ID)
        .client_token("flint-containerized-cleanup")
        .send()
        .await
        .expect("containerized stop succeeds");
    assert_eq!(stopped.status_code(), Some(200));
    assert!(docker.inspect_container(container_id, None).await.is_err());
}
