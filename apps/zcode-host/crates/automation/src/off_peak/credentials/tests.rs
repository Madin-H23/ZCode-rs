//! 闲时凭据解析的单元测试；带真实账号栈的端到端在 crates/services/tests/suite/off_peak_account.rs。
use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::off_peak::models::request_auth;

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn selected(access: Value) -> SelectedProvider {
    SelectedProvider {
        provider_id: "account:p".into(),
        access,
        base_url: None,
    }
}

#[tokio::test]
async fn mock_and_missing_account() {
    let mock = CredentialSource::new(vars(&[("ZCODE_OFFPEAK_MOCK", "1")]), None);
    let credentials = mock.resolve().await.unwrap();
    assert_eq!(credentials.jwt, "offpeak-mock-jwt");
    assert_eq!(credentials.kind, "bigmodel-personal");
    assert_eq!(
        mock.support().await.unwrap(),
        json!({"supported": true, "kind": "bigmodel-personal", "providerFamily": "bigmodel",
               "providerId": "account:bigmodel-individual-coding-plan"})
    );
    let no_plan = CredentialSource::new(
        vars(&[
            ("ZCODE_OFFPEAK_MOCK", "1"),
            ("ZCODE_OFFPEAK_MOCK_NO_PLAN", "1"),
        ]),
        None,
    );
    assert_eq!(
        no_plan.support().await.unwrap(),
        json!({"supported": false, "reason": "connection_unavailable"})
    );
    let real = CredentialSource::new(BTreeMap::new(), None);
    assert_eq!(
        real.resolve().await.unwrap_err(),
        Unavailable::CodingPlan("connection_unavailable")
    );
}

#[test]
fn selection_follows_the_support_matrix() {
    let kind =
        |access: Value| selection_of(Some(&selected(access))).map(|selection| selection.kind);
    assert_eq!(
        kind(
            json!({"type": "zhipu-account", "family": "zai", "planKind": "individual-coding-plan"})
        ),
        Ok("zai-personal")
    );
    assert_eq!(
        kind(
            json!({"type": "zhipu-account", "family": "bigmodel", "planKind": "team-coding-plan"})
        ),
        Ok("bigmodel-team")
    );
    assert_eq!(
        kind(json!({"type": "zhipu-account", "family": "zai", "planKind": "start-plan"})),
        Err(Unavailable::CodingPlan("start_plan_not_supported"))
    );
    assert_eq!(
        selection_of(None).map(|selection| selection.kind),
        Err(Unavailable::CodingPlan("connection_unavailable"))
    );
    assert!(coding_plan_access(
        &json!({"type": "zhipu-account", "accountType": "zai", "mode": "team-coding-plan", "entitled": true})
    ));
    assert!(!coding_plan_access(
        &json!({"type": "zhipu-account", "accountType": "zai", "mode": "off-peak", "entitled": true})
    ));
    assert!(!coding_plan_access(
        &json!({"type": "zhipu-account", "accountType": "zai", "mode": "team-coding-plan", "entitled": true, "x": 1})
    ));
}

#[test]
fn errors_map_to_node_messages_and_support_reasons() {
    let jwt = Unavailable::Credentials(Missing::Jwt);
    assert_eq!(
        jwt.to_string(),
        "off-peak requires zcode login (jwt missing)"
    );
    assert_eq!(jwt.support_reason(), Some("jwt_missing"));
    let key = Unavailable::Credentials(Missing::CodingPlanApiKey);
    assert_eq!(
        key.to_string(),
        "off-peak requires a coding plan provider api key"
    );
    assert_eq!(key.support_reason(), Some("connection_unavailable"));
    let mismatch = Unavailable::CodingPlan("provider_identity_mismatch");
    assert_eq!(
        mismatch.to_string(),
        "off-peak selected coding plan unavailable: provider_identity_mismatch"
    );
    assert!(mismatch.permanent());
    let failed = Unavailable::Failed("disk".into());
    assert_eq!(failed.support_reason(), None);
    assert!(!failed.permanent());
}

#[test]
fn team_identity_headers_need_both_ids() {
    let mut credentials = Credentials {
        jwt: "j".into(),
        coding_plan_api_key: "k".into(),
        kind: "bigmodel-team",
        provider_family: "bigmodel",
        provider_id: "account:bigmodel-team-coding-plan".into(),
        organization_id: Some(" org ".into()),
        project_id: Some("proj".into()),
        provider_base_url: None,
    };
    assert_eq!(
        request_auth(&credentials, "t1"),
        json!({"apiKey": "j", "headers": {
            "Authorization": "Bearer j",
            "X-Coding-Plan-Api-Key": "k",
            "X-Off-Peak-Ticket-ID": "t1",
            "bigmodel-organization": "org",
            "bigmodel-project": "proj",
        }})
    );
    credentials.project_id = None;
    assert!(plan_identity_headers(&credentials).is_empty());
    credentials.project_id = Some("proj".into());
    credentials.kind = "zai-team";
    assert!(plan_identity_headers(&credentials).is_empty());
}
