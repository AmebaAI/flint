use std::collections::HashMap;

use axum::http::{HeaderMap, Method, Uri};
use chrono::{DateTime, NaiveDateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::catalog::{
    AuthenticationMode, AuthorizationPolicy, DEFAULT_ACCOUNT_ID, LocalIdentity, PolicyEffect,
    PolicyStatement, ResolvedCredential, ResolvedRuntime,
};

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const TERMINATOR: &str = "aws4_request";
const SERVICE: &str = "bedrock-agentcore";
const AUTHORIZATION_HEADER: &str = "authorization";
const AMZ_DATE_HEADER: &str = "x-amz-date";
const AMZ_CONTENT_SHA256_HEADER: &str = "x-amz-content-sha256";
const AMZ_SECURITY_TOKEN_HEADER: &str = "x-amz-security-token";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeIamAction {
    InvokeAgentRuntime,
    InvokeAgentRuntimeForUser,
    InvokeAgentRuntimeCommand,
    StopRuntimeSession,
    GetAgentCard,
}

impl RuntimeIamAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvokeAgentRuntime => "bedrock-agentcore:InvokeAgentRuntime",
            Self::InvokeAgentRuntimeForUser => "bedrock-agentcore:InvokeAgentRuntimeForUser",
            Self::InvokeAgentRuntimeCommand => "bedrock-agentcore:InvokeAgentRuntimeCommand",
            Self::StopRuntimeSession => "bedrock-agentcore:StopRuntimeSession",
            Self::GetAgentCard => "bedrock-agentcore:GetAgentCard",
        }
    }
}

pub(crate) struct AuthorizationRequest<'a> {
    pub(crate) method: &'a Method,
    pub(crate) uri: &'a Uri,
    pub(crate) headers: &'a HeaderMap,
    pub(crate) body: &'a [u8],
    pub(crate) action: RuntimeIamAction,
    pub(crate) runtime_user_id: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthorizedPrincipal {
    #[allow(dead_code)]
    pub(crate) principal_arn: Option<String>,
}

pub(crate) fn local_identity(headers: &HeaderMap) -> Result<LocalIdentity, AuthorizationError> {
    let Some(value) = headers.get(AUTHORIZATION_HEADER) else {
        return Ok(LocalIdentity::default());
    };
    let authorization = parse_authorization(header_text(value, AUTHORIZATION_HEADER)?)?;
    if authorization.service != SERVICE || authorization.terminator != TERMINATOR {
        return Err(AuthorizationError::AccessDenied(
            "credential scope has the wrong service or terminator".to_owned(),
        ));
    }
    let account_id = if authorization.access_key_id.len() == 12
        && authorization
            .access_key_id
            .bytes()
            .all(|character| character.is_ascii_digit())
    {
        authorization.access_key_id
    } else {
        DEFAULT_ACCOUNT_ID.to_owned()
    };
    Ok(LocalIdentity {
        region: authorization.region,
        account_id,
    })
}

pub(crate) fn authorize(
    runtime: &ResolvedRuntime,
    request: AuthorizationRequest<'_>,
) -> Result<AuthorizedPrincipal, AuthorizationError> {
    match runtime.authentication.mode {
        AuthenticationMode::Permissive => authorize_permissive(runtime, request),
        AuthenticationMode::Signature | AuthenticationMode::Policy => {
            let principal = verify_signature(runtime, &request, Utc::now())?;
            if runtime.authentication.mode == AuthenticationMode::Policy {
                authorize_policy(runtime, &request, principal)?;
            }
            Ok(AuthorizedPrincipal {
                principal_arn: principal.principal_arn.clone(),
            })
        }
    }
}

fn authorize_permissive(
    runtime: &ResolvedRuntime,
    request: AuthorizationRequest<'_>,
) -> Result<AuthorizedPrincipal, AuthorizationError> {
    let Some(value) = request.headers.get(AUTHORIZATION_HEADER) else {
        return Ok(AuthorizedPrincipal {
            principal_arn: None,
        });
    };
    let authorization = parse_authorization(header_text(value, AUTHORIZATION_HEADER)?)?;
    validate_scope(runtime, &authorization)?;
    validate_signed_headers(request.headers, &authorization.signed_headers)?;
    let date = required_header(request.headers, AMZ_DATE_HEADER)?;
    if !date.starts_with(&authorization.date) {
        return Err(AuthorizationError::AccessDenied(
            "credential date does not match x-amz-date".to_owned(),
        ));
    }
    Ok(AuthorizedPrincipal {
        principal_arn: None,
    })
}

fn verify_signature<'a>(
    runtime: &'a ResolvedRuntime,
    request: &AuthorizationRequest<'_>,
    now: DateTime<Utc>,
) -> Result<&'a ResolvedCredential, AuthorizationError> {
    let authorization =
        parse_authorization(required_header(request.headers, AUTHORIZATION_HEADER)?)?;
    validate_scope(runtime, &authorization)?;
    validate_signed_headers(request.headers, &authorization.signed_headers)?;
    let credential = runtime
        .authentication
        .credentials
        .iter()
        .find(|credential| credential.access_key_id == authorization.access_key_id)
        .ok_or_else(|| AuthorizationError::AccessDenied("unknown access key ID".to_owned()))?;
    validate_session_token(request.headers, credential)?;

    let amz_date = required_header(request.headers, AMZ_DATE_HEADER)?;
    if !amz_date.starts_with(&authorization.date) {
        return Err(AuthorizationError::AccessDenied(
            "credential date does not match x-amz-date".to_owned(),
        ));
    }
    let request_time = parse_amz_date(amz_date)?;
    let skew = (now - request_time).num_seconds().unsigned_abs();
    if skew > runtime.authentication.allowed_clock_skew_seconds {
        return Err(AuthorizationError::AccessDenied(
            "request timestamp is outside the allowed clock skew".to_owned(),
        ));
    }

    let payload_hash = sha256_hex(request.body);
    if let Some(header_hash) = request
        .headers
        .get(AMZ_CONTENT_SHA256_HEADER)
        .map(|value| header_text(value, AMZ_CONTENT_SHA256_HEADER))
        .transpose()?
        && header_hash != payload_hash
    {
        return Err(AuthorizationError::AccessDenied(
            "request payload hash does not match x-amz-content-sha256".to_owned(),
        ));
    }
    let canonical_request = canonical_request(request, &authorization, &payload_hash)?;
    let scope = format!(
        "{}/{}/{}/{}",
        authorization.date, authorization.region, authorization.service, authorization.terminator
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signing_key = signing_key(
        &credential.secret_access_key,
        &authorization.date,
        &authorization.region,
        &authorization.service,
    );
    let mut verifier =
        Hmac::<Sha256>::new_from_slice(&signing_key).expect("HMAC accepts keys of any length");
    verifier.update(string_to_sign.as_bytes());
    let signature = hex::decode(&authorization.signature)
        .map_err(|_| AuthorizationError::AccessDenied("signature is not hexadecimal".to_owned()))?;
    verifier
        .verify_slice(&signature)
        .map_err(|_| AuthorizationError::AccessDenied("signature does not match".to_owned()))?;
    Ok(credential)
}

fn authorize_policy(
    runtime: &ResolvedRuntime,
    request: &AuthorizationRequest<'_>,
    credential: &ResolvedCredential,
) -> Result<(), AuthorizationError> {
    let principal = credential.principal_arn.as_deref().ok_or_else(|| {
        AuthorizationError::AccessDenied("policy mode requires a principal ARN".to_owned())
    })?;
    let mut context = HashMap::from([
        ("aws:PrincipalArn", principal),
        ("aws:RequestedRegion", runtime_region(runtime)?),
        ("bedrock-agentcore:Qualifier", runtime.qualifier.as_str()),
    ]);
    if let Some(runtime_user_id) = request.runtime_user_id {
        context.insert("bedrock-agentcore:RuntimeUserId", runtime_user_id);
    }

    evaluate_action(
        &runtime.policy,
        request.action,
        &runtime.runtime_arn,
        principal,
        &context,
    )?;
    if request.action == RuntimeIamAction::InvokeAgentRuntime && request.runtime_user_id.is_some() {
        evaluate_action(
            &runtime.policy,
            RuntimeIamAction::InvokeAgentRuntimeForUser,
            &runtime.runtime_arn,
            principal,
            &context,
        )?;
    }
    Ok(())
}

fn evaluate_action(
    policy: &AuthorizationPolicy,
    action: RuntimeIamAction,
    resource: &str,
    principal: &str,
    context: &HashMap<&str, &str>,
) -> Result<(), AuthorizationError> {
    let identity = policy
        .identity_statements
        .iter()
        .filter(|statement| statement_matches(statement, action, resource, None, context))
        .collect::<Vec<_>>();
    let resource_matches = policy
        .resource_statements
        .iter()
        .filter(|statement| {
            statement_matches(statement, action, resource, Some(principal), context)
        })
        .collect::<Vec<_>>();
    if identity
        .iter()
        .chain(resource_matches.iter())
        .any(|statement| statement.effect == PolicyEffect::Deny)
    {
        return Err(AuthorizationError::AccessDenied(format!(
            "explicit policy deny for {}",
            action.as_str()
        )));
    }
    if !identity
        .iter()
        .any(|statement| statement.effect == PolicyEffect::Allow)
    {
        return Err(AuthorizationError::AccessDenied(format!(
            "no identity policy allows {}",
            action.as_str()
        )));
    }
    if !policy.resource_statements.is_empty()
        && !resource_matches
            .iter()
            .any(|statement| statement.effect == PolicyEffect::Allow)
    {
        return Err(AuthorizationError::AccessDenied(format!(
            "no resource policy allows {}",
            action.as_str()
        )));
    }
    Ok(())
}

fn statement_matches(
    statement: &PolicyStatement,
    action: RuntimeIamAction,
    resource: &str,
    principal: Option<&str>,
    context: &HashMap<&str, &str>,
) -> bool {
    statement
        .actions
        .iter()
        .any(|pattern| wildcard_match(pattern, action.as_str()))
        && statement
            .resources
            .iter()
            .any(|pattern| wildcard_match(pattern, resource))
        && principal.is_none_or(|principal| {
            statement
                .principals
                .iter()
                .any(|pattern| wildcard_match(pattern, principal))
        })
        && conditions_match(statement, context)
}

fn conditions_match(statement: &PolicyStatement, context: &HashMap<&str, &str>) -> bool {
    statement.conditions.iter().all(|(operator, conditions)| {
        conditions.iter().all(|(key, expected)| {
            let Some(actual) = context.get(key.as_str()) else {
                return false;
            };
            match operator.as_str() {
                "StringEquals" | "ArnEquals" => expected.iter().any(|value| value == actual),
                "StringLike" | "ArnLike" => {
                    expected.iter().any(|value| wildcard_match(value, actual))
                }
                _ => false,
            }
        })
    })
}

fn canonical_request(
    request: &AuthorizationRequest<'_>,
    authorization: &AuthorizationHeader,
    payload_hash: &str,
) -> Result<String, AuthorizationError> {
    let mut canonical_headers = String::new();
    for name in &authorization.signed_headers {
        let values = request
            .headers
            .get_all(name)
            .iter()
            .map(|value| header_text(value, name).map(normalize_header_value))
            .collect::<Result<Vec<_>, _>>()?;
        if values.is_empty() {
            return Err(AuthorizationError::AccessDenied(format!(
                "signed header {name} is missing"
            )));
        }
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&values.join(","));
        canonical_headers.push('\n');
    }
    Ok(format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        canonical_uri(request.uri.path()),
        canonical_query(request.uri.query()),
        canonical_headers,
        authorization.signed_headers.join(";"),
        payload_hash,
    ))
}

fn canonical_uri(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    uri_encode(path.as_bytes(), true)
}

fn canonical_query(query: Option<&str>) -> String {
    let mut parameters = query
        .unwrap_or_default()
        .split('&')
        .filter(|parameter| !parameter.is_empty())
        .map(|parameter| parameter.split_once('=').unwrap_or((parameter, "")))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect::<Vec<_>>();
    parameters.sort();
    parameters
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn uri_encode(value: &[u8], preserve_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for &byte in value {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (preserve_slash && byte == b'/')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn validate_scope(
    runtime: &ResolvedRuntime,
    authorization: &AuthorizationHeader,
) -> Result<(), AuthorizationError> {
    if authorization.service != SERVICE || authorization.terminator != TERMINATOR {
        return Err(AuthorizationError::AccessDenied(
            "credential scope has the wrong service or terminator".to_owned(),
        ));
    }
    if authorization.region != runtime_region(runtime)? {
        return Err(AuthorizationError::AccessDenied(
            "credential scope has the wrong region".to_owned(),
        ));
    }
    Ok(())
}

fn runtime_region(runtime: &ResolvedRuntime) -> Result<&str, AuthorizationError> {
    runtime
        .runtime_arn
        .split(':')
        .nth(3)
        .filter(|region| !region.is_empty())
        .ok_or_else(|| AuthorizationError::AccessDenied("runtime ARN has no region".to_owned()))
}

fn validate_signed_headers(
    headers: &HeaderMap,
    signed_headers: &[String],
) -> Result<(), AuthorizationError> {
    if signed_headers.is_empty() || !signed_headers.iter().any(|header| header == "host") {
        return Err(AuthorizationError::AccessDenied(
            "signed headers must include host".to_owned(),
        ));
    }
    let mut sorted = signed_headers.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted != signed_headers {
        return Err(AuthorizationError::AccessDenied(
            "signed headers must be sorted and unique".to_owned(),
        ));
    }
    for name in signed_headers {
        if name.bytes().any(|byte| byte.is_ascii_uppercase()) || !headers.contains_key(name) {
            return Err(AuthorizationError::AccessDenied(format!(
                "signed header {name} is invalid or missing"
            )));
        }
    }
    Ok(())
}

fn validate_session_token(
    headers: &HeaderMap,
    credential: &ResolvedCredential,
) -> Result<(), AuthorizationError> {
    let supplied = headers
        .get(AMZ_SECURITY_TOKEN_HEADER)
        .map(|value| header_text(value, AMZ_SECURITY_TOKEN_HEADER))
        .transpose()?;
    if supplied != credential.session_token.as_deref() {
        return Err(AuthorizationError::AccessDenied(
            "session token does not match the configured credential".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct AuthorizationHeader {
    access_key_id: String,
    date: String,
    region: String,
    service: String,
    terminator: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization(value: &str) -> Result<AuthorizationHeader, AuthorizationError> {
    let parameters = value
        .strip_prefix(&format!("{ALGORITHM} "))
        .ok_or_else(|| {
            AuthorizationError::AccessDenied(
                "authorization algorithm is not AWS4-HMAC-SHA256".to_owned(),
            )
        })?;
    let mut fields = HashMap::new();
    for parameter in parameters.split(',') {
        let (name, value) = parameter.trim().split_once('=').ok_or_else(|| {
            AuthorizationError::AccessDenied("authorization parameter is malformed".to_owned())
        })?;
        if fields.insert(name, value).is_some() {
            return Err(AuthorizationError::AccessDenied(format!(
                "authorization parameter {name} is duplicated"
            )));
        }
    }
    let credential = fields.get("Credential").ok_or_else(|| {
        AuthorizationError::AccessDenied("authorization has no Credential".to_owned())
    })?;
    let credential = credential.split('/').collect::<Vec<_>>();
    if credential.len() != 5 || credential.iter().any(|part| part.is_empty()) {
        return Err(AuthorizationError::AccessDenied(
            "credential scope is malformed".to_owned(),
        ));
    }
    let signed_headers = fields
        .get("SignedHeaders")
        .ok_or_else(|| {
            AuthorizationError::AccessDenied("authorization has no SignedHeaders".to_owned())
        })?
        .split(';')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let signature = fields
        .get("Signature")
        .filter(|signature| signature.len() == 64)
        .ok_or_else(|| {
            AuthorizationError::AccessDenied("authorization has an invalid Signature".to_owned())
        })?
        .to_string();
    if !signature.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AuthorizationError::AccessDenied(
            "authorization signature is not hexadecimal".to_owned(),
        ));
    }
    Ok(AuthorizationHeader {
        access_key_id: credential[0].to_owned(),
        date: credential[1].to_owned(),
        region: credential[2].to_owned(),
        service: credential[3].to_owned(),
        terminator: credential[4].to_owned(),
        signed_headers,
        signature,
    })
}

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, AuthorizationError> {
    headers
        .get(name)
        .ok_or_else(|| {
            AuthorizationError::AccessDenied(format!("required header {name} is missing"))
        })
        .and_then(|value| header_text(value, name))
}

fn header_text<'a>(
    value: &'a axum::http::HeaderValue,
    name: &str,
) -> Result<&'a str, AuthorizationError> {
    value
        .to_str()
        .map_err(|_| AuthorizationError::AccessDenied(format!("header {name} is not valid text")))
}

fn normalize_header_value(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_amz_date(value: &str) -> Result<DateTime<Utc>, AuthorizationError> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").map_err(|_| {
        AuthorizationError::AccessDenied("x-amz-date is not a SigV4 timestamp".to_owned())
    })?;
    Ok(DateTime::from_naive_utc_and_offset(naive, Utc))
}

fn sha256_hex(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, service.as_bytes());
    hmac_sha256(&service_key, TERMINATOR.as_bytes())
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    hmac.update(value);
    hmac.finalize().into_bytes().to_vec()
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index, mut star, mut checkpoint) = (0, 0, None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            checkpoint = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            checkpoint += 1;
            value_index = checkpoint;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

#[derive(Debug, Error)]
pub(crate) enum AuthorizationError {
    #[error("access denied: {0}")]
    AccessDenied(String),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{
        RuntimeIamAction, conditions_match, evaluate_action, local_identity, statement_matches,
        wildcard_match,
    };
    use crate::catalog::{
        AuthorizationPolicy, DEFAULT_ACCOUNT_ID, DEFAULT_REGION, PolicyEffect, PolicyStatement,
    };

    #[test]
    fn local_identity_uses_floci_style_sigv4_context() {
        let unsigned = local_identity(&HeaderMap::new()).expect("unsigned identity");
        assert_eq!(unsigned.region, DEFAULT_REGION);
        assert_eq!(unsigned.account_id, DEFAULT_ACCOUNT_ID);

        let authorization = |access_key: &str, region: &str| {
            HeaderValue::from_str(&format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/20260819/{region}/bedrock-agentcore/aws4_request, SignedHeaders=host;x-amz-date, Signature={}",
                "0".repeat(64)
            ))
            .expect("authorization header")
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            authorization("123456789012", "eu-west-1"),
        );
        let multi_account = local_identity(&headers).expect("multi-account identity");
        assert_eq!(multi_account.region, "eu-west-1");
        assert_eq!(multi_account.account_id, "123456789012");

        headers.insert(
            header::AUTHORIZATION,
            authorization("test", "ap-southeast-2"),
        );
        let default_account = local_identity(&headers).expect("default account identity");
        assert_eq!(default_account.region, "ap-southeast-2");
        assert_eq!(default_account.account_id, DEFAULT_ACCOUNT_ID);
    }

    #[test]
    fn wildcard_matching_is_bounded_and_predictable() {
        assert!(wildcard_match(
            "bedrock-agentcore:Invoke*",
            "bedrock-agentcore:InvokeAgentRuntime"
        ));
        assert!(wildcard_match(
            "arn:aws:*:runtime/*",
            "arn:aws:bedrock-agentcore:runtime/demo"
        ));
        assert!(!wildcard_match(
            "bedrock-agentcore:Get*",
            "bedrock-agentcore:InvokeAgentRuntime"
        ));
    }

    #[test]
    fn statement_matching_requires_action_resource_principal_and_conditions() {
        let statement = PolicyStatement {
            effect: PolicyEffect::Allow,
            actions: vec!["bedrock-agentcore:Invoke*".to_owned()],
            resources: vec!["arn:aws:bedrock-agentcore:*:*:runtime/*".to_owned()],
            principals: vec!["arn:aws:iam::*:role/local-*".to_owned()],
            conditions: HashMap::from([(
                "StringEquals".to_owned(),
                HashMap::from([(
                    "bedrock-agentcore:Qualifier".to_owned(),
                    vec!["DEFAULT".to_owned()],
                )]),
            )]),
        };
        let context = HashMap::from([("bedrock-agentcore:Qualifier", "DEFAULT")]);
        assert!(statement_matches(
            &statement,
            RuntimeIamAction::InvokeAgentRuntime,
            "arn:aws:bedrock-agentcore:us-west-2:000000000000:runtime/flint_local",
            Some("arn:aws:iam::000000000000:role/local-runtime"),
            &context,
        ));
        assert!(conditions_match(&statement, &context));
    }

    #[test]
    fn explicit_deny_overrides_matching_allow() {
        let statement = |effect, actions: Vec<String>| PolicyStatement {
            effect,
            actions,
            resources: vec!["*".to_owned()],
            principals: Vec::new(),
            conditions: HashMap::new(),
        };
        let policy = AuthorizationPolicy {
            identity_statements: vec![
                statement(PolicyEffect::Allow, vec!["bedrock-agentcore:*".to_owned()]),
                statement(
                    PolicyEffect::Deny,
                    vec!["bedrock-agentcore:StopRuntimeSession".to_owned()],
                ),
            ],
            resource_statements: Vec::new(),
        };
        let error = evaluate_action(
            &policy,
            RuntimeIamAction::StopRuntimeSession,
            "arn:aws:bedrock-agentcore:us-west-2:000000000000:runtime/flint_local",
            "arn:aws:iam::000000000000:role/local-runtime",
            &HashMap::new(),
        )
        .expect_err("explicit deny wins");
        assert!(error.to_string().contains("explicit policy deny"));
    }
}
